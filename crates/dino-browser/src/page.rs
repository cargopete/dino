//! One tab: open, settle, read, act. Written once over CDP, which both the
//! headless browser and the extension's `chrome.debugger` speak.

use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::CALL_LIMIT;
use crate::Error;
use crate::rpc::Rpc;

const EXTRACT: &str = include_str!("extract.js");
const SETTLE: &str = include_str!("settle.js");
const NAVIGATION_LIMIT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone)]
pub(crate) enum Wire {
    Cdp {
        rpc: Rpc,
        session: String,
        target: String,
    },
    Bridge {
        rpc: Rpc,
        tab: i64,
    },
}

#[derive(Debug)]
pub struct Page {
    wire: Wire,
}

/// Where a navigation ended up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Landing {
    pub url: String,
    /// The HTTP status of the document, when the browser reports one.
    pub status: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub href: String,
    pub text: String,
}

/// An element a model may click or type into, by `n`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub n: u32,
    pub role: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reading {
    pub url: String,
    pub canonical: Option<String>,
    pub status: Option<u16>,
    pub title: String,
    pub lang: Option<String>,
    pub markdown: String,
    pub links: Vec<Link>,
    pub jsonld: Vec<Value>,
    pub meta: serde_json::Map<String, Value>,
    pub actions: Vec<Action>,
}

impl Page {
    pub(crate) fn new(wire: Wire) -> Self {
        Self { wire }
    }

