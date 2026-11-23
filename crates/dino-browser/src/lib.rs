//! A real Chromium, driven over the DevTools Protocol, either one Dino starts
//! headless or the person's own through the Dino extension. RFC-0022.

mod bridge;
mod chromium;
#[cfg(feature = "crawl")]
pub mod crawl;
mod page;
mod rpc;
pub mod urls;

use std::time::Duration;

pub use crate::bridge::Bridge;
pub use crate::bridge::DEFAULT_PORT;
pub use crate::bridge::token;
pub use crate::bridge::token_path;
pub use crate::chromium::Chromium;
pub use crate::chromium::find_binary;
pub use crate::page::Action;
pub use crate::page::Landing;
pub use crate::page::Link;
pub use crate::page::Page;
pub use crate::page::Reading;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no Chromium-family browser found; set DINO_BROWSER to its binary")]
    NoBrowser,
    #[error("the browser would not start: {0}")]
    Launch(String),
    #[error("the browser connection closed")]
    Closed,
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("the browser said: {0}")]
    Remote(String),
    #[error("navigation failed: {0}")]
    Navigation(String),
    #[error("{0}")]
    Page(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Whether the browser itself is gone, so it must be started again rather
    /// than the page retried on it.
    #[must_use]
    pub fn is_dead_browser(&self) -> bool {
        matches!(self, Self::Closed)
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// How long one CDP call may take. A navigation is bounded separately.
pub(crate) const CALL_LIMIT: Duration = Duration::from_secs(30);

/// Either kind of browser, so callers hold one type.
#[derive(Debug)]
pub enum Browser {
    Chromium(Box<Chromium>),
    Extension(Bridge),
}

impl Browser {
    /// # Errors
    ///
    /// Fails when the browser is gone or will not open a tab.
    pub async fn new_page(&self) -> Result<Page, Error> {
        match self {
            Self::Chromium(chromium) => chromium.new_page().await,
            Self::Extension(bridge) => bridge.new_page().await,
        }
    }

    #[must_use]
    pub fn alive(&self) -> bool {
        match self {
            Self::Chromium(chromium) => chromium.alive(),
            Self::Extension(bridge) => bridge.alive(),
        }
    }
}
