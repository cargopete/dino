//! Numbered JSON calls over one websocket, replies matched by `id`.
//!
//! CDP and the extension bridge share this: both send `{id, ...}` and answer
//! `{id, result}` or `{id, error: {message}}`. Messages without an `id` are
//! events and go to whoever subscribed.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::Error;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Error>>>>>;

#[derive(Debug, Clone)]
pub struct Rpc {
    next: Arc<AtomicU64>,
    pending: Pending,
    out: mpsc::UnboundedSender<String>,
    events: broadcast::Sender<Value>,
    alive: Arc<AtomicBool>,
}

impl Rpc {
    pub fn spawn<S>(socket: WebSocketStream<S>) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut stream) = socket.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::default();
        let (events, _) = broadcast::channel(1_024);
        let alive = Arc::new(AtomicBool::new(true));

        tokio::spawn(async move {
            while let Some(text) = outgoing.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
        });

        let reader = Self {
            next: Arc::new(AtomicU64::new(1)),
            pending,
            out,
            events,
            alive,
        };
        let rpc = reader.clone();
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                let text = match message {
                    Message::Text(text) => text.to_string(),
                    Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                reader.deliver(value);
            }
            reader.alive.store(false, Ordering::SeqCst);
            for (_, waiter) in reader.pending.lock().expect("pending lock").drain() {
                let _ = waiter.send(Err(Error::Closed));
            }
        });
        rpc
    }

    fn deliver(&self, value: Value) {
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            let _ = self.events.send(value);
            return;
        };
        let Some(waiter) = self.pending.lock().expect("pending lock").remove(&id) else {
            return;
        };
        let reply = match value.get("error") {
            Some(error) => Err(Error::Remote(
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| error.to_string(), str::to_owned),
            )),
            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = waiter.send(reply);
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Sends `body` with an `id` added and waits up to `limit` for its reply.
    pub async fn call(&self, mut body: Value, limit: Duration) -> Result<Value, Error> {
        if !self.alive() {
            return Err(Error::Closed);
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        body["id"] = id.into();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        if self.out.send(body.to_string()).is_err() {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(Error::Closed);
        }
        match tokio::time::timeout(limit, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err(Error::Closed),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                let method = body
                    .get("method")
                    .or_else(|| body.get("op"))
                    .cloned()
                    .unwrap_or_default();
                Err(Error::Timeout(format!(
                    "{method} after {}s",
                    limit.as_secs()
                )))
            }
        }
    }
}
