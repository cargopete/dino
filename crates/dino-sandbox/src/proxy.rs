//! The session's HTTPS proxy, which lets a sandboxed process reach the hosts
//! named in `--allow-net` and nothing else. RFC-0021.
//!
//! Neither sandbox can name a host, so the sandbox lets processes reach this
//! proxy and the proxy decides by name. It carries `CONNECT` only, resolves the
//! name itself, and will not connect to a local or private address, so a DNS
//! answer cannot point a listed name back inside the machine.

use std::io;
use std::io::Read;
use std::io::Write;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::net::ToSocketAddrs as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::SandboxError;

/// The longest request head read before giving up on a client.
const MAX_HEAD: usize = 8 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One entry of `--allow-net`: `crates.io`, `*.npmjs.org` or `host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedHost {
    /// Lower case, without the `*.` of a suffix.
    name: String,
    suffix: bool,
    port: u16,
}

impl AllowedHost {
    /// # Errors
    ///
    /// Returns [`SandboxError::Profile`] for an entry that is not a host name,
    /// such as a URL or a bare `*`.
    pub fn parse(entry: &str) -> Result<Self, SandboxError> {
        let refuse = |why: &str| {
            SandboxError::Profile(format!(
                "--allow-net {entry:?} {why}; give a host such as crates.io, *.npmjs.org or \
                 example.com:8443"
            ))
        };
        let entry = entry.trim();
        if entry.contains("://") || entry.contains('/') {
            return Err(refuse("is a URL, not a host"));
        }
        let (host, port) = match entry.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or_else(|| refuse("has a port that is not a number"))?,
            ),
            None => (entry, 443),
        };
        let (name, suffix) = match host.strip_prefix("*.") {
            Some(rest) => (rest, true),
            None => (host, false),
        };
        let valid = !name.is_empty()
            && name.contains('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
            && !name.starts_with('.')
            && !name.ends_with('.');
        if !valid {
            return Err(refuse("is not a host name"));
        }
        Ok(Self {
            name: name.to_ascii_lowercase(),
            suffix,
            port,
        })
    }

    fn admits(&self, host: &str, port: u16) -> bool {
        if port != self.port {
            return false;
        }
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if self.suffix {
            host.strip_suffix(&self.name)
                .is_some_and(|rest| rest.len() > 1 && rest.ends_with('.'))
        } else {
            host == self.name
        }
    }
}

impl std::fmt::Display for AllowedHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let star = if self.suffix { "*." } else { "" };
        if self.port == 443 {
            write!(formatter, "{star}{}", self.name)
        } else {
            write!(formatter, "{star}{}:{}", self.name, self.port)
        }
    }
}

#[derive(Debug)]
struct Rules {
    hosts: Vec<AllowedHost>,
    /// Only for tests, which have no public host to reach.
    local_allowed: bool,
}

/// A running proxy. Its listeners live as long as the process; the socket file
/// is removed when this is dropped.
#[derive(Debug)]
pub struct Proxy {
    port: u16,
    socket: Option<PathBuf>,
    hosts: Vec<AllowedHost>,
}

impl Proxy {
    /// Listens on 127.0.0.1 and, given `socket`, on that Unix socket too, for
    /// a process in a network namespace of its own to reach through a relay.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Profile`] when a listener cannot be opened.
    pub fn start(
        hosts: Vec<AllowedHost>,
        socket: Option<&Path>,
    ) -> Result<Arc<Self>, SandboxError> {
        Self::start_with(
            Rules {
                hosts,
                local_allowed: false,
            },
            socket,
        )
    }

    fn start_with(rules: Rules, socket: Option<&Path>) -> Result<Arc<Self>, SandboxError> {
        let failed = |what: &str, error: io::Error| {
            SandboxError::Profile(format!(
                "cannot start the --allow-net proxy ({what}): {error}"
            ))
        };
        let tcp = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| failed("TCP", error))?;
        let port = tcp
            .local_addr()
            .map_err(|error| failed("TCP", error))?
            .port();
        let hosts = rules.hosts.clone();
        let rules = Arc::new(rules);
        let accepting = Arc::clone(&rules);
        std::thread::spawn(move || {
            for stream in tcp.incoming().flatten() {
                let rules = Arc::clone(&accepting);
                std::thread::spawn(move || serve(stream, &rules));
            }
        });
        #[cfg(unix)]
        if let Some(path) = socket {
            let _ = std::fs::remove_file(path);
            let unix = std::os::unix::net::UnixListener::bind(path)
                .map_err(|error| failed(&path.display().to_string(), error))?;
            std::thread::spawn(move || {
                for stream in unix.incoming().flatten() {
                    let rules = Arc::clone(&rules);
                    std::thread::spawn(move || serve(stream, &rules));
                }
            });
        }
        Ok(Arc::new(Self {
            port,
            socket: socket.map(Path::to_path_buf),
            hosts,
        }))
    }

    /// The TCP port on 127.0.0.1, for `HTTPS_PROXY`.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    #[must_use]
    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    #[must_use]
    pub fn hosts(&self) -> &[AllowedHost] {
        &self.hosts
    }
}

