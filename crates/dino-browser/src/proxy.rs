//! A forward proxy every request of a guarded browser goes through, so a page
//! cannot reach an address the caller has not allowed: loopback, the LAN, a
//! tailnet. Each connection is resolved here, checked, and pinned to the
//! addresses checked, so a second DNS answer cannot swap in another.

use std::net::IpAddr;
use std::net::Ipv4Addr;
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
/// As [`Allow`], told the port too.
pub type Admit = Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>;

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
        Self::start_admitting(Arc::new(move |addr: SocketAddr| allow(addr.ip()))).await
    }

    /// As [`Self::start`], deciding each address with its port.
    ///
    /// # Errors
    ///
    /// Fails when no port can be bound.
    pub async fn start_admitting(allow: Admit) -> Result<Self, Error> {
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
async fn dial(host: &str, port: u16, allow: &Admit) -> Result<TcpStream, Refusal> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| Refusal::Unreachable(e.to_string()))?
        .collect();
    if addrs.is_empty() || !addrs.iter().all(|addr| allow(*addr)) {
        return Err(Refusal::NotPublic(format!(
            "{host} is not a public address"
        )));
    }
    let mut last = String::from("no address answered");
    for addr in addrs {
        match tokio::time::timeout(CONNECT_LIMIT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => last = error.to_string(),
            Err(_) => "connect timed out".clone_into(&mut last),
        }
    }
    Err(Refusal::Unreachable(last))
}

enum Refusal {
    NotPublic(String),
    Unreachable(String),
}

fn split_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, port.parse().ok()?),
        _ => (authority, default_port),
    };
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

async fn serve(mut client: TcpStream, allow: &Admit) -> std::io::Result<()> {
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
            return refuse(&mut client, &Refusal::Unreachable("bad target".into())).await;
        };
        let mut upstream = match dial(&host, port, allow).await {
            Ok(upstream) => upstream,
            Err(refusal) => return refuse(&mut client, &refusal).await,
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
        return refuse(
            &mut client,
            &Refusal::Unreachable("only http and CONNECT".into()),
        )
        .await;
    };
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_owned()), |(a, p)| (a, format!("/{p}")));
    let Some((host, port)) = split_authority(authority, 80) else {
        return refuse(&mut client, &Refusal::Unreachable("bad target".into())).await;
    };
    let mut upstream = match dial(&host, port, allow).await {
        Ok(upstream) => upstream,
        Err(refusal) => return refuse(&mut client, &refusal).await,
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

/// On every reply the guard makes itself, so it is not taken for the site's: `not-public` or
/// `unreachable`.
pub const GUARD_HEADER: &str = "x-dino-guard";

/// Public addresses, and the loopback or LAN ones `seeds` resolve to, each only at its own
/// port: a dev server that was typed is reached, and nothing else on this machine or network.
/// Link-local stays refused even when typed, since it is where cloud credentials are served.
pub async fn public_and(seeds: &[String]) -> Admit {
    let mut kept = std::collections::HashSet::new();
    for seed in seeds {
        let Ok(url) = url::Url::parse(seed) else {
            continue;
        };
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            continue;
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(addrs) = tokio::net::lookup_host((host, port)).await {
            kept.extend(addrs.filter(|addr| is_local(addr.ip())));
        }
    }
    Arc::new(move |addr: SocketAddr| is_public(addr.ip()) || kept.contains(&addr))
}

/// Whether an address is on the public internet, rather than this machine, a
/// private network, a tailnet, link-local, or a range set aside.
#[must_use]
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                // 64:ff9b::/96 reaches IPv4 through a translator, possibly a private address.
                || (s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|&x| x == 0)))
        }
    }
}

fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b))
        || a >= 240)
}

async fn refuse(client: &mut TcpStream, refusal: &Refusal) -> std::io::Result<()> {
    let (status, marked, body) = match refusal {
        Refusal::NotPublic(why) => (
            "403 Forbidden",
            "not-public",
            format!("refused by the guard: {why}"),
        ),
        Refusal::Unreachable(why) => ("502 Bad Gateway", "unreachable", why.clone()),
    };
    let reply = format!(
        "HTTP/1.1 {status}\r\n{GUARD_HEADER}: {marked}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(reply.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses_are_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.1.1",
            "192.0.2.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "93.184.215.14", "2606:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn a_typed_seed_is_reached_at_its_own_port_only() {
        let admit = public_and(&["http://127.0.0.1:8765/".into()]).await;
        assert!(admit("127.0.0.1:8765".parse().unwrap()));
        assert!(!admit("127.0.0.1:8766".parse().unwrap()));
        assert!(!admit("169.254.169.254:80".parse().unwrap()));
        assert!(admit("1.1.1.1:443".parse().unwrap()));
        let typed = public_and(&["http://169.254.169.254/".into()]).await;
        assert!(!typed("169.254.169.254:80".parse().unwrap()), "even typed");
    }

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
