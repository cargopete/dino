//! A crawl over real browser tabs, durable by construction. RFC-0022 section 4.

mod robots;
mod store;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use regex::Regex;
use sha2::Digest as _;
use url::Url;

pub use self::robots::Robots;
pub use self::store::Counts;
pub use self::store::Store;
use crate::Bridge;
use crate::Browser;
use crate::Chromium;
use crate::Error;
use crate::Page;
use crate::Reading;
use crate::urls;

/// One page, opened, settled and read, may take this long before it is a failure.
const VISIT_LIMIT: Duration = Duration::from_secs(90);
const FILES: &[&str] = &[
    "pdf", "zip", "gz", "tgz", "rar", "7z", "jpg", "jpeg", "png", "gif", "webp", "svg", "ico",
    "mp3", "mp4", "mov", "avi", "webm", "exe", "dmg", "iso", "apk", "woff", "woff2", "ttf", "css",
    "js", "xlsx", "xls", "docx", "doc", "pptx", "csv",
];

#[derive(Debug, Clone)]
pub struct Config {
    pub seeds: Vec<String>,
    pub db: PathBuf,
    pub concurrency: usize,
    /// Least time between two visits to one host.
    pub delay: Duration,
    pub depth: u32,
    pub max_pages: Option<u64>,
    /// Follow links to any host, not only the seeds'.
    pub any_host: bool,
    pub prefixes: Vec<String>,
    pub include: Vec<Regex>,
    pub exclude: Vec<Regex>,
    /// Scroll each page until it stops growing before reading it.
    pub scroll: bool,
    pub extension: Option<Bridge>,
}

#[derive(Debug)]
struct Scope {
    hosts: Vec<String>,
    config: Config,
}

impl Scope {
    fn admits(&self, url: &str, depth: u32) -> bool {
        let config = &self.config;
        depth <= config.depth
            && (config.any_host || urls::host(url).is_some_and(|host| self.hosts.contains(&host)))
            && (config.prefixes.is_empty()
                || config.prefixes.iter().any(|prefix| url.starts_with(prefix)))
            && (config.include.is_empty() || config.include.iter().any(|re| re.is_match(url)))
            && !config.exclude.iter().any(|re| re.is_match(url))
    }
}

fn file_kind(url: &str) -> Option<&'static str> {
    let path = Url::parse(url).ok()?.path().to_ascii_lowercase();
    let extension = path.rsplit_once('.')?.1;
    FILES.iter().copied().find(|kind| *kind == extension)
}

/// Starts the headless browser again when it dies, so no single crash ends a crawl.
#[derive(Debug)]
struct Pool {
    extension: Option<Bridge>,
    current: tokio::sync::Mutex<Option<Arc<Browser>>>,
}

impl Pool {
    async fn browser(&self) -> Result<Arc<Browser>, Error> {
        let mut current = self.current.lock().await;
        if let Some(browser) = current.as_ref().filter(|browser| browser.alive()) {
            return Ok(browser.clone());
        }
        let browser = Arc::new(if let Some(bridge) = &self.extension {
            bridge.wait(Duration::from_secs(300)).await?;
            Browser::Extension(bridge.clone())
        } else {
            if current.is_some() {
                eprintln!("the browser died; starting it again");
            }
            Browser::Chromium(Box::new(Chromium::launch().await?))
        });
        *current = Some(browser.clone());
        Ok(browser)
    }
}

#[derive(Debug, Default)]
struct Hosts {
    next: Mutex<HashMap<String, Instant>>,
    robots: tokio::sync::Mutex<HashMap<String, Arc<Robots>>>,
}

