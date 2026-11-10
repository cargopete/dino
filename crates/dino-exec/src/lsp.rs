//! A minimal Language Server Protocol client over stdio, enough to ask
//! rust-analyzer where things are.
//!
//! One reader task owns the server's output: it hands each response to the
//! request waiting for it, answers the server's own requests, and tracks
//! whether the server says it is still loading the workspace.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use dino_core::ToolError;
use dino_sandbox::Sandbox;
use dino_sandbox::Snapshot;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::sync::watch;

use crate::process;

/// The server's answer to one request: its result, or its error's code and
/// message.
type Reply = Result<Value, (i64, String)>;
type Pending = Arc<std::sync::Mutex<HashMap<i64, oneshot::Sender<Reply>>>>;

/// `ContentModified`: the state changed under the request; asking again is
/// the remedy the protocol prescribes.
const CONTENT_MODIFIED: i64 = -32_801;
const LARGEST_MESSAGE: usize = 64 * 1_024 * 1_024;

/// What the server last said about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Status {
    /// Done loading and indexing: answers will not change by waiting.
    pub quiescent: bool,
    pub message: Option<String>,
    pub exited: bool,
}

/// A running server, killed with everything it started when dropped.
pub(crate) struct Server {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    status: watch::Receiver<Status>,
    next_id: i64,
    reader: tokio::task::JoinHandle<()>,
    group: Option<i32>,
    snapshot: Option<Snapshot>,
    timeout: Duration,
    _child: Child,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Server")
            .field("status", &*self.status.borrow())
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Starts `program` in the sandbox and completes the handshake.
    pub(crate) async fn start(
        program: &str,
        arguments: &[String],
        root: &Path,
        sandbox: &dyn Sandbox,
        initialization: Value,
        timeout: Duration,
    ) -> Result<Self, ToolError> {
        let (mut command, snapshot) = process::prepare(program, arguments, root, sandbox, &[])?;
        let mut child = command
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| ToolError::new(format!("cannot start {program}: {error}")))?;
        let group = child.id().and_then(|id| i32::try_from(id).ok());
        let stdin = Arc::new(Mutex::new(
            child
                .stdin
                .take()
                .ok_or_else(|| ToolError::new("no stdin"))?,
        ));
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::new("no stdout"))?;
        let pending: Pending = Arc::default();
        let (status_tx, status) = watch::channel(Status::default());
        let reader = tokio::spawn(read_loop(
            BufReader::new(stdout),
            Arc::clone(&stdin),
            Arc::clone(&pending),
            status_tx,
        ));
        let mut server = Self {
            stdin,
            pending,
            status,
            next_id: 1,
            reader,
            group,
            snapshot,
            timeout,
            _child: child,
        };
        server.request("initialize", initialization).await?;
        server.notify("initialized", json!({})).await?;
        Ok(server)
    }

    pub(crate) fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    /// Waits until the server is quiescent or `limit` passes, and says which.
    pub(crate) async fn settle(&mut self, limit: Duration) -> bool {
        let settled = tokio::time::timeout(
            limit,
            self.status
                .wait_for(|status| status.quiescent || status.exited),
        )
        .await;
        matches!(settled, Ok(Ok(status)) if status.quiescent)
    }

    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<(), ToolError> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        write(&self.stdin, &message).await
    }

    /// One request, asked again while the server says the state moved under it.
    pub(crate) async fn request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, ToolError> {
        for _ in 0..3 {
            match self.ask(method, params.clone()).await? {
                Err((CONTENT_MODIFIED, _)) => tokio::time::sleep(Duration::from_millis(500)).await,
                Err((_, message)) => {
                    return Err(ToolError::new(format!(
                        "rust-analyzer refused {method}: {message}"
                    )));
                }
                Ok(result) => return Ok(result),
            }
        }
        Err(ToolError::new(format!(
            "rust-analyzer kept changing under {method}; try again"
        )))
    }

    async fn ask(&mut self, method: &str, params: Value) -> Result<Reply, ToolError> {
        let id = self.next_id;
        self.next_id += 1;
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| ToolError::new("the LSP request table lock was poisoned"))?
            .insert(id, sender);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        write(&self.stdin, &message).await?;
        let answered = tokio::time::timeout(self.timeout, receiver).await;
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
        match answered {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(ToolError::new("rust-analyzer exited")),
            Err(_) => {
                let _ = self.notify("$/cancelRequest", json!({"id": id})).await;
                Err(ToolError::new(format!(
                    "rust-analyzer did not answer {method} within {} seconds",
                    self.timeout.as_secs()
                )))
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.reader.abort();
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(group),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        if let Some(snapshot) = self.snapshot.as_mut() {
            let _ = snapshot.restore();
        }
    }
}

async fn write(stdin: &Mutex<ChildStdin>, message: &Value) -> Result<(), ToolError> {
    let mut stdin = stdin.lock().await;
    let failed = |error: std::io::Error| ToolError::new(format!("LSP write failed: {error}"));
    stdin.write_all(&frame(message)).await.map_err(failed)?;
    stdin.flush().await.map_err(failed)
}