/// The sandbox label's network clause.
pub(crate) fn describe(proxy: &Proxy) -> String {
    let hosts: Vec<String> = proxy.hosts.iter().map(ToString::to_string).collect();
    format!(
        "network: HTTPS to {} through dino's proxy",
        hosts.join(", ")
    )
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(socket) = &self.socket {
            let _ = std::fs::remove_file(socket);
        }
    }
}

/// A stream that can be split into a reading and a writing half.
trait Duplex: Read + Write + Send + Sized + 'static {
    fn split(&self) -> io::Result<Self>;
    fn close_write(&self);
}

impl Duplex for TcpStream {
    fn split(&self) -> io::Result<Self> {
        self.try_clone()
    }

    fn close_write(&self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

#[cfg(unix)]
impl Duplex for std::os::unix::net::UnixStream {
    fn split(&self) -> io::Result<Self> {
        self.try_clone()
    }

    fn close_write(&self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

fn serve<S: Duplex>(mut client: S, rules: &Rules) {
    let Ok((head, early)) = read_head(&mut client) else {
        return;
    };
    let upstream = match decide(&head, rules) {
        Ok(upstream) => upstream,
        Err((status, why)) => {
            let _ = write!(
                client,
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{why}",
                why.len()
            );
            return;
        }
    };
    if client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .is_err()
    {
        return;
    }
    let mut upstream = upstream;
    if !early.is_empty() && upstream.write_all(&early).is_err() {
        return;
    }
    tunnel(client, upstream);
}

/// Reads up to the end of the request head, and returns whatever arrived after it.
fn read_head<S: Read>(client: &mut S) -> io::Result<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = client.read(&mut chunk)?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let early = buffer.split_off(end + 4);
            return Ok((String::from_utf8_lossy(&buffer).into_owned(), early));
        }
        if buffer.len() > MAX_HEAD {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
}

type Refusal = (&'static str, String);

fn decide(head: &str, rules: &Rules) -> Result<TcpStream, Refusal> {
    let line = head.lines().next().unwrap_or_default();
    let mut words = line.split_whitespace();
    let (method, target) = (
        words.next().unwrap_or_default(),
        words.next().unwrap_or_default(),
    );
    if !method.eq_ignore_ascii_case("CONNECT") {
        return Err((
            "405 Method Not Allowed",
            "dino's --allow-net proxy carries HTTPS only (CONNECT)\n".to_owned(),
        ));
    }
    let (host, port) = target
        .rsplit_once(':')
        .and_then(|(host, port)| Some((host.trim_matches(['[', ']']), port.parse::<u16>().ok()?)))
        .ok_or_else(|| ("400 Bad Request", format!("dino: cannot read {target:?}\n")))?;
    if !rules.hosts.iter().any(|allowed| allowed.admits(host, port)) {
        return Err((
            "403 Forbidden",
            format!("dino: {host}:{port} is not in --allow-net\n"),
        ));
    }
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| {
            (
                "502 Bad Gateway",
                format!("dino: cannot resolve {host}: {error}\n"),
            )
        })?
        .filter(|address| rules.local_allowed || is_public(address.ip()))
        .collect();
    if addresses.is_empty() {
        return Err((
            "403 Forbidden",
            format!("dino: {host} resolves only to a local or private address\n"),
        ));
    }
    let mut last = None;
    for address in &addresses {
        match TcpStream::connect_timeout(address, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err((
        "502 Bad Gateway",
        format!(
            "dino: cannot connect to {host}:{port}: {}\n",
            last.map_or_else(String::new, |error| error.to_string())
        ),
    ))
}

/// Whether an address is on the public internet, rather than this machine, a
/// private network, a tailnet or link-local.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

fn tunnel<S: Duplex>(client: S, upstream: TcpStream) {
    let (Ok(mut client_read), Ok(mut upstream_write)) = (client.split(), upstream.try_clone())
    else {
        return;
    };
    let forward = std::thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(std::net::Shutdown::Write);
    });
    let (mut upstream_read, mut client_write) = (upstream, client);
    let _ = io::copy(&mut upstream_read, &mut client_write);
    client_write.close_write();
    let _ = forward.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(entry: &str) -> AllowedHost {
        AllowedHost::parse(entry).unwrap()
    }

    #[test]
    fn an_entry_is_a_name_a_suffix_or_a_name_and_port() {
        assert!(host("crates.io").admits("crates.io", 443));
        assert!(host("crates.io").admits("Crates.IO.", 443));
        assert!(!host("crates.io").admits("static.crates.io", 443));
        assert!(!host("crates.io").admits("crates.io", 80));
        assert!(host("*.npmjs.org").admits("registry.npmjs.org", 443));
        assert!(!host("*.npmjs.org").admits("npmjs.org", 443));
        assert!(!host("*.npmjs.org").admits("evilnpmjs.org", 443));
        assert!(host("example.com:8443").admits("example.com", 8443));
        assert_eq!(host("*.npmjs.org").to_string(), "*.npmjs.org");
        for bad in [
            "https://crates.io",
            "crates.io/x",
            "*",
            "*.",
            "localhost",
            "a.b:0",
            "",
        ] {
            assert!(AllowedHost::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_public_addresses_are_reached() {
        for local in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.14",
            "100.100.1.1",
            "169.254.1.1",
            "::1",
            "fd00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(local.parse().unwrap()), "{local}");
        }
        for public in ["1.1.1.1", "151.101.1.1", "2606:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }

    /// An echo server on loopback, and a proxy that may reach it as
    /// `localhost`, which `parse` would refuse for having no dot.
    fn proxied() -> (Arc<Proxy>, u16) {
        let echo = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = echo.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in echo.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut buffer = [0_u8; 64];
                    if let Ok(read) = stream.read(&mut buffer) {
                        let _ = stream.write_all(&buffer[..read]);
                    }
                });
            }
        });
        let rules = Rules {
            hosts: vec![AllowedHost {
                name: "localhost".to_owned(),
                suffix: false,
                port,
            }],
            local_allowed: true,
        };
        (Proxy::start_with(rules, None).unwrap(), port)
    }

    fn ask(proxy: &Proxy, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", proxy.port())).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = Vec::new();
        let mut chunk = [0_u8; 256];
        while !String::from_utf8_lossy(&reply).contains("\r\n\r\n") {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            reply.extend_from_slice(&chunk[..read]);
        }
        if String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 200") {
            stream.write_all(b"ping").unwrap();
            let read = stream.read(&mut chunk).unwrap();
            reply.extend_from_slice(&chunk[..read]);
        } else {
            let _ = stream.read_to_end(&mut reply);
        }
        String::from_utf8_lossy(&reply).into_owned()
    }

    #[test]
    fn a_listed_host_is_tunnelled_and_its_address_is_not_a_way_round() {
        let (proxy, port) = proxied();
        let reply = ask(
            &proxy,
            &format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n"),
        );
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.ends_with("ping"), "{reply}");
        let reply = ask(
            &proxy,
            &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n"),
        );
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    }