#[derive(Debug)]
pub struct Crawl {
    store: Store,
    scope: Scope,
    pool: Pool,
    hosts: Hosts,
    http: reqwest::Client,
    stop: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Summary {
    pub counts: Counts,
    pub seconds: u64,
}

enum Outcome {
    Kept {
        status: Option<u16>,
        links: usize,
        duplicate: bool,
    },
    Failed {
        why: String,
        retry: bool,
    },
}

impl Crawl {
    /// # Errors
    ///
    /// Fails when the crawl file cannot be opened.
    pub fn new(
        config: Config,
        stop: Arc<AtomicBool>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let store = Store::open(&config.db)?;
        let mut hosts = Vec::new();
        for seed in &config.seeds {
            let url = urls::normalise(seed).ok_or_else(|| format!("not a web address: {seed}"))?;
            let host = urls::host(&url).unwrap_or_default();
            store.add(&url, &host, 0, None)?;
            hosts.push(host);
        }
        // A resumed crawl has no seeds on its command line; its hosts are the depth-0 rows.
        if hosts.is_empty() {
            hosts = store.seed_hosts()?;
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("dino-crawl (+https://github.com/cargopete/dino)")
            .build()?;
        Ok(Self {
            store,
            pool: Pool {
                extension: config.extension.clone(),
                current: tokio::sync::Mutex::new(None),
            },
            scope: Scope { hosts, config },
            hosts: Hosts::default(),
            http,
            stop,
        })
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Runs until the frontier is empty, `max_pages` are kept, or `stop` is set.
    ///
    /// # Errors
    ///
    /// Fails only when the crawl file does; a page or browser failure is recorded and survived.
    pub async fn run(self: Arc<Self>) -> Result<Summary, Box<dyn std::error::Error + Send + Sync>> {
        let started = Instant::now();
        let workers: Vec<_> = (0..self.scope.config.concurrency.max(1))
            .map(|worker| {
                let crawl = self.clone();
                tokio::spawn(async move { crawl.work(worker).await })
            })
            .collect();
        for worker in workers {
            worker.await??;
        }
        Ok(Summary {
            counts: self.store.counts()?,
            seconds: started.elapsed().as_secs(),
        })
    }

    fn host_ready(&self, host: &str) -> bool {
        self.hosts
            .next
            .lock()
            .expect("hosts lock")
            .get(host)
            .is_none_or(|at| *at <= Instant::now())
    }

    fn reserve(&self, host: &str, gap: Duration) {
        self.hosts
            .next
            .lock()
            .expect("hosts lock")
            .insert(host.to_owned(), Instant::now() + gap);
    }

    async fn robots(&self, url: &str) -> Arc<Robots> {
        let Ok(parsed) = Url::parse(url) else {
            return Arc::default();
        };
        let origin = parsed.origin().ascii_serialization();
        let mut cache = self.hosts.robots.lock().await;
        if let Some(robots) = cache.get(&origin) {
            return robots.clone();
        }
        let mut robots = None;
        for _ in 0..2 {
            match self.http.get(format!("{origin}/robots.txt")).send().await {
                Ok(response) if response.status().is_success() => {
                    robots = Some(Robots::parse(&response.text().await.unwrap_or_default()));
                    break;
                }
                Ok(response) if response.status().is_client_error() => {
                    robots = Some(Robots::default());
                    break;
                }
                Ok(_) | Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
        let robots = Arc::new(robots.unwrap_or_else(|| {
            eprintln!(
                "{origin}/robots.txt could not be read; going ahead as though it allowed everything"
            );
            Robots::default()
        }));
        cache.insert(origin, robots.clone());
        robots
    }

    async fn work(&self, worker: usize) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut page: Option<Page> = None;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            let counts = self.store.counts()?;
            if self
                .scope
                .config
                .max_pages
                .is_some_and(|max| counts.done >= max)
            {
                break;
            }
            let Some((url, host, depth)) = self.store.take(|host| self.host_ready(host))? else {
                if counts.queued + counts.taken + counts.waiting == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            };
            let robots = self.robots(&url).await;
            let path = Url::parse(&url)
                .map(|u| {
                    u.query()
                        .map_or_else(|| u.path().to_owned(), |q| format!("{}?{q}", u.path()))
                })
                .unwrap_or_default();
            if !robots.allows(&path) {
                self.store.set_skipped(&url, "disallowed by robots.txt")?;
                continue;
            }
            let gap = robots
                .crawl_delay
                .map_or(self.scope.config.delay, |seconds| {
                    self.scope
                        .config
                        .delay
                        .max(Duration::from_secs_f64(seconds.clamp(0.0, 30.0)))
                });
            self.reserve(&host, gap);

            let started = Instant::now();
            let outcome = match self.visit(&mut page, &url).await {
                Ok(reading) => self.record(&url, depth, &reading)?,
                Err(error) => {
                    // A fresh tab next time, whatever went wrong with this one.
                    if let Some(old) = page.take() {
                        let _ = tokio::time::timeout(Duration::from_secs(5), old.close()).await;
                    }
                    Outcome::Failed {
                        why: error.to_string(),
                        retry: true,
                    }
                }
            };
            let took = started.elapsed().as_secs_f32();
            let counts = self.store.counts()?;
            let tally = format!(
                "{} kept, {} to go",
                counts.done,
                counts.queued + counts.waiting + counts.taken
            );
            match outcome {
                Outcome::Kept {
                    status,
                    links,
                    duplicate,
                } => {
                    let status = status.map_or_else(|| "---".to_owned(), |s| s.to_string());
                    let same = if duplicate {
                        " (same text as a page already kept)"
                    } else {
                        ""
                    };
                    eprintln!(
                        "[{worker}] {status} {url} {took:.1}s, {links} new links{same} · {tally}"
                    );
                }
                Outcome::Failed { why, retry } => {
                    self.store.fail(&url, &why, retry)?;
                    eprintln!("[{worker}] failed {url} {took:.1}s: {why} · {tally}");
                }
            }
        }
        if let Some(page) = page {
            let _ = tokio::time::timeout(Duration::from_secs(5), page.close()).await;
        }
        Ok(())
    }

    async fn visit(&self, page: &mut Option<Page>, url: &str) -> Result<Reading, Error> {
        if page.is_none() {
            *page = Some(self.pool.browser().await?.new_page().await?);
        }
        let tab = page.as_ref().expect("page just made");
        let run = async {
            tab.goto(url).await?;
            if self.scope.config.scroll {
                tab.scroll_all(30).await?;
            }
            tab.read().await
        };
        tokio::time::timeout(VISIT_LIMIT, run)
            .await
            .map_err(|_| Error::Timeout(format!("{url} after {}s", VISIT_LIMIT.as_secs())))?
    }

    fn record(
        &self,
        url: &str,
        depth: u32,
        reading: &Reading,
    ) -> Result<Outcome, Box<dyn std::error::Error + Send + Sync>> {
        match reading.status {
            Some(status @ (401 | 403)) => {
                return Ok(Outcome::Failed {
                    why: format!("refused ({status}); not retried, see RFC-0022 section 5"),
                    retry: false,
                });
            }
            Some(status @ (404 | 410)) => {
                return Ok(Outcome::Failed {
                    why: format!("gone ({status})"),
                    retry: false,
                });
            }
            Some(status @ (429 | 500..=599)) => {
                return Ok(Outcome::Failed {
                    why: format!("server said {status}"),
                    retry: true,
                });
            }
            _ => {}
        }
        let hash = crate::hex(&sha2::Sha256::digest(reading.markdown.as_bytes()));
        let duplicate = self.store.keep(url, reading, &hash)?.is_some();
        let mut added = 0;
        if !duplicate {
            for link in &reading.links {
                let Some(next) = urls::normalise(&link.href) else {
                    continue;
                };
                if !self.scope.admits(&next, depth + 1) {
                    continue;
                }
                let host = urls::host(&next).unwrap_or_default();
                if let Some(kind) = file_kind(&next) {
                    self.store
                        .skip(&next, &host, depth + 1, &format!("file: {kind}"))?;
                } else if self.store.add(&next, &host, depth + 1, Some(url))? {
                    added += 1;
                }
            }
        }
        Ok(Outcome::Kept {
            status: reading.status,
            links: added,
            duplicate,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(f: impl FnOnce(&mut Config)) -> Scope {
        let mut config = Config {
            seeds: vec!["https://www.bnb.bg/".into()],
            db: PathBuf::new(),
            concurrency: 1,
            delay: Duration::ZERO,
            depth: 2,
            max_pages: None,
            any_host: false,
            prefixes: vec![],
            include: vec![],
            exclude: vec![],
            scroll: false,
            extension: None,
        };
        f(&mut config);
        Scope {
            hosts: vec!["bnb.bg".into()],
            config,
        }
    }

    #[test]
    fn the_seed_host_and_depth_bound_a_crawl() {
        let s = scope(|_| {});
        assert!(s.admits("https://bnb.bg/x", 1));
        assert!(s.admits("https://www.bnb.bg/x", 2));
        assert!(!s.admits("https://bnb.bg/x", 3));
        assert!(!s.admits("https://example.com/", 1));
    }

    #[test]
    fn prefix_include_and_exclude_narrow_it() {
        let s = scope(|c| {
            c.prefixes = vec!["https://bnb.bg/Statistics".into()];
            c.exclude = vec![Regex::new("/print/").unwrap()];
        });
        assert!(s.admits("https://bnb.bg/Statistics/a", 1));
        assert!(!s.admits("https://bnb.bg/News", 1));
        assert!(!s.admits("https://bnb.bg/Statistics/print/a", 1));
    }

    #[test]
    fn files_are_recognised_by_extension() {
        assert_eq!(file_kind("https://bnb.bg/a/b.XLSX?x=1"), Some("xlsx"));
        assert_eq!(file_kind("https://bnb.bg/a/b.html"), None);
        assert_eq!(file_kind("https://bnb.bg/"), None);
    }
}
