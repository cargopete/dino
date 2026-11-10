//! The network half of remote control: who may connect and what they may ask.
//!
//! Plain HTTP, because it is meant to be reached over a tunnel that already
//! encrypts and authenticates the machine: Tailscale, or `ssh -L`. Which
//! addresses it will listen on is therefore a rule rather than a default, and
//! every request that does anything carries a bearer token.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::Read as _;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use bytes::Bytes;
use futures_util::stream;
use http_body_util::BodyExt;
use http_body_util::Full;
use http_body_util::Limited;
use http_body_util::StreamBody;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::HeaderMap;
use hyper::Method;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::body::Frame as BodyFrame;
use hyper::body::Incoming;
use hyper::header::AUTHORIZATION;
use hyper::header::CACHE_CONTROL;
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::rt::TokioTimer;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use super::AnswerError;
use super::Frame;
use super::Hub;
use super::MoveError;
use super::PageAnswer;
use super::ReplyError;
use super::SubmitError;
use super::Submitted;
use super::Subscription;

type Body = UnsyncBoxBody<Bytes, Infallible>;

const INDEX_HTML: &str = include_str!("page/index.html");
const APP_JS: &str = include_str!("page/app.js");
const APP_CSS: &str = include_str!("page/app.css");

/// Connections open at once, streams included. More are dropped unanswered.
const MAX_CONNECTIONS: usize = 32;

/// How long a client has to send its request headers.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a client has to send a request body it announced.
const BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// The most a request body may weigh. A prompt is capped well under it.
const MAX_BODY_BYTES: usize = 64 * 1_024;

/// The most a request's headers and buffered input may weigh.
const MAX_BUFFER_BYTES: usize = 16 * 1_024;

/// A stream with nothing to say sends a comment this often, so a tunnel that
/// drops idle connections does not, and a dead client is noticed.
const KEEPALIVE: Duration = Duration::from_secs(20);

/// What a refused client waits before it is told.
const REFUSAL_DELAY: Duration = Duration::from_millis(250);

/// What a browser may do with the page: load its own two files and talk to the
/// server it came from. Nothing inline, nothing from elsewhere, no framing.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Whether `ip` is an address Tailscale assigns: 100.64.0.0/10, or its IPv6
/// unique-local prefix `fd7a:115c:a1e0::/48`.
fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [first, second, ..] = ip.octets();
            first == 100 && (64..=127).contains(&second)
        }
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// The first Tailscale IPv4 address in the output of `tailscale ip -4`.
fn parse_tailscale_ip(output: &str) -> Option<IpAddr> {
    output
        .lines()
        .filter_map(|line| line.trim().parse::<IpAddr>().ok())
        .find(|ip| is_tailscale(*ip))
}

/// The macOS app puts its CLI here and not on PATH unless asked to.
const TAILSCALE_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

fn tailscale_address() -> Option<IpAddr> {
    ["tailscale", TAILSCALE_APP_CLI]
        .into_iter()
        .find_map(|cli| {
            let output = std::process::Command::new(cli)
                .args(["ip", "-4"])
                .stdin(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|output| output.status.success())?;
            parse_tailscale_ip(&String::from_utf8_lossy(&output.stdout))
        })
}

/// Where to listen.
///
/// Loopback and Tailscale addresses only, whatever was asked for. `0.0.0.0` and
/// a LAN address would put a plain-HTTP page, and a token that travels in every
/// request, on a network nothing encrypts. Without `requested`, this machine's
/// Tailscale address, or loopback when it has none.
///
/// # Errors
///
/// Returns an error when `requested` is an address this refuses.
pub fn bind_address(requested: Option<IpAddr>, port: u16) -> Result<SocketAddr> {
    let ip = match requested {
        Some(ip) => ip,
        None => tailscale_address().unwrap_or_else(|| {
            eprintln!("dino: no Tailscale address found, so listening on localhost only.");
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }),
    };
    if !(ip.is_loopback() || is_tailscale(ip)) {
        bail!(
            "{ip} is neither a loopback nor a Tailscale address. The page is plain HTTP and \
             relies on a tunnel for encryption, so it will not listen anywhere else; use \
             Tailscale, or `ssh -L` to localhost"
        );
    }
    Ok(SocketAddr::new(ip, port))
}

