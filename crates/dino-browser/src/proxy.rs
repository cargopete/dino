//! A forward proxy every request of a guarded browser goes through, so a page
//! cannot reach an address the caller has not allowed: loopback, the LAN, a
//! tailnet. Each connection is resolved here, checked, and pinned to the
//! addresses checked, so a second DNS answer cannot swap in another.

use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use crate::Error;

const HEAD_LIMIT: usize = 64 * 1024;
const CONNECT_LIMIT: Duration = Duration::from_secs(10);

pub type Allow = Arc<dyn Fn(IpAddr) -> bool + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct Guard {
    pub addr: SocketAddr,
}

impl Guard {
    /// Listens on a free loopback port; `allow` decides each resolved address.
    ///
    /// # Errors
    ///
    /// Fails when no port can be bound.
    pub async fn start(allow: Allow) -> Result<Self, Error> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let allow = allow.clone();
                tokio::spawn(async move {
                    let _ = serve(client, &allow).await;
                });
            }
        });
        Ok(Self { addr })
    }
}

async fn read_head(client: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = client.read(&mut byte).await?;
        if n == 0 || head.len() > HEAD_LIMIT {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        head.extend_from_slice(&byte[..n]);
    }
    Ok(head)
}

/// Resolves `host:port` and connects to the first address that answers,
/// provided every address it resolves to is allowed.
async fn dial(host: &str, port: u16, allow: &Allow) -> Result<TcpStream, String> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| e.to_string())?
        .collect();
    if addrs.is_empty() || !addrs.iter().all(|addr| allow(addr.ip())) {
        return Err(format!("{host} is not a public address"));
    }
    let mut last = String::from("no address answered");
    for addr in addrs {
        match tokio::time::timeout(CONNECT_LIMIT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => last = error.to_string(),
            Err(_) => "connect timed out".clone_into(&mut last),
        }
    }
    Err(last)
}

fn split_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, port.parse().ok()?),
        _ => (authority, default_port),
    };
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

async fn serve(mut client: TcpStream, allow: &Allow) -> std::io::Result<()> {
    let head = read_head(&mut client).await?;
    let end = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(head.len(), |at| at + 4);
    let text = String::from_utf8_lossy(&head[..end]).into_owned();
    let mut first = text.lines().next().unwrap_or_default().split_whitespace();
    let (method, target) = (
        first.next().unwrap_or_default(),
        first.next().unwrap_or_default(),
    );

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = split_authority(target, 443) else {
            return refuse(&mut client, "bad target").await;
        };
        let mut upstream = match dial(&host, port, allow).await {
            Ok(upstream) => upstream,
            Err(why) => return refuse(&mut client, &why).await,
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        if end < head.len() {
            upstream.write_all(&head[end..]).await?;
        }
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        return Ok(());
    }

    // Plain http in absolute form. The request is sent with Connection: close, so
    // a kept-alive proxy connection cannot carry a second request to another host.
    let Some(rest) = target.strip_prefix("http://") else {
        return refuse(&mut client, "only http and CONNECT").await;
    };
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_owned()), |(a, p)| (a, format!("/{p}")));
    let Some((host, port)) = split_authority(authority, 80) else {
        return refuse(&mut client, "bad target").await;
    };
    let mut upstream = match dial(&host, port, allow).await {
        Ok(upstream) => upstream,
        Err(why) => return refuse(&mut client, &why).await,
    };
    let mut forwarded = format!("{method} {path} HTTP/1.1\r\n");
    for line in text.lines().skip(1).take_while(|line| !line.is_empty()) {
        let name = line
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            "connection" | "proxy-connection" | "keep-alive" | "proxy-authorization"
        ) {
            forwarded.push_str(line);
            forwarded.push_str("\r\n");
        }
    }
    forwarded.push_str("Connection: close\r\n\r\n");
    upstream.write_all(forwarded.as_bytes()).await?;
    upstream.write_all(&head[end..]).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn refuse(client: &mut TcpStream, why: &str) -> std::io::Result<()> {
    let body = format!("refused by the guard: {why}");
    let reply = format!(
        "HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(reply.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorities_split_with_their_default_port() {
        assert_eq!(split_authority("bnb.bg", 443), Some(("bnb.bg".into(), 443)));
        assert_eq!(
            split_authority("bnb.bg:8443", 443),
            Some(("bnb.bg".into(), 8443))
        );
        assert_eq!(split_authority("[::1]:80", 443), Some(("[::1]".into(), 80)));
        assert_eq!(split_authority("[::1]", 443), Some(("[::1]".into(), 443)));
    }

    #[tokio::test]
    async fn a_refused_address_is_never_dialled() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = target.local_addr().unwrap().port();
        let guard = Guard::start(Arc::new(|ip: IpAddr| !ip.is_loopback()))
            .await
            .unwrap();
        let mut client = TcpStream::connect(guard.addr).await.unwrap();
        client
            .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        let accepted = tokio::time::timeout(Duration::from_millis(200), target.accept()).await;
        assert!(
            accepted.is_err(),
            "the guard connected to a refused address"
        );
    }

    #[tokio::test]
    async fn an_allowed_address_is_tunnelled() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = target.accept().await.unwrap();
            stream.write_all(b"hello").await.unwrap();
        });
        let guard = Guard::start(Arc::new(|_| true)).await.unwrap();
        let mut client = TcpStream::connect(guard.addr).await.unwrap();
        client
            .write_all(format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        assert!(
            reply.starts_with("HTTP/1.1 200") && reply.ends_with("hello"),
            "{reply}"
        );
    }
}