    /// # Errors
    ///
    /// Fails when the browser rejects the command or is gone.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        self.call_within(method, params, CALL_LIMIT).await
    }

    async fn call_within(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, Error> {
        match &self.wire {
            Wire::Cdp { rpc, session, .. } => {
                rpc.call(
                    json!({ "method": method, "params": params, "sessionId": session }),
                    limit,
                )
                .await
            }
            Wire::Bridge { rpc, tab } => {
                rpc.call(
                    json!({ "op": "cdp", "tab": tab, "method": method, "params": params }),
                    limit,
                )
                .await
            }
        }
    }

    /// Runs `expression` in the page and returns its value, awaiting a promise.
    /// A navigation can destroy the context mid-call, so that is retried.
    ///
    /// # Errors
    ///
    /// Fails when the script throws or the page will not answer.
    pub async fn eval(&self, expression: &str) -> Result<Value, Error> {
        let mut tries = 0;
        loop {
            let result = self
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": expression, "awaitPromise": true, "returnByValue": true, "userGesture": true }),
                )
                .await;
            match result {
                Ok(reply) => {
                    if let Some(thrown) = reply.get("exceptionDetails") {
                        let text = thrown["exception"]["description"]
                            .as_str()
                            .or_else(|| thrown["text"].as_str());
                        return Err(Error::Page(text.unwrap_or("the script threw").to_owned()));
                    }
                    return Ok(reply["result"]["value"].clone());
                }
                Err(Error::Remote(message)) if tries < 8 && message.contains("context") => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Opens `url` and waits for it to settle.
    ///
    /// # Errors
    ///
    /// Fails when the navigation does, with the browser's reason.
    pub async fn goto(&self, url: &str) -> Result<Landing, Error> {
        let reply = self
            .call_within("Page.navigate", json!({ "url": url }), NAVIGATION_LIMIT)
            .await?;
        if let Some(reason) = reply["errorText"].as_str().filter(|text| !text.is_empty()) {
            return Err(Error::Navigation(reason.to_owned()));
        }
        self.settle().await?;
        let landed = self
            .eval("({url: location.href, status: (performance.getEntriesByType('navigation')[0] || {}).responseStatus || null})")
            .await?;
        Ok(Landing {
            url: landed["url"].as_str().unwrap_or(url).to_owned(),
            status: landed["status"]
                .as_u64()
                .and_then(|status| u16::try_from(status).ok())
                .filter(|&s| s > 0),
        })
    }

    /// Waits for the load event and then 500 ms without a DOM change, at most 15 s.
    ///
    /// # Errors
    ///
    /// Fails when the page will not answer.
    pub async fn settle(&self) -> Result<(), Error> {
        self.eval(SETTLE).await.map(|_| ())
    }

    /// # Errors
    ///
    /// Fails when the page will not answer or the reading cannot be parsed.
    pub async fn read(&self) -> Result<Reading, Error> {
        let value = self.eval(EXTRACT).await?;
        serde_json::from_value(value)
            .map_err(|error| Error::Page(format!("unreadable reading: {error}")))
    }

    async fn centre(&self, n: u32) -> Result<(f64, f64), Error> {
        let at = self
            .eval(&format!(
                "(() => {{ const el = document.querySelector('[data-dino-ref=\"{n}\"]'); if (!el) return null; \
                 el.scrollIntoView({{block: 'center', inline: 'center'}}); const r = el.getBoundingClientRect(); \
                 return [r.left + r.width / 2, r.top + r.height / 2]; }})()"
            ))
            .await?;
        match (at[0].as_f64(), at[1].as_f64()) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err(Error::Page(format!(
                "no element {n}; read the page again for fresh numbers"
            ))),
        }
    }

    /// Clicks action `n` with real mouse events, then settles.
    ///
    /// # Errors
    ///
    /// Fails when there is no such action on the page.
    pub async fn click(&self, n: u32) -> Result<(), Error> {
        let (x, y) = self.centre(n).await?;
        for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            )
            .await?;
        }
        self.settle().await
    }

    /// Types into action `n`, replacing what was there, and presses Enter if asked.
    ///
    /// # Errors
    ///
    /// Fails when there is no such action on the page.
    pub async fn type_into(&self, n: u32, text: &str, enter: bool) -> Result<(), Error> {
        self.click(n).await?;
        self.eval(&format!(
            "(() => {{ const el = document.querySelector('[data-dino-ref=\"{n}\"]'); el.focus(); \
             if ('value' in el) el.value = ''; else el.textContent = ''; }})()"
        ))
        .await?;
        self.call("Input.insertText", json!({ "text": text }))
            .await?;
        if enter {
            for kind in ["keyDown", "keyUp"] {
                self.call(
                    "Input.dispatchKeyEvent",
                    json!({ "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": "\r" }),
                )
                .await?;
            }
        }
        self.settle().await
    }

    /// Scrolls until the page stops growing or `rounds` run out, for listings
    /// that load as you go. Returns the rounds that grew it.
    ///
    /// # Errors
    ///
    /// Fails when the page will not answer.
    pub async fn scroll_all(&self, rounds: u32) -> Result<u32, Error> {
        let grew = self
            .eval(&format!(
                "(async () => {{ let grew = 0; for (let i = 0; i < {rounds}; i++) {{ \
                 const h = document.scrollingElement.scrollHeight; window.scrollTo(0, h); \
                 await new Promise(r => setTimeout(r, 900)); \
                 if (document.scrollingElement.scrollHeight <= h) {{ await new Promise(r => setTimeout(r, 900)); \
                 if (document.scrollingElement.scrollHeight <= h) break; }} grew++; }} return grew; }})()"
            ))
            .await?;
        self.settle().await?;
        Ok(grew
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0))
    }

    /// A PNG of the viewport, or of the whole page.
    ///
    /// # Errors
    ///
    /// Fails when the browser will not capture.
    pub async fn screenshot(&self, full: bool) -> Result<Vec<u8>, Error> {
        let shot = self
            .call(
                "Page.captureScreenshot",
                json!({ "format": "png", "captureBeyondViewport": full }),
            )
            .await?;
        let data = shot["data"]
            .as_str()
            .ok_or_else(|| Error::Remote("no image".into()))?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| Error::Remote(error.to_string()))
    }

    /// # Errors
    ///
    /// Fails when the browser is gone, which callers may ignore.
    pub async fn close(self) -> Result<(), Error> {
        match self.wire {
            Wire::Cdp { rpc, target, .. } => rpc
                .call(
                    json!({ "method": "Target.closeTarget", "params": { "targetId": target } }),
                    CALL_LIMIT,
                )
                .await
                .map(|_| ()),
            Wire::Bridge { rpc, tab } => rpc
                .call(json!({ "op": "close", "tab": tab }), CALL_LIMIT)
                .await
                .map(|_| ()),
        }
    }
}