/// A fresh token: 256 bits from the operating system, in hex.
///
/// # Errors
///
/// Returns an error when the system's randomness cannot be read.
pub fn new_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .context("cannot read random bytes for the token")?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

/// Equality that does not stop at the first difference.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

fn authorised(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|given| constant_time_eq(given.as_bytes(), token.as_bytes()))
}

struct Shared {
    hub: Arc<Hub>,
    token: String,
}

/// Answers connections on `listener` for as long as it runs.
pub async fn serve(listener: TcpListener, hub: Arc<Hub>, token: String) -> Infallible {
    let shared = Arc::new(Shared { hub, token });
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            // Out of descriptors, usually: wait rather than spin.
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            continue;
        };
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let shared = Arc::clone(&shared);
                async move { Ok::<_, Infallible>(route(request, &shared, peer).await) }
            });
            let connection = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .max_buf_size(MAX_BUFFER_BYTES)
                .serve_connection(TokioIo::new(stream), service);
            let _ = connection.await;
            drop(slot);
        });
    }
}

fn respond(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header(CACHE_CONTROL, "no-store")
        .header("content-security-policy", CONTENT_SECURITY_POLICY)
        .header("x-content-type-options", "nosniff")
        .header("x-frame-options", "DENY")
        .header("referrer-policy", "no-referrer")
        .body(Full::new(body.into()).boxed_unsync())
        .expect("fixed headers are valid")
}

fn status(code: StatusCode) -> Response<Body> {
    respond(
        code,
        "text/plain; charset=utf-8",
        code.canonical_reason().unwrap_or("error"),
    )
}

fn json(code: StatusCode, value: &serde_json::Value) -> Response<Body> {
    respond(code, "application/json", value.to_string())
}

async fn route(request: Request<Incoming>, shared: &Shared, peer: SocketAddr) -> Response<Body> {
    let path = request.uri().path().to_owned();
    if request.method() == Method::GET {
        // The page is public: it holds no secret, and the token it needs is
        // in the address fragment, which a browser never sends.
        match path.as_str() {
            "/" | "/index.html" => {
                return respond(StatusCode::OK, "text/html; charset=utf-8", INDEX_HTML);
            }
            "/app.js" => return respond(StatusCode::OK, "text/javascript; charset=utf-8", APP_JS),
            "/app.css" => return respond(StatusCode::OK, "text/css; charset=utf-8", APP_CSS),
            _ => {}
        }
    }
    if !path.starts_with("/api/") {
        return status(StatusCode::NOT_FOUND);
    }
    if !authorised(request.headers(), &shared.token) {
        eprintln!("dino: remote: refused a request from {peer} without the right token");
        tokio::time::sleep(REFUSAL_DELAY).await;
        return status(StatusCode::UNAUTHORIZED);
    }
    let hub = &shared.hub;
    match (request.method().clone(), path.as_str()) {
        (Method::GET, "/api/events") => events(hub, after(request.uri().query())),
        (Method::POST, "/api/prompt") => prompt(request, hub).await,
        (Method::POST, "/api/cancel") => json(
            StatusCode::OK,
            &serde_json::json!({ "cancelled": hub.cancel() }),
        ),
        (Method::POST, "/api/approve") => approve(request, hub).await,
        (Method::POST, "/api/answer") => answer(request, hub).await,
        (Method::GET, "/api/sessions") => sessions(hub),
        (Method::GET, "/api/workspaces") => json(
            StatusCode::OK,
            &serde_json::json!({ "workspaces": hub.workspaces() }),
        ),
        (Method::GET, "/api/trace") => trace(hub, request.uri().query()).await,
        (Method::POST, "/api/resume") => resume(request, hub).await,
        (Method::POST, "/api/workspace") => workspace(request, hub).await,
        (
            _,
            "/api/events" | "/api/prompt" | "/api/cancel" | "/api/approve" | "/api/answer"
            | "/api/sessions" | "/api/workspaces" | "/api/trace" | "/api/resume" | "/api/workspace",
        ) => status(StatusCode::METHOD_NOT_ALLOWED),
        _ => status(StatusCode::NOT_FOUND),
    }
}

