//! The person's own browser, through the Dino extension. The extension dials
//! in to `127.0.0.1` with a token and relays CDP over `chrome.debugger`.

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::handshake::server::ErrorResponse;
use tokio_tungstenite::tungstenite::handshake::server::Request;
use tokio_tungstenite::tungstenite::handshake::server::Response;
use tokio_tungstenite::tungstenite::http::StatusCode;

use crate::CALL_LIMIT;
use crate::Error;
use crate::page::Page;
use crate::page::Wire;
use crate::rpc::Rpc;

pub const DEFAULT_PORT: u16 = 8793;

#[derive(Debug, Clone)]
pub struct Bridge {
    current: Arc<Mutex<Option<Rpc>>>,
    connected: watch::Receiver<u64>,
    pub port: u16,
}

#[must_use]
pub fn token_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("dino").join("browser-token")
}

/// The token the extension must present, made on first use and kept 0600.
///
/// # Errors
///
/// Fails when the token file cannot be read or written.
pub fn token() -> Result<String, Error> {
    let path = token_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_owned();
        if existing.len() >= 32 {
            return Ok(existing);
        }
    }
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token = crate::hex(&bytes);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

fn admitted(request: &Request, token: &str) -> bool {
    let origin_ok = request
        .headers()
        .get("origin")
        .and_then(|origin| origin.to_str().ok())
        .is_some_and(|origin| {
            origin.starts_with("chrome-extension://") || origin.starts_with("moz-extension://")
        });
    let token_ok = request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .any(|(key, value)| key == "token" && constant_eq(value.as_bytes(), token.as_bytes()));
    origin_ok && token_ok
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Bridge {
    /// Listens on `127.0.0.1:port` for the extension. The newest connection wins,
    /// so a reloaded extension replaces the one before it.
    ///
    /// # Errors
    ///
    /// Fails when the port is taken or the token cannot be made.
    pub async fn listen(port: u16) -> Result<Self, Error> {
        let token = token()?;
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let port = listener.local_addr()?.port();
        let current: Arc<Mutex<Option<Rpc>>> = Arc::default();
        let (bump, connected) = watch::channel(0u64);
        let slot = current.clone();
        tokio::spawn(async move {
            let mut count = 0;
            while let Ok((stream, _)) = listener.accept().await {
                let token = token.clone();
                // tungstenite fixes this callback's error type.
                #[allow(clippy::result_large_err)]
                let check = move |request: &Request,
                                  response: Response|
                      -> Result<Response, ErrorResponse> {
                    if admitted(request, &token) {
                        Ok(response)
                    } else {
                        let mut refusal = ErrorResponse::new(Some("wrong token or origin".into()));
                        *refusal.status_mut() = StatusCode::FORBIDDEN;
                        Err(refusal)
                    }
                };
                let Ok(socket) = tokio_tungstenite::accept_hdr_async(stream, check).await else {
                    continue;
                };
                *slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Rpc::spawn(socket));
                count += 1;
                let _ = bump.send(count);
            }
        });
        Ok(Self {
            current,
            connected,
            port,
        })
    }

    fn rpc(&self) -> Result<Rpc, Error> {
        self.current
            .lock()
            .expect("bridge lock")
            .clone()
            .filter(Rpc::alive)
            .ok_or(Error::Closed)
    }

    #[must_use]
    pub fn alive(&self) -> bool {
        self.rpc().is_ok()
    }

    /// Waits for the extension to dial in.
    ///
    /// # Errors
    ///
    /// Times out when it has not within `limit`.
    pub async fn wait(&self, limit: Duration) -> Result<(), Error> {
        let mut connected = self.connected.clone();
        tokio::time::timeout(limit, async {
            while !self.alive() {
                if connected.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .map_err(|_| {
            Error::Timeout(format!(
                "the extension did not connect to 127.0.0.1:{}",
                self.port
            ))
        })
    }

    /// A new background tab in the person's browser, which only Dino drives.
    ///
    /// # Errors
    ///
    /// Fails when the extension is not connected or will not open a tab.
    pub async fn new_page(&self) -> Result<Page, Error> {
        let rpc = self.rpc()?;
        let opened = rpc.call(json!({ "op": "open" }), CALL_LIMIT).await?;
        let tab = opened["tab"]
            .as_i64()
            .ok_or_else(|| Error::Remote("no tab".into()))?;
        Ok(Page::new(Wire::Bridge { rpc, tab }))
    }

    /// Waits for the person to press "Send this tab to Dino" and returns that tab.
    ///
    /// # Errors
    ///
    /// Fails when the extension disconnects first.
    pub async fn shared_tab(&self) -> Result<Page, Error> {
        let rpc = self.rpc()?;
        let mut events = rpc.events();
        loop {
            match events.recv().await {
                Ok(event) if event["op"] == "shared" => {
                    if let Some(tab) = event["tab"].as_i64() {
                        return Ok(Page::new(Wire::Bridge { rpc, tab }));
                    }
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return Err(Error::Closed),
            }
        }
    }

    /// What the extension says about itself.
    ///
    /// # Errors
    ///
    /// Fails when it is not connected.
    pub async fn hello(&self) -> Result<Value, Error> {
        self.rpc()?.call(json!({ "op": "hello" }), CALL_LIMIT).await
    }
}
