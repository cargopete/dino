//! A headless Chromium of Dino's own, on a throwaway profile.

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::Command;

use crate::CALL_LIMIT;
use crate::Error;
use crate::page::Page;
use crate::page::Wire;
use crate::rpc::Rpc;

/// Chromium runs under `sh`, which waits on Dino's pipe and kills the browser
/// when that pipe closes, so a Dino killed by SIGKILL leaves no browser behind.
const GUARD: &str = r#""$0" "$@" </dev/null & browser=$!; read _; kill "$browser" 2>/dev/null; sleep 3; kill -9 "$browser" 2>/dev/null"#;

#[derive(Debug)]
pub struct Chromium {
    rpc: Rpc,
    _guard: Child,
    lifeline: Option<ChildStdin>,
    _profile: tempfile::TempDir,
}

impl Drop for Chromium {
    fn drop(&mut self) {
        self.lifeline.take();
    }
}

/// `DINO_BROWSER`, then the usual installs, then Chrome for Testing's headless
/// shell under `~/.cache/dino`.
#[must_use]
pub fn find_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("DINO_BROWSER") {
        return Some(PathBuf::from(path));
    }
    let apps = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];
    if let Some(app) = apps.iter().map(PathBuf::from).find(|path| path.exists()) {
        return Some(app);
    }
    let names = [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome-headless-shell",
        "brave-browser",
    ];
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        if let Some(found) = names
            .iter()
            .map(|name| dir.join(name))
            .find(|path| path.is_file())
        {
            return Some(found);
        }
    }
    let cache = std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache/dino/chrome"))?;
    std::fs::read_dir(cache).ok()?.flatten().find_map(|entry| {
        ["chrome-headless-shell", "chrome"]
            .iter()
            .map(|name| entry.path().join(name))
            .find(|path| path.is_file())
    })
}

impl Chromium {
    /// # Errors
    ///
    /// Fails when no browser is found, or it does not print its DevTools
    /// address within 20 seconds.
    pub async fn launch() -> Result<Self, Error> {
        Self::launch_guarded(None).await
    }

    /// As [`Self::launch`], with every connection the browser makes sent
    /// through `guard`, websockets included, and no UDP of its own.
    ///
    /// # Errors
    ///
    /// As [`Self::launch`].
    pub async fn launch_guarded(guard: Option<crate::Guard>) -> Result<Self, Error> {
        let binary = find_binary().ok_or(Error::NoBrowser)?;
        let profile = tempfile::Builder::new().prefix("dino-browser-").tempdir()?;
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(GUARD)
            .arg(&binary)
            .arg("--headless=new")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-gpu",
                "--mute-audio",
                "--hide-scrollbars",
                "--window-size=1366,900",
                "--disable-dev-shm-usage",
                "--disable-background-timer-throttling",
                "--disable-renderer-backgrounding",
            ]);
        if let Some(guard) = guard {
            // `<-loopback>` stops Chromium sending localhost round the proxy.
            command.args([
                format!("--proxy-server=http://{}", guard.addr),
                "--proxy-bypass-list=<-loopback>".to_owned(),
                "--disable-quic".to_owned(),
                "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_owned(),
            ]);
        }
        if std::env::var_os("DINO_BROWSER_NO_SANDBOX").is_some() {
            command.arg("--no-sandbox");
        }
        command.arg("about:blank");
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| Error::Launch(format!("{}: {error}", binary.display())))?;
        let lifeline = child.stdin.take();
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Launch("no stderr".into()))?;
        let mut lines = BufReader::new(stderr).lines();
        let address = tokio::time::timeout(Duration::from_secs(20), async {
            let mut said = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(at) = line.find("ws://") {
                    return Ok(line[at..].trim().to_owned());
                }
                said.push(line);
            }
            Err(Error::Launch(said.join("\n")))
        })
        .await
        .map_err(|_| Error::Launch("no DevTools address within 20 s".into()))??;
        // Chromium blocks once a full stderr pipe stops being read.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        let (socket, _) = tokio_tungstenite::connect_async(address.as_str())
            .await
            .map_err(|error| Error::Launch(format!("cannot reach {address}: {error}")))?;
        Ok(Self {
            rpc: Rpc::spawn(socket),
            _guard: child,
            lifeline,
            _profile: profile,
        })
    }

    #[must_use]
    pub fn alive(&self) -> bool {
        self.rpc.alive()
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        self.rpc
            .call(json!({ "method": method, "params": params }), CALL_LIMIT)
            .await
    }

    /// # Errors
    ///
    /// Fails when the browser is gone.
    pub async fn new_page(&self) -> Result<Page, Error> {
        let target = self
            .call("Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let target = target["targetId"]
            .as_str()
            .ok_or_else(|| Error::Remote("no targetId".into()))?
            .to_owned();
        let attached = self
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target, "flatten": true }),
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or_else(|| Error::Remote("no sessionId".into()))?
            .to_owned();
        Ok(Page::new(Wire::Cdp {
            rpc: self.rpc.clone(),
            session,
            target,
        }))
    }
}