    #[test]
    fn a_listed_name_that_resolves_locally_is_refused() {
        let (_, port) = proxied();
        let rules = Rules {
            hosts: vec![AllowedHost {
                name: "localhost".to_owned(),
                suffix: false,
                port,
            }],
            local_allowed: false,
        };
        let proxy = Proxy::start_with(rules, None).unwrap();
        let reply = ask(
            &proxy,
            &format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n"),
        );
        assert!(
            reply.contains("resolves only to a local or private address"),
            "{reply}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_unix_socket_is_the_same_proxy() {
        let path =
            std::env::temp_dir().join(format!("dino-proxy-test-{}.sock", std::process::id()));
        let proxy = Proxy::start(vec![host("crates.io")], Some(&path)).unwrap();
        let mut stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
        stream
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        drop(proxy);
        assert!(!path.exists(), "the socket is removed with the proxy");
    }

    #[test]
    fn an_unlisted_host_and_plain_http_are_refused() {
        let (proxy, _) = proxied();
        let reply = ask(&proxy, "CONNECT example.com:443 HTTP/1.1\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert!(
            reply.contains("example.com:443 is not in --allow-net"),
            "{reply}"
        );
        let reply = ask(&proxy, "GET http://crates.io/ HTTP/1.1\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 405"), "{reply}");
    }
}