async fn read_loop<R: AsyncBufRead + Unpin>(
    mut stdout: R,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    status: watch::Sender<Status>,
) {
    while let Ok(Some(message)) = read_frame(&mut stdout).await {
        match (
            message.get("id"),
            message.get("method").and_then(Value::as_str),
        ) {
            (Some(id), Some(method)) => {
                // Progress tokens and registrations are accepted; nothing else is offered.
                let reply = if matches!(
                    method,
                    "window/workDoneProgress/create" | "client/registerCapability"
                ) {
                    json!({"jsonrpc": "2.0", "id": id, "result": null})
                } else {
                    json!({"jsonrpc": "2.0", "id": id,
                           "error": {"code": -32_601, "message": "not supported"}})
                };
                if write(&stdin, &reply).await.is_err() {
                    break;
                }
            }
            (None, Some("experimental/serverStatus")) => {
                let params = &message["params"];
                status.send_modify(|status| {
                    status.quiescent = params["quiescent"] == true;
                    status.message = params["message"].as_str().map(str::to_owned);
                });
            }
            (Some(id), None) => {
                let Some(id) = id.as_i64() else { continue };
                let reply = match message.get("error") {
                    Some(error) => Err((
                        error["code"].as_i64().unwrap_or_default(),
                        error["message"]
                            .as_str()
                            .unwrap_or("unknown error")
                            .to_owned(),
                    )),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let waiting = pending
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.remove(&id));
                if let Some(waiting) = waiting {
                    let _ = waiting.send(reply);
                }
            }
            _ => {}
        }
    }
    status.send_modify(|status| status.exited = true);
    if let Ok(mut pending) = pending.lock() {
        pending.clear();
    }
}

/// One message with its `Content-Length` header.
pub(crate) fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// The next message, or None at the end of the stream.
pub(crate) async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Value>> {
    let invalid = |what: String| std::io::Error::new(std::io::ErrorKind::InvalidData, what);
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if length.is_some() {
                break;
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| invalid(format!("bad Content-Length: {value}")))?,
            );
        }
    }
    let length = length.unwrap_or_default();
    if length > LARGEST_MESSAGE {
        return Err(invalid(format!("a {length}-byte message is too large")));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| invalid(format!("not JSON: {error}")))
}

/// A `file://` URI for an absolute path.
pub(crate) fn uri(path: &Path) -> String {
    let mut text = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            text.push(char::from(byte));
        } else {
            let _ = write!(text, "%{byte:02X}");
        }
    }
    text
}

/// The path a `file://` URI names, or None for any other scheme.
pub(crate) fn path_of(uri: &str) -> Option<std::path::PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%'
            && let Some(hex) = tail.get(..2)
            && let Ok(decoded) = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16)
        {
            bytes.push(decoded);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    Some(std::path::PathBuf::from(String::from_utf8(bytes).ok()?))
}

/// The UTF-16 offset LSP counts for a character column, both from zero.
pub(crate) fn utf16_offset(line: &str, column: usize) -> usize {
    line.chars().take(column).map(char::len_utf16).sum()
}

/// The character column of a UTF-16 offset, both from zero.
pub(crate) fn char_column(line: &str, offset: usize) -> usize {
    let mut units = 0;
    for (column, character) in line.chars().enumerate() {
        if units >= offset {
            return column;
        }
        units += character.len_utf16();
    }
    line.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn frames_read_back_in_order_and_the_end_is_none() {
        let first = json!({"jsonrpc": "2.0", "id": 1, "result": {"text": "é∀"}});
        let second = json!({"jsonrpc": "2.0", "method": "x"});
        let mut bytes = frame(&first);
        // A server may send other headers, in any case.
        bytes.extend_from_slice(b"content-type: application/vscode-jsonrpc; charset=utf-8\r\n");
        bytes.extend_from_slice(&frame(&second));
        let mut reader = BufReader::new(bytes.as_slice());
        assert_eq!(read_frame(&mut reader).await.unwrap(), Some(first));
        assert_eq!(read_frame(&mut reader).await.unwrap(), Some(second));
        assert_eq!(read_frame(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_length_counts_bytes_not_characters() {
        let bytes = frame(&json!("∀"));
        assert!(bytes.starts_with(b"Content-Length: 5\r\n\r\n"));
    }

    #[tokio::test]
    async fn a_bad_length_is_an_error_rather_than_a_hang() {
        let mut reader = BufReader::new(&b"Content-Length: lots\r\n\r\n{}"[..]);
        assert!(read_frame(&mut reader).await.is_err());
        let mut short = BufReader::new(&b"Content-Length: 10\r\n\r\n{}"[..]);
        assert!(read_frame(&mut short).await.is_err());
    }

    #[test]
    fn uris_round_trip_through_escapes() {
        let path = Path::new("/tmp/a b/ü#1.rs");
        let encoded = uri(path);
        assert_eq!(encoded, "file:///tmp/a%20b/%C3%BC%231.rs");
        assert_eq!(path_of(&encoded).as_deref(), Some(path));
        assert_eq!(path_of("untitled:1"), None);
    }

    #[test]
    fn columns_convert_through_utf16() {
        let line = "let 𝔵 = é + y;";
        let y = line.chars().position(|c| c == 'y').unwrap();
        let offset = utf16_offset(line, y);
        assert_eq!(offset, y + 1, "𝔵 is two UTF-16 units");
        assert_eq!(char_column(line, offset), y);
        assert_eq!(char_column(line, 999), line.chars().count());
    }
}
