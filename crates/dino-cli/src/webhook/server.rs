//! The listener GitHub delivers to. One route, `POST /github`, and nothing is
//! read from a delivery until its signature checks out.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
use http_body_util::Limited;
use hyper::Method;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::rt::TokioTimer;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

use super::Admit;
use super::Config;
use super::Gate;
use super::Job;
use super::Verdict;

/// Connections open at once. GitHub sends one delivery a connection.
const MAX_CONNECTIONS: usize = 16;

const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// A pull request event is a few tens of kilobytes; GitHub's own cap is 25 MB.
const MAX_BODY_BYTES: usize = 1_024 * 1_024;
const MAX_BUFFER_BYTES: usize = 16 * 1_024;

/// What a delivery with a bad signature waits before it is told.
const REFUSAL_DELAY: Duration = Duration::from_millis(250);

struct Shared {
    config: Arc<Config>,
    gate: Mutex<Gate>,
    jobs: mpsc::Sender<Job>,
}

/// Answers deliveries on `listener`, passing each review it admits to `jobs`.
pub async fn serve(
    listener: TcpListener,
    config: Arc<Config>,
    jobs: mpsc::Sender<Job>,
) -> Infallible {
    let shared = Arc::new(Shared {
        gate: Mutex::new(Gate::new(config.per_hour)),
        config,
        jobs,
    });
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
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

fn say(status: StatusCode, text: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(format!("{text}\n"))))
        .expect("fixed headers are valid")
}

fn header<'a>(request: &'a Request<Incoming>, name: &str) -> Option<&'a str> {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