/// The `after` a page sends to say which frame it last saw.
fn after(query: Option<&str>) -> u64 {
    parameter(query, "after")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn parameter<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

fn event_chunk(frame: &Frame) -> Bytes {
    // The JSON is compact, so it has no newline to end the field early.
    Bytes::from(format!("id: {}\ndata: {}\n\n", frame.seq, frame.json))
}

fn events(hub: &Hub, after: u64) -> Response<Body> {
    let Subscription { backlog, live } = hub.subscribe(after);
    let chunks = stream::unfold((backlog, live), |(mut backlog, mut live)| async move {
        let chunk = if let Some(frame) = backlog.pop_front() {
            event_chunk(&frame)
        } else {
            match tokio::time::timeout(KEEPALIVE, live.recv()).await {
                Ok(Ok(frame)) => event_chunk(&frame),
                // Fell behind or the session ended: the page reconnects and
                // is brought up from the backlog.
                Ok(Err(_)) => return None,
                Err(_) => Bytes::from_static(b": keepalive\n\n"),
            }
        };
        Some((Ok::<_, Infallible>(BodyFrame::data(chunk)), (backlog, live)))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-store")
        .header("content-security-policy", CONTENT_SECURITY_POLICY)
        .header("x-content-type-options", "nosniff")
        .body(StreamBody::new(chunks).boxed_unsync())
        .expect("fixed headers are valid")
}

async fn read_json<T: DeserializeOwned>(request: Request<Incoming>) -> Result<T, StatusCode> {
    let read = Limited::new(request.into_body(), MAX_BODY_BYTES).collect();
    let body = tokio::time::timeout(BODY_TIMEOUT, read)
        .await
        .map_err(|_| StatusCode::REQUEST_TIMEOUT)?
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?
        .to_bytes();
    serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptBody {
    text: String,
}

async fn prompt(request: Request<Incoming>, hub: &Hub) -> Response<Body> {
    let body: PromptBody = match read_json(request).await {
        Ok(body) => body,
        Err(code) => return status(code),
    };
    match hub.submit(&body.text) {
        Ok(taken) => json(
            StatusCode::ACCEPTED,
            &serde_json::json!({ "queued": taken == Submitted::Queued }),
        ),
        Err(SubmitError::Empty) => status(StatusCode::BAD_REQUEST),
        Err(SubmitError::TooLong) => status(StatusCode::PAYLOAD_TOO_LARGE),
        Err(SubmitError::Full) => status(StatusCode::TOO_MANY_REQUESTS),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApproveBody {
    id: u64,
    choice: String,
}

async fn approve(request: Request<Incoming>, hub: &Hub) -> Response<Body> {
    let body: ApproveBody = match read_json(request).await {
        Ok(body) => body,
        Err(code) => return status(code),
    };
    match hub.answer(body.id, &body.choice) {
        Ok(()) => json(StatusCode::OK, &serde_json::json!({ "ok": true })),
        Err(AnswerError::NoSuchApproval) => status(StatusCode::NOT_FOUND),
        Err(AnswerError::NotOffered) => status(StatusCode::BAD_REQUEST),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerBody {
    id: u64,
    answers: Vec<PageAnswer>,
}

async fn answer(request: Request<Incoming>, hub: &Hub) -> Response<Body> {
    let body: AnswerBody = match read_json(request).await {
        Ok(body) => body,
        Err(code) => return status(code),
    };
    match hub.reply(body.id, &body.answers) {
        Ok(()) => json(StatusCode::OK, &serde_json::json!({ "ok": true })),
        Err(ReplyError::NoSuchQuestion) => status(StatusCode::NOT_FOUND),
        Err(ReplyError::Malformed) => status(StatusCode::BAD_REQUEST),
    }
}

fn sessions(hub: &Hub) -> Response<Body> {
    json(
        StatusCode::OK,
        &serde_json::json!({ "sessions": hub.sessions() }),
    )
}

/// A log's trajectory. Read off the runtime's threads: a long session's log
/// runs to tens of megabytes.
async fn trace(hub: &Hub, query: Option<&str>) -> Response<Body> {
    let Some(path) = parameter(query, "id").and_then(|id| hub.log_path(id)) else {
        return status(StatusCode::NOT_FOUND);
    };
    let read = tokio::task::spawn_blocking(move || {
        crate::trace::read(&path, crate::trace::CAP_BYTES)
            .and_then(|trajectory| Ok(serde_json::to_vec(&trajectory)?))
    })
    .await;
    match read {
        Ok(Ok(body)) => respond(StatusCode::OK, "application/json", body),
        _ => status(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn moved(result: Result<(), MoveError>) -> Response<Body> {
    match result {
        Ok(()) => json(StatusCode::ACCEPTED, &serde_json::json!({ "ok": true })),
        Err(MoveError::NotOffered) => status(StatusCode::NOT_FOUND),
        Err(MoveError::Busy) => status(StatusCode::CONFLICT),
        Err(MoveError::Full) => status(StatusCode::TOO_MANY_REQUESTS),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeBody {
    id: String,
}

async fn resume(request: Request<Incoming>, hub: &Hub) -> Response<Body> {
    match read_json::<ResumeBody>(request).await {
        Ok(body) => moved(hub.resume(&body.id)),
        Err(code) => status(code),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceBody {
    path: String,
}

async fn workspace(request: Request<Incoming>, hub: &Hub) -> Response<Body> {
    match read_json::<WorkspaceBody>(request).await {
        Ok(body) => moved(hub.switch(&body.path)),
        Err(code) => status(code),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use dino_core::approval::ApprovalReply;
    use dino_core::approval::Approver;
    use dino_core::approval::AskContext;
    use dino_protocol::ToolAction;
    use dino_protocol::ToolCall;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;

    use super::super::RemoteApprover;
    use super::*;

    struct Running {
        address: SocketAddr,
        token: String,
        hub: Arc<Hub>,
        prompts: mpsc::Receiver<super::super::Request>,
        task: tokio::task::JoinHandle<Infallible>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn running() -> Running {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (hub, prompts) = Hub::new();
        let token = new_token().unwrap();
        let task = tokio::spawn(serve(listener, Arc::clone(&hub), token.clone()));
        Running {
            address,
            token,
            hub,
            prompts,
            task,
        }
    }

    fn request(method: &str, path: &str, token: Option<&str>, body: &str, close: bool) -> String {
        let mut text = format!("{method} {path} HTTP/1.1\r\nHost: dino\r\n");
        if let Some(token) = token {
            let _ = write!(text, "Authorization: Bearer {token}\r\n");
        }
        if close {
            text.push_str("Connection: close\r\n");
        }
        let _ = write!(text, "Content-Length: {}\r\n\r\n{body}", body.len());
        text
    }

    /// One request, and the whole response up to the server closing.
    async fn exchange(address: SocketAddr, text: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("the server closes after a Connection: close request")
            .unwrap();
        let response = String::from_utf8_lossy(&response).into_owned();
        let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
        let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (code, head.to_ascii_lowercase(), body.to_owned())
    }

    async fn call(
        server: &Running,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (u16, String, String) {
        exchange(server.address, &request(method, path, token, body, true)).await
    }

    /// Reads from an open stream until `wanted` has arrived.
    async fn read_until(stream: &mut TcpStream, seen: &mut String, wanted: &str) {
        let mut buffer = [0u8; 4096];
        while !seen.contains(wanted) {
            let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {wanted:?} in {seen:?}"))
                .unwrap();
            assert!(read > 0, "stream ended waiting for {wanted:?} in {seen:?}");
            seen.push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
    }

    #[tokio::test]
    async fn the_page_is_public_locked_down_and_holds_no_token() {
        let server = running().await;
        for path in ["/", "/app.js", "/app.css"] {
            let (code, head, body) = call(&server, "GET", path, None, "").await;
            assert_eq!(code, 200, "{path}");
            assert!(head.contains("no-store"), "{head}");
            assert!(head.contains("default-src 'none'"), "{head}");
            assert!(head.contains("x-frame-options: deny"), "{head}");
            assert!(!body.contains(&server.token), "{path}");
        }
        let (_, _, page) = call(&server, "GET", "/", None, "").await;
        // Nothing inline for the policy to have to allow.
        assert!(page.contains(r#"<script src="/app.js">"#), "{page}");
        assert!(!page.contains("<script>"), "{page}");
    }

    #[tokio::test]
    async fn the_api_wants_the_exact_token_in_a_header() {
        let server = running().await;
        let truncated = &server.token[..server.token.len() - 1];
        let longer = format!("{}0", server.token);
        for token in [None, Some("nope"), Some(truncated), Some(longer.as_str())] {
            let (code, _, _) = call(&server, "POST", "/api/cancel", token, "{}").await;
            assert_eq!(code, 401, "{token:?}");
        }
        // Not in the address, where it would reach logs and referrers.
        let path = format!("/api/events?token={}", server.token);
        assert_eq!(call(&server, "GET", &path, None, "").await.0, 401);

        let (code, _, body) = call(&server, "POST", "/api/cancel", Some(&server.token), "{}").await;
        assert_eq!((code, body.as_str()), (200, r#"{"cancelled":false}"#));
    }

    #[tokio::test]
    async fn a_prompt_reaches_the_session() {
        let mut server = running().await;
        let token = server.token.clone();
        let (code, _, body) = call(
            &server,
            "POST",
            "/api/prompt",
            Some(&token),
            r#"{"text":"run the tests"}"#,
        )
        .await;
        assert_eq!((code, body.as_str()), (202, r#"{"queued":false}"#));
        assert_eq!(
            server.prompts.recv().await.unwrap(),
            super::super::Request::Prompt("run the tests".to_owned())
        );

        server.hub.set_busy(true);
        let (_, _, body) = call(
            &server,
            "POST",
            "/api/prompt",
            Some(&token),
            r#"{"text":"and clippy"}"#,
        )
        .await;
        assert_eq!(body, r#"{"queued":true}"#);
    }

    #[tokio::test]
    async fn malformed_empty_and_oversized_bodies_are_refused() {
        let server = running().await;
        let token = Some(server.token.as_str());
        let cases = [
            ("not json", 400),
            (r#"{"text":"  "}"#, 400),
            (r#"{"text":"x","extra":1}"#, 400),
            (
                &format!(r#"{{"text":"{}"}}"#, "x".repeat(MAX_BODY_BYTES)),
                413,
            ),
        ];
        for (body, expected) in cases {
            let (code, _, _) = call(&server, "POST", "/api/prompt", token, body).await;
            assert_eq!(code, expected, "{}", &body[..body.len().min(40)]);
        }
    }

    #[tokio::test]
    async fn unknown_paths_and_wrong_methods_are_refused() {
        let server = running().await;
        let token = Some(server.token.as_str());
        assert_eq!(call(&server, "GET", "/nothing", None, "").await.0, 404);
        assert_eq!(call(&server, "GET", "/api/nothing", token, "").await.0, 404);
        assert_eq!(call(&server, "GET", "/api/prompt", token, "").await.0, 405);
        assert_eq!(call(&server, "POST", "/", None, "").await.0, 404);
    }

    #[tokio::test]
    async fn a_page_gets_the_backlog_then_what_follows_and_can_resume() {
        let server = running().await;
        server.hub.notice("first");
        server.hub.notice("second");

        let mut stream = TcpStream::connect(server.address).await.unwrap();
        let text = request("GET", "/api/events?after=0", Some(&server.token), "", false);
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut seen = String::new();
        read_until(&mut stream, &mut seen, "second").await;
        assert!(seen.contains("text/event-stream"), "{seen}");
        assert!(seen.contains("first"), "{seen}");
        server.hub.notice("third");
        read_until(&mut stream, &mut seen, "third").await;

        // A page that saw up to frame 2 is not sent them again.
        let mut again = TcpStream::connect(server.address).await.unwrap();
        let text = request("GET", "/api/events?after=2", Some(&server.token), "", false);
        again.write_all(text.as_bytes()).await.unwrap();
        let mut seen = String::new();
        read_until(&mut again, &mut seen, "third").await;
        assert!(
            !seen.contains("first") && !seen.contains("second"),
            "{seen}"
        );
    }

    #[tokio::test]
    async fn an_approval_is_asked_on_the_page_and_answered_over_http() {
        let server = running().await;
        let approver = RemoteApprover::new(Arc::clone(&server.hub));
        let asked = tokio::spawn(async move {
            approver
                .ask_in(
                    &ToolCall {
                        id: "c1".to_owned(),
                        name: "exec".to_owned(),
                        arguments: serde_json::json!({ "command": ["cargo", "test"] }),
                    },
                    &ToolAction::process("cargo test"),
                    &AskContext::default(),
                )
                .await
        });

        let mut stream = TcpStream::connect(server.address).await.unwrap();
        let text = request("GET", "/api/events", Some(&server.token), "", false);
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut seen = String::new();
        read_until(&mut stream, &mut seen, r#""kind":"approval""#).await;
        assert!(seen.contains(r#""subject":"cargo test""#), "{seen}");

        let token = Some(server.token.as_str());
        let broad = r#"{"id":1,"choice":"remember"}"#;
        assert_eq!(
            call(&server, "POST", "/api/approve", token, broad).await.0,
            400
        );
        let missing = r#"{"id":99,"choice":"once"}"#;
        assert_eq!(
            call(&server, "POST", "/api/approve", token, missing)
                .await
                .0,
            404
        );
        let once = r#"{"id":1,"choice":"once"}"#;
        assert_eq!(
            call(&server, "POST", "/api/approve", token, once).await.0,
            200
        );
        assert_eq!(asked.await.unwrap(), ApprovalReply::Once);
        read_until(&mut stream, &mut seen, r#""outcome":"allowed""#).await;
    }

    /// A workspace with one saved conversation and its log, described to the
    /// hub as the current one.
    fn workspace_with_a_session(server: &Running, name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dino-serve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sessions = root.join(".dino").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        std::fs::write(
            sessions.join("s1.state.json"),
            r#"{"provider":"anthropic","model_key":"claude-x","version":1,"payload":{}}"#,
        )
        .unwrap();
        let lines = [
            r#"{"record":"started","seq":0,"unix_millis":1000,"session":{"model_key":"claude-x"}}"#,
            r#"{"record":"event","seq":1,"unix_millis":1000,"event":{"type":"submitted","text":"hello"}}"#,
            r#"{"record":"event","seq":2,"unix_millis":1700,"event":{"type":"model","model_turn":1,"event":{"type":"finished","reason":"end_turn"}}}"#,
        ];
        std::fs::write(sessions.join("s1.jsonl"), lines.join("\n")).unwrap();
        server.hub.describe(super::super::Info {
            root: root.clone(),
            ..super::super::Info::default()
        });
        root
    }

    #[tokio::test]
    async fn a_page_lists_sessions_and_reads_a_trajectory_but_only_by_id() {
        let server = running().await;
        let root = workspace_with_a_session(&server, "trace");
        let token = Some(server.token.as_str());

        let (code, _, body) = call(&server, "GET", "/api/sessions", token, "").await;
        assert_eq!(code, 200);
        let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(listed["sessions"][0]["id"], "s1");
        assert_eq!(listed["sessions"][0]["resumable"], true);
        assert_eq!(listed["sessions"][0]["first_prompt"], "hello");
        assert_eq!(listed["sessions"][0]["submissions"], 1);

        let (code, _, body) = call(&server, "GET", "/api/trace?id=s1", token, "").await;
        assert_eq!(code, 200, "{body}");
        let trajectory: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(trajectory["submissions"][0]["text"], "hello");
        assert_eq!(trajectory["submissions"][0]["turns"][0]["model_ms"], 700);

        std::fs::write(root.join("outside.jsonl"), "").unwrap();
        for path in [
            "/api/trace?id=../../outside",
            "/api/trace?id=..%2F..%2Foutside",
            "/api/trace?id=nothing",
            "/api/trace",
        ] {
            assert_eq!(call(&server, "GET", path, token, "").await.0, 404, "{path}");
        }
        assert_eq!(call(&server, "GET", "/api/sessions", None, "").await.0, 401);
        assert_eq!(
            call(&server, "POST", "/api/sessions", token, "").await.0,
            405
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_page_resumes_only_a_listed_conversation_and_only_while_idle() {
        let mut server = running().await;
        let root = workspace_with_a_session(&server, "resume");
        let token = server.token.clone();
        let token = Some(token.as_str());

        let unknown = r#"{"id":"../s1"}"#;
        assert_eq!(
            call(&server, "POST", "/api/resume", token, unknown).await.0,
            404
        );
        server.hub.set_busy(true);
        let listed = r#"{"id":"s1"}"#;
        assert_eq!(
            call(&server, "POST", "/api/resume", token, listed).await.0,
            409
        );
        server.hub.set_busy(false);
        assert_eq!(
            call(&server, "POST", "/api/resume", token, listed).await.0,
            202
        );
        assert_eq!(
            server.prompts.recv().await.unwrap(),
            super::super::Request::Resume("s1".to_owned())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_page_moves_only_to_a_workspace_it_was_offered() {
        let mut server = running().await;
        let root = workspace_with_a_session(&server, "move");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        server.hub.offer(super::super::Workspaces {
            configured: vec![elsewhere.clone()],
            recent: None,
        });
        let token = server.token.clone();
        let token = Some(token.as_str());

        let (code, _, body) = call(&server, "GET", "/api/workspaces", token, "").await;
        assert_eq!(code, 200);
        let offered: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(offered["workspaces"][0]["current"], true);
        assert_eq!(
            offered["workspaces"][1]["path"],
            elsewhere.to_str().unwrap()
        );

        for refused in ["/", "/tmp", "", "elsewhere"] {
            let body = serde_json::json!({ "path": refused }).to_string();
            assert_eq!(
                call(&server, "POST", "/api/workspace", token, &body)
                    .await
                    .0,
                404,
                "{refused}"
            );
        }
        let body = serde_json::json!({ "path": elsewhere }).to_string();
        assert_eq!(
            call(&server, "POST", "/api/workspace", token, &body)
                .await
                .0,
            202
        );
        assert_eq!(
            server.prompts.recv().await.unwrap(),
            super::super::Request::Workspace(elsewhere)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn connections_past_the_limit_are_dropped_not_queued() {
        let server = running().await;
        let mut held = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            held.push(TcpStream::connect(server.address).await.unwrap());
        }
        let mut extra = TcpStream::connect(server.address).await.unwrap();
        // Accepted by the kernel, then closed by the server without a response.
        let mut buffer = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), extra.read_to_end(&mut buffer))
            .await
            .expect("the extra connection is closed");
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
        assert!(buffer.is_empty());
    }

    #[test]
    fn only_loopback_and_tailscale_addresses_are_listened_on() {
        let port = 7878;
        for allowed in [
            "127.0.0.1",
            "::1",
            "100.64.0.1",
            "100.127.255.254",
            "fd7a:115c:a1e0::1",
        ] {
            let ip: IpAddr = allowed.parse().unwrap();
            assert_eq!(bind_address(Some(ip), port).unwrap().ip(), ip, "{allowed}");
        }
        for refused in [
            "0.0.0.0",
            "::",
            "192.168.1.20",
            "10.0.0.5",
            "100.63.255.255",
            "100.128.0.1",
            "8.8.8.8",
            "fd7b:115c:a1e0::1",
        ] {
            let ip: IpAddr = refused.parse().unwrap();
            let error = bind_address(Some(ip), port).unwrap_err().to_string();
            assert!(error.contains("Tailscale"), "{refused}: {error}");
        }
    }

    #[test]
    fn the_tailscale_address_is_read_from_the_cli_output() {
        assert_eq!(
            parse_tailscale_ip("100.101.102.103\n"),
            Some("100.101.102.103".parse().unwrap())
        );
        assert_eq!(parse_tailscale_ip("192.168.1.2\n"), None);
        assert_eq!(
            parse_tailscale_ip("failed to connect to local tailscaled\n"),
            None
        );
        assert_eq!(parse_tailscale_ip(""), None);
    }

    #[test]
    fn tokens_are_long_hex_and_never_repeat() {
        let first = new_token().unwrap();
        let second = new_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn comparison_sees_a_difference_anywhere() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abcd", b"bbcd"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn the_frame_a_page_resumes_from_is_read_from_the_query() {
        assert_eq!(after(None), 0);
        assert_eq!(after(Some("after=41")), 41);
        assert_eq!(after(Some("x=1&after=7")), 7);
        assert_eq!(after(Some("after=-1")), 0);
        assert_eq!(after(Some("after=many")), 0);
        assert_eq!(after(Some("afterwards=3")), 0);
    }
}