async fn route(
    request: Request<Incoming>,
    shared: &Shared,
    peer: SocketAddr,
) -> Response<Full<Bytes>> {
    if request.uri().path() != "/github" {
        return say(StatusCode::NOT_FOUND, "not found");
    }
    if request.method() != Method::POST {
        return say(StatusCode::METHOD_NOT_ALLOWED, "deliveries are POSTed");
    }
    let signature = header(&request, "x-hub-signature-256").map(str::to_owned);
    let event = header(&request, "x-github-event").map(str::to_owned);
    let delivery = header(&request, "x-github-delivery").map(str::to_owned);
    let read = Limited::new(request.into_body(), MAX_BODY_BYTES).collect();
    let body = match tokio::time::timeout(BODY_TIMEOUT, read).await {
        Ok(Ok(body)) => body.to_bytes(),
        Ok(Err(_)) => return say(StatusCode::PAYLOAD_TOO_LARGE, "too large"),
        Err(_) => return say(StatusCode::REQUEST_TIMEOUT, "too slow"),
    };
    if !super::signed(&shared.config.secret, &body, signature.as_deref()) {
        eprintln!("dino: webhook: refused a delivery from {peer} without a valid signature");
        tokio::time::sleep(REFUSAL_DELAY).await;
        return say(StatusCode::UNAUTHORIZED, "bad signature");
    }
    let (Some(event), Some(delivery)) = (event, delivery) else {
        return say(StatusCode::BAD_REQUEST, "no event or delivery id");
    };
    let job = match super::judge(&event, &delivery, &body, &shared.config) {
        Ok(Verdict::Review(job)) => job,
        Ok(Verdict::Ignore(why)) => return say(StatusCode::OK, &format!("ignored: {why}")),
        Err(why) => return say(StatusCode::BAD_REQUEST, why),
    };
    // Reserved before the gate, so a full queue does not count a review as started.
    let Ok(permit) = shared.jobs.try_reserve() else {
        return say(
            StatusCode::SERVICE_UNAVAILABLE,
            "reviews are queued; send it later",
        );
    };
    let admitted = shared
        .gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .admit(&job, Instant::now());
    match admitted {
        Admit::Admitted => {
            eprintln!(
                "dino: webhook: queued a review of {}#{} at {} (delivery {})",
                job.repo,
                job.number,
                job.short_head(),
                job.delivery
            );
            permit.send(job);
            say(StatusCode::ACCEPTED, "queued")
        }
        Admit::Duplicate => say(StatusCode::OK, "ignored: delivered before"),
        Admit::AlreadyReviewed => say(StatusCode::OK, "ignored: this head was reviewed"),
        Admit::Limited => say(
            StatusCode::TOO_MANY_REQUESTS,
            "over the hourly review limit; redeliver it later",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    use std::path::PathBuf;

    use hmac::KeyInit as _;
    use hmac::Mac as _;
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpStream;

    use super::*;
    use crate::settings::WebhookConfig;

    const SECRET: &str = "a secret of some length";

    struct Running {
        address: SocketAddr,
        jobs: mpsc::Receiver<Job>,
        task: tokio::task::JoinHandle<Infallible>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn running(per_hour: u32, queue: usize) -> Running {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = Config::resolve(
            Some(&WebhookConfig {
                secret: Some(SECRET.to_owned()),
                repos: BTreeMap::from([("cargopete/dino".to_owned(), PathBuf::from("/src/dino"))]),
                per_hour: Some(per_hour),
                ..WebhookConfig::default()
            }),
            None,
            &|_| None,
        )
        .unwrap();
        let (sender, jobs) = mpsc::channel(queue);
        let task = tokio::spawn(serve(listener, Arc::new(config), sender));
        Running {
            address,
            jobs,
            task,
        }
    }

    fn ready(head: &str) -> String {
        serde_json::json!({
            "action": "ready_for_review",
            "number": 7,
            "pull_request": {
                "state": "open",
                "title": "t",
                "body": null,
                "head": { "sha": head },
                "base": { "sha": "f".repeat(40) },
                "author_association": "COLLABORATOR",
            },
            "repository": { "full_name": "cargopete/dino" },
        })
        .to_string()
    }

    fn sign(body: &str) -> String {
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(body.as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .fold("sha256=".to_owned(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    }

    async fn deliver(
        server: &Running,
        path: &str,
        event: &str,
        delivery: &str,
        signature: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let mut text = format!(
            "POST {path} HTTP/1.1\r\nHost: dino\r\nConnection: close\r\n\
             X-GitHub-Event: {event}\r\nX-GitHub-Delivery: {delivery}\r\n"
        );
        if let Some(signature) = signature {
            let _ = write!(text, "X-Hub-Signature-256: {signature}\r\n");
        }
        let _ = write!(text, "Content-Length: {}\r\n\r\n{body}", body.len());
        let mut stream = TcpStream::connect(server.address).await.unwrap();
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        let response = String::from_utf8_lossy(&response).into_owned();
        let code = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.trim().to_owned())
            .unwrap_or_default();
        (code, body)
    }

    #[tokio::test]
    async fn a_signed_ready_pull_request_is_queued_once() {
        let mut server = running(6, 4).await;
        let body = ready(&"a".repeat(40));
        let delivered = signed_delivery(&server, "pull_request", "d1", &body).await;
        assert_eq!(delivered, (202, "queued".to_owned()));
        let job = server.jobs.recv().await.unwrap();
        assert_eq!((job.number, job.delivery.as_str()), (7, "d1"));

        let again = signed_delivery(&server, "pull_request", "d1", &body).await;
        assert_eq!(again, (200, "ignored: delivered before".to_owned()));
        let other = signed_delivery(&server, "pull_request", "d2", &body).await;
        assert_eq!(other.1, "ignored: this head was reviewed");
        assert!(server.jobs.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_unsigned_or_wrongly_signed_delivery_does_nothing() {
        let mut server = running(6, 4).await;
        let body = ready(&"a".repeat(40));
        let wrong = sign(&format!("{body} "));
        for signature in [None, Some(wrong.as_str()), Some("sha256=00")] {
            let (code, _) =
                deliver(&server, "/github", "pull_request", "d1", signature, &body).await;
            assert_eq!(code, 401, "{signature:?}");
        }
        assert!(server.jobs.try_recv().is_err());
        // Refused unsigned, so the delivery id was never taken.
        let signed = signed_delivery(&server, "pull_request", "d1", &body).await;
        assert_eq!(signed.0, 202);
    }

    #[tokio::test]
    async fn pings_other_events_and_other_paths_are_answered_without_a_review() {
        let mut server = running(6, 4).await;
        let zen = r#"{"zen":"Keep it logically awesome."}"#;
        let answer = signed_delivery(&server, "ping", "p", zen).await;
        assert_eq!(answer, (200, "ignored: pong".to_owned()));
        assert_eq!(signed_delivery(&server, "push", "q", "{}").await.0, 200);
        let elsewhere = deliver(&server, "/api/events", "ping", "r", None, "{}").await;
        assert_eq!(elsewhere.0, 404);
        let garbled = signed_delivery(&server, "pull_request", "s", "[]").await;
        assert_eq!(garbled.0, 400);
        assert!(server.jobs.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_hourly_limit_and_a_full_queue_refuse_without_losing_the_delivery() {
        let mut server = running(1, 1).await;
        let first = ready(&"a".repeat(40));
        let second = ready(&"b".repeat(40));
        assert_eq!(
            signed_delivery(&server, "pull_request", "d1", &first)
                .await
                .0,
            202
        );
        // The queue holds the first, unread.
        let full = signed_delivery(&server, "pull_request", "d2", &second).await;
        assert_eq!(full.0, 503);
        server.jobs.recv().await.unwrap();
        let limited = signed_delivery(&server, "pull_request", "d2", &second).await;
        assert_eq!(limited.0, 429);
    }

    async fn signed_delivery(
        server: &Running,
        event: &str,
        delivery: &str,
        body: &str,
    ) -> (u16, String) {
        let signature = sign(body);
        deliver(server, "/github", event, delivery, Some(&signature), body).await
    }
}
