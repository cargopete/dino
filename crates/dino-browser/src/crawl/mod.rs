//! A crawl over real browser tabs, durable by construction. RFC-0022 section 4.

mod plain;
mod robots;
mod sitemap;
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
pub use self::store::File;
pub use self::store::Store;
use crate::Admit;
use crate::Bridge;
use crate::Browser;
use crate::Chromium;
use crate::Error;
use crate::Guard;
use crate::Page;
use crate::Reading;
use crate::urls;

/// One page, opened, settled and read, may take this long before it is a failure.
const VISIT_LIMIT: Duration = Duration::from_secs(90);
/// Never fetched: nothing in them a reader wants.
const MEDIA: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "svg", "ico", "bmp", "tif", "tiff", "mp3", "mp4", "m4a",
    "mov", "avi", "webm", "wav", "ogg", "exe", "dmg", "iso", "apk", "msi", "woff", "woff2", "ttf",
    "otf", "eot", "css", "js", "mjs", "map",
];
/// Saved to disk beside the crawl, when files are wanted.
const DOCUMENTS: &[&str] = &[
    "pdf", "xlsx", "xls", "xlsm", "csv", "tsv", "docx", "doc", "rtf", "pptx", "ppt", "odt", "ods",
    "odp", "zip", "gz", "tgz", "7z", "rar", "json", "txt", "epub",
];
const MAX_FILE: u64 = 200 * 1024 * 1024;
const MAX_PAGE: usize = 10 * 1024 * 1024;
/// Sitemaps read at the start of a crawl, and the URLs taken from them, at most.
const MAX_SITEMAPS: usize = 200;
const MAX_SITEMAP_URLS: usize = 100_000;

/// When a page is drawn in a browser rather than read from its HTML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Render {
    /// Only a page whose HTML is a shell a script fills in.
    #[default]
    Auto,
    Always,
    Never,
}

// Switches, each one flag on the command line, rather than a state machine.
#[allow(clippy::struct_excessive_bools)]
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
    pub render: Render,
    /// Save linked documents (PDFs, spreadsheets, archives) to disk.
    pub files: bool,
    /// Add the seeds' sitemaps to the frontier at the start.
    pub sitemaps: bool,
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

fn extension(url: &str) -> Option<String> {
    let path = Url::parse(url).ok()?.path().to_ascii_lowercase();
    let last = path.rsplit('/').next()?;
    last.rsplit_once('.')
        .map(|(_, extension)| extension.to_owned())
}

fn media_kind(url: &str) -> Option<&'static str> {
    let extension = extension(url)?;
    MEDIA.iter().copied().find(|kind| *kind == extension)
}

fn document_kind(url: &str) -> Option<&'static str> {
    let extension = extension(url)?;
    DOCUMENTS.iter().copied().find(|kind| *kind == extension)
}

/// Whether a response of this type is a document to save rather than a page to read.
fn document_type(content_type: &str) -> bool {
    let kind = content_type.split(';').next().unwrap_or_default().trim();
    kind == "application/pdf"
        || kind.starts_with("application/vnd.openxmlformats")
        || kind.starts_with("application/vnd.ms-")
        || kind.starts_with("application/vnd.oasis.opendocument")
        || matches!(kind, "application/msword" | "application/zip" | "application/gzip" | "application/json" | "application/rtf" | "application/epub+zip" | "application/octet-stream")
        // bnb.bg serves spreadsheets as text/xls; any text that is not HTML is kept as a file.
        || (kind.starts_with("text/") && kind != "text/html")
}

/// An extension for a saved file whose URL gives none.
fn type_extension(content_type: &str) -> Option<&'static str> {
    let kind = content_type.split(';').next().unwrap_or_default().trim();
    Some(match kind {
        "application/pdf" => "pdf",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.ms-excel" | "text/xls" | "application/xls" => "xls",
        "application/msword" => "doc",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.oasis.opendocument.spreadsheet" => "ods",
        "application/vnd.oasis.opendocument.text" => "odt",
        "text/csv" => "csv",
        "application/json" => "json",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "text/plain" => "txt",
        "application/xml" | "text/xml" => "xml",
        _ => return None,
    })
}

fn files_dir(db: &std::path::Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(".files");
    PathBuf::from(name)
}

/// A host that refuses this many pages in a row is given up whole.
const REFUSALS: u32 = 3;

/// A page that redirected to the site's own "not found" page, with a 200.
fn soft_404(requested: &str, landed: &str) -> bool {
    let path = |url: &str| {
        Url::parse(url)
            .map(|u| u.path().to_ascii_lowercase())
            .unwrap_or_default()
    };
    let (asked, got) = (path(requested), path(landed));
    let missing = |p: &str| {
        p.split('/').any(|part| part == "404")
            || ["not-found", "notfound", "page-not-found", "error404"]
                .iter()
                .any(|word| p.contains(word))
    };
    asked != got && missing(&got) && !missing(&asked)
}

/// What the guard said of a request, when the reply is its own rather than the site's.
fn guarded(response: &reqwest::Response) -> Option<&str> {
    response
        .headers()
        .get(crate::GUARD_HEADER)
        .and_then(|value| value.to_str().ok())
}

/// What a plain GET found.
enum Plain {
    Page { reading: Reading, shell: bool },
    File(File),
    NotAPage(String),
}

/// Starts the headless browser again when it dies, so no single crash ends a crawl.
#[derive(Debug)]
struct Pool {
    extension: Option<Bridge>,
    guard: Guard,
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
            Browser::Chromium(Box::new(Chromium::launch_guarded(Some(self.guard)).await?))
        });
        *current = Some(browser.clone());
        Ok(browser)
    }
}

struct Reach(Admit);

impl std::fmt::Debug for Reach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Reach")
    }
}

#[derive(Debug, Default)]
struct Hosts {
    next: Mutex<HashMap<String, Instant>>,
    /// Refusals in a row, per host.
    refusals: Mutex<HashMap<String, u32>>,
    /// Hosts that have answered at least once in this crawl.
    answered: Mutex<std::collections::HashSet<String>>,
    robots: tokio::sync::Mutex<HashMap<String, Arc<Robots>>>,
}

#[derive(Debug)]
pub struct Crawl {
    store: Store,
    scope: Scope,
    pool: Pool,
    hosts: Hosts,
    http: reqwest::Client,
    /// What the guard lets through, checked first so a refused page says why.
    admit: Reach,
    stop: Arc<AtomicBool>,
    /// Pages taken and not yet recorded, counted against `max_pages` so open tabs cannot overshoot it.
    open: Mutex<u64>,
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
        drawn: bool,
    },
    Saved {
        bytes: u64,
    },
    Skipped {
        why: String,
    },
    Failed {
        why: String,
        retry: bool,
    },
}

impl Crawl {
    /// Every request goes through a guard that refuses loopback, the LAN, a
    /// tailnet and link-local addresses, except a seed's own host and port.
    ///
    /// # Errors
    ///
    /// Fails when the crawl file cannot be opened or the guard cannot listen.
    pub async fn new(
        config: Config,
        stop: Arc<AtomicBool>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let store = Store::open(&config.db)?;
        let mut seeds = Vec::new();
        for seed in &config.seeds {
            let url = urls::normalise(seed).ok_or_else(|| format!("not a web address: {seed}"))?;
            store.add(&url, &urls::host(&url).unwrap_or_default(), 0, None)?;
            seeds.push(url);
        }
        // A resumed crawl has no seeds on its command line; they are its depth-0 rows.
        if seeds.is_empty() {
            seeds = store.seed_urls()?;
        }
        let mut hosts: Vec<String> = seeds.iter().filter_map(|url| urls::host(url)).collect();
        hosts.sort();
        hosts.dedup();
        let admit = crate::public_and(&seeds).await;
        let guard = Guard::start_admitting(admit.clone()).await?;
        // No limit on the whole body: bnb.bg builds an export for over 15 s before its first byte,
        // and a large file takes as long as it takes, provided bytes keep coming.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(90))
            .user_agent("dino-crawl (+https://github.com/cargopete/dino)")
            .proxy(reqwest::Proxy::all(format!("http://{}", guard.addr))?)
            .build()?;
        Ok(Self {
            store,
            pool: Pool {
                extension: config.extension.clone(),
                guard,
                current: tokio::sync::Mutex::new(None),
            },
            scope: Scope { hosts, config },
            hosts: Hosts::default(),
            http,
            admit: Reach(admit),
            stop,
            open: Mutex::new(0),
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
        if self.scope.config.sitemaps && !self.scope.config.seeds.is_empty() {
            let added = self.seed_sitemaps().await?;
            eprintln!("{added} URLs from sitemaps");
        }
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

    fn answered(&self, host: &str) -> bool {
        self.hosts
            .answered
            .lock()
            .expect("hosts lock")
            .contains(host)
    }

    fn heard_from(&self, url: &str) {
        if let Some(host) = urls::host(url) {
            self.hosts.answered.lock().expect("hosts lock").insert(host);
        }
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
            let response = self.http.get(format!("{origin}/robots.txt")).send().await;
            if response.as_ref().is_ok_and(|r| guarded(r).is_none()) {
                self.heard_from(&origin);
            }
            match response {
                Ok(response) if response.status().is_success() => {
                    robots = Some(Robots::parse(&response.text().await.unwrap_or_default()));
                    break;
                }
                Ok(response) if response.status().is_client_error() => {
                    robots = Some(Robots::default());
                    break;
                }
                // Unreachable is not flaky; the page's own retries will try the host again.
                Err(_) => break,
                Ok(_) => tokio::time::sleep(Duration::from_secs(2)).await,
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
            let max = self.scope.config.max_pages;
            if max.is_some_and(|max| counts.done >= max) {
                break;
            }
            let taken = {
                let mut open = self.open.lock().expect("open lock");
                if max.is_some_and(|max| counts.done + *open >= max) {
                    None
                } else {
                    let taken = self.store.take(|host| self.host_ready(host))?;
                    *open += u64::from(taken.is_some());
                    taken
                }
            };
            let Some((url, host, depth)) = taken else {
                if counts.queued + counts.taken + counts.waiting == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            };
            let result = self.visit_one(worker, &mut page, &url, &host, depth).await;
            *self.open.lock().expect("open lock") -= 1;
            result?;
        }
        if let Some(page) = page {
            let _ = tokio::time::timeout(Duration::from_secs(5), page.close()).await;
        }
        Ok(())
    }

    /// Checks robots.txt, waits the host's turn, then fetches and records one URL.
    async fn visit_one(
        &self,
        worker: usize,
        page: &mut Option<Page>,
        url: &str,
        host: &str,
        depth: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let robots = self.robots(url).await;
        let path = Url::parse(url)
            .map(|u| {
                u.query()
                    .map_or_else(|| u.path().to_owned(), |q| format!("{}?{q}", u.path()))
            })
            .unwrap_or_default();
        if !robots.allows(&path) {
            self.store.set_skipped(url, "disallowed by robots.txt")?;
            return Ok(());
        }
        let gap = robots
            .crawl_delay
            .map_or(self.scope.config.delay, |seconds| {
                self.scope
                    .config
                    .delay
                    .max(Duration::from_secs_f64(seconds.clamp(0.0, 30.0)))
            });
        self.reserve(host, gap);
        let started = Instant::now();
        let outcome = self.fetch(page, url, depth).await?;
        let took = started.elapsed().as_secs_f32();
        self.report(worker, url, host, took, outcome)
    }

    /// Records what became of one URL, says so, and gives up a host that keeps refusing.
    fn report(
        &self,
        worker: usize,
        url: &str,
        host: &str,
        took: f32,
        outcome: Outcome,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let counts = self.store.counts()?;
        let tally = format!(
            "{} kept, {} to go",
            counts.done,
            counts.queued + counts.waiting + counts.taken
        );
        if matches!(outcome, Outcome::Kept { .. } | Outcome::Saved { .. }) {
            self.hosts.refusals.lock().expect("hosts lock").remove(host);
        }
        match outcome {
            Outcome::Kept {
                status,
                links,
                duplicate,
                drawn,
            } => {
                let status = status.map_or_else(|| "---".to_owned(), |s| s.to_string());
                let same = if duplicate {
                    " (same text as a page already kept)"
                } else {
                    ""
                };
                let how = if drawn { " drawn" } else { "" };
                eprintln!(
                    "[{worker}] {status}{how} {url} {took:.1}s, {links} new links{same} · {tally}"
                );
            }
            Outcome::Saved { bytes } => {
                eprintln!(
                    "[{worker}] file {url} {took:.1}s, {} KB · {tally}",
                    bytes / 1024
                );
            }
            Outcome::Skipped { why } => {
                self.store.set_skipped(url, &why)?;
                eprintln!("[{worker}] skipped {url}: {why} · {tally}");
            }
            Outcome::Failed { why, retry } => {
                // A host that has never answered is most likely not there at all: one more go, not eleven minutes.
                let retries = match retry {
                    false => 0,
                    true if self.answered(host) => store::BACKOFF.len(),
                    true => 1,
                };
                let next = match self.store.fail(url, &why, retries)? {
                    Some(wait) => format!(", tried again in {wait}s"),
                    None if retry => ", given up".to_owned(),
                    None => String::new(),
                };
                eprintln!("[{worker}] failed {url} {took:.1}s: {why}{next} · {tally}");
                if why.starts_with("refused") {
                    let mut refusals = self.hosts.refusals.lock().expect("hosts lock");
                    let count = refusals.entry(host.to_owned()).or_default();
                    *count += 1;
                    if *count >= REFUSALS {
                        let dropped = self
                            .store
                            .skip_host(host, "the host refuses automated reading")?;
                        eprintln!(
                            "{host} refused {count} pages in a row; giving up its other {dropped}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// A plain GET first, the browser only for a page that needs it.
    async fn fetch(
        &self,
        page: &mut Option<Page>,
        url: &str,
        depth: u32,
    ) -> Result<Outcome, Box<dyn std::error::Error + Send + Sync>> {
        if let Err(why) = self.reachable(url).await {
            return Ok(Outcome::Failed { why, retry: false });
        }
        let render = self.scope.config.render;
        if render != Render::Always || document_kind(url).is_some() {
            match self.plain(url).await {
                Ok(Plain::File(file)) => {
                    self.store.keep_file(&file)?;
                    return Ok(Outcome::Saved { bytes: file.bytes });
                }
                Ok(Plain::NotAPage(why)) => return Ok(Outcome::Skipped { why }),
                Ok(Plain::Page { reading, shell }) => {
                    if !(shell && render == Render::Auto) {
                        return self.record(url, depth, &reading, false);
                    }
                }
                Err(outcome) => return Ok(outcome),
            }
        }
        match self.visit(page, url).await {
            Ok(reading) => {
                self.heard_from(url);
                self.record(url, depth, &reading, true)
            }
            Err(error) => {
                // A fresh tab next time, whatever went wrong with this one.
                if let Some(old) = page.take() {
                    let _ = tokio::time::timeout(Duration::from_secs(5), old.close()).await;
                }
                // A download aborts the navigation; read it plainly instead.
                if render == Render::Always
                    && error.to_string().contains("ERR_ABORTED")
                    && let Ok(Plain::File(file)) = self.plain(url).await
                {
                    self.store.keep_file(&file)?;
                    return Ok(Outcome::Saved { bytes: file.bytes });
                }
                Ok(Outcome::Failed {
                    why: error.to_string(),
                    retry: true,
                })
            }
        }
    }

    /// Whether the guard would let `url` through. It refuses anyway; this only says why, at once.
    async fn reachable(&self, url: &str) -> Result<(), String> {
        let Ok(parsed) = Url::parse(url) else {
            return Ok(());
        };
        let (Some(host), Some(port)) = (parsed.host_str(), parsed.port_or_known_default()) else {
            return Ok(());
        };
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        // A name that does not resolve fails as a request, and is retried as one.
        let Ok(addrs) = tokio::net::lookup_host((bare, port)).await else {
            return Ok(());
        };
        let addrs: Vec<_> = addrs.collect();
        if addrs.iter().all(|addr| (self.admit.0)(*addr)) {
            Ok(())
        } else {
            Err(format!("{host} is not a public address; not fetched"))
        }
    }

    async fn plain(&self, url: &str) -> Result<Plain, Outcome> {
        let failed = |why: String, retry: bool| Outcome::Failed { why, retry };
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| failed(format!("request failed: {error}"), true))?;
        match guarded(&response) {
            Some("not-public") => {
                return Err(failed(
                    "sent on to an address that is not public; not fetched".into(),
                    false,
                ));
            }
            Some(_) => {
                let why = response.text().await.unwrap_or_default();
                return Err(failed(format!("request failed: {why}"), true));
            }
            None => self.heard_from(url),
        }
        let status = response.status().as_u16();
        match status {
            401 | 403 => {
                return Err(failed(
                    format!("refused ({status}); not retried, see RFC-0022 section 5"),
                    false,
                ));
            }
            404 | 410 => return Err(failed(format!("gone ({status})"), false)),
            429 | 500..=599 => return Err(failed(format!("server said {status}"), true)),
            _ if status >= 400 => return Err(failed(format!("server said {status}"), false)),
            _ => {}
        }
        let final_url = response.url().clone();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("text/html")
            .to_ascii_lowercase();
        let html =
            content_type.starts_with("text/html") || content_type.starts_with("application/xhtml");
        if document_kind(url).is_some()
            || document_kind(final_url.as_str()).is_some()
            || (!html && document_type(&content_type))
        {
            if !self.scope.config.files {
                return Ok(Plain::NotAPage("a file, and files are off".into()));
            }
            return self
                .download(url, &final_url, &content_type, response)
                .await
                .map(Plain::File)
                .map_err(|why| failed(why, true));
        }
        if !html {
            return Ok(Plain::NotAPage(format!("not a page: {content_type}")));
        }
        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| failed(format!("cannot read the page: {error}"), true))?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_PAGE {
                break;
            }
        }
        let (reading, shell) = plain::read(&final_url, status, &String::from_utf8_lossy(&body));
        Ok(Plain::Page { reading, shell })
    }

    async fn download(
        &self,
        url: &str,
        final_url: &Url,
        content_type: &str,
        mut response: reqwest::Response,
    ) -> Result<File, String> {
        use tokio::io::AsyncWriteExt as _;
        let dir = files_dir(&self.scope.config.db);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
        let part = dir.join(format!(
            "{}.part",
            crate::hex(&sha2::Sha256::digest(url.as_bytes()))
        ));
        let mut out = tokio::fs::File::create(&part)
            .await
            .map_err(|e| e.to_string())?;
        let mut hasher = sha2::Sha256::new();
        let mut bytes = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("cannot read the file: {e}"))?
        {
            bytes += chunk.len() as u64;
            if bytes > MAX_FILE {
                drop(out);
                let _ = tokio::fs::remove_file(&part).await;
                return Err(format!("over {} MB", MAX_FILE / (1024 * 1024)));
            }
            hasher.update(&chunk);
            out.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        out.flush().await.map_err(|e| e.to_string())?;
        let sha256 = crate::hex(&hasher.finalize());
        let kind = document_kind(final_url.as_str())
            .or_else(|| document_kind(url))
            .or_else(|| type_extension(content_type))
            .unwrap_or("bin");
        let path = dir.join(format!("{}.{kind}", &sha256[..16]));
        tokio::fs::rename(&part, &path)
            .await
            .map_err(|e| e.to_string())?;
        Ok(File {
            url: url.to_owned(),
            final_url: final_url.to_string(),
            content_type: Some(content_type.to_owned()),
            bytes,
            sha256,
            path: path.display().to_string(),
        })
    }

    /// Every page the seeds' sitemaps list that is in scope, at depth 1.
    async fn seed_sitemaps(&self) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let mut queue: Vec<String> = Vec::new();
        for seed in &self.scope.config.seeds {
            let Ok(url) = Url::parse(seed) else { continue };
            let origin = url.origin().ascii_serialization();
            queue.extend(self.robots(seed).await.sitemaps.iter().cloned());
            queue.push(format!("{origin}/sitemap.xml"));
        }
        let (mut seen, mut read, mut added, mut listed) =
            (std::collections::HashSet::new(), 0, 0, 0);
        while let Some(map) = queue.pop() {
            if !seen.insert(map.clone()) || read >= MAX_SITEMAPS || listed >= MAX_SITEMAP_URLS {
                continue;
            }
            read += 1;
            let Ok(response) = self.http.get(&map).send().await else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            let Ok(bytes) = response.bytes().await else {
                continue;
            };
            match sitemap::parse(&sitemap::text(&bytes)) {
                sitemap::Sitemap::Index(maps) => queue.extend(maps),
                sitemap::Sitemap::Pages(pages) => {
                    for page in pages {
                        listed += 1;
                        let Some(next) = urls::normalise(&page) else {
                            continue;
                        };
                        if self.scope.admits(&next, 1)
                            && media_kind(&next).is_none()
                            && self.store.add(
                                &next,
                                &urls::host(&next).unwrap_or_default(),
                                1,
                                Some(&map),
                            )?
                        {
                            added += 1;
                        }
                    }
                }
            }
        }
        Ok(added)
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
        drawn: bool,
    ) -> Result<Outcome, Box<dyn std::error::Error + Send + Sync>> {
        if soft_404(url, &reading.url) {
            return Ok(Outcome::Failed {
                why: format!("gone (sent on to {})", reading.url),
                retry: false,
            });
        }
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
                if let Some(kind) = media_kind(&next) {
                    self.store
                        .skip(&next, &host, depth + 1, &format!("media: {kind}"))?;
                } else if let Some(kind) = document_kind(&next).filter(|_| !self.scope.config.files)
                {
                    self.store.skip(
                        &next,
                        &host,
                        depth + 1,
                        &format!("file, not saved: {kind}"),
                    )?;
                } else if self.store.add(&next, &host, depth + 1, Some(url))? {
                    added += 1;
                }
            }
        }
        Ok(Outcome::Kept {
            status: reading.status,
            links: added,
            duplicate,
            drawn,
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
            render: Render::Auto,
            files: true,
            sitemaps: true,
        };
        f(&mut config);
        Scope {
            hosts: vec!["bnb.bg".into()],
            config,
        }
    }

    /// A site on loopback: each page links to ten others, and answers after a pause.
    async fn site() -> String {
        use tokio::io::AsyncReadExt as _;
        use tokio::io::AsyncWriteExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut head = [0u8; 2048];
                    let n = stream.read(&mut head).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&head[..n]).into_owned();
                    let body = if request.starts_with("GET /robots.txt") {
                        String::new()
                    } else {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        let links = (0..10).fold(String::new(), |mut links, n| {
                            use std::fmt::Write as _;
                            let _ = write!(links, r#"<a href="/p{n}">Page {n}</a> "#);
                            links
                        });
                        let text = "<p>Plain words a reader would read here. </p>".repeat(20);
                        format!("<html><body>{text}{links}</body></html>")
                    };
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn open_tabs_do_not_carry_a_crawl_past_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let seed = site().await;
        let config = Config {
            db: dir.path().join("c.sqlite"),
            concurrency: 4,
            max_pages: Some(2),
            render: Render::Never,
            sitemaps: false,
            ..scope(|_| {}).config
        };
        let config = Config {
            seeds: vec![seed],
            ..config
        };
        let crawl = Arc::new(Crawl::new(config, Arc::default()).await.unwrap());
        let summary = crawl.run().await.unwrap();
        assert_eq!(summary.counts.done, 2, "{:?}", summary.counts);
    }

    #[tokio::test]
    async fn only_the_seeds_own_private_address_is_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let (seed, other) = (site().await, site().await);
        let config = Config {
            seeds: vec![seed.clone()],
            db: dir.path().join("c.sqlite"),
            depth: 0,
            any_host: true,
            render: Render::Never,
            sitemaps: false,
            ..scope(|_| {}).config
        };
        let crawl = Arc::new(Crawl::new(config, Arc::default()).await.unwrap());
        for url in [other.clone(), "http://169.254.169.254/".to_owned()] {
            crawl
                .store()
                .add(&url, &urls::host(&url).unwrap(), 0, None)
                .unwrap();
        }
        let started = Instant::now();
        let summary = crawl.clone().run().await.unwrap();
        assert_eq!(summary.counts.done, 1, "{:?}", summary.counts);
        let failures = crawl.store().failures(10).unwrap();
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(
            failures
                .iter()
                .all(|(_, why)| why.contains("not a public address")),
            "{failures:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "refused at once"
        );
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
        assert_eq!(document_kind("https://bnb.bg/a/b.XLSX?x=1"), Some("xlsx"));
        assert_eq!(document_kind("https://bnb.bg/a/b.html"), None);
        assert_eq!(document_kind("https://bnb.bg/"), None);
        assert_eq!(
            document_kind("https://bnb.bg/v1.2/page"),
            None,
            "a dot in a directory is not an extension"
        );
        assert_eq!(media_kind("https://bnb.bg/logo.PNG"), Some("png"));
    }

    #[test]
    fn a_redirect_to_the_sites_404_page_is_gone() {
        assert!(soft_404(
            "https://parliament.bg/bg/MP/3264",
            "https://parliament.bg/bg/404"
        ));
        assert!(soft_404(
            "https://a.test/x",
            "https://a.test/page-not-found?from=x"
        ));
        assert!(!soft_404("https://a.test/x", "https://a.test/x/"));
        assert!(!soft_404(
            "https://a.test/errors/404",
            "https://a.test/errors/404"
        ));
        assert!(!soft_404("https://a.test/x", "https://a.test/y/4040"));
    }

    #[test]
    fn a_response_type_can_say_it_is_a_document() {
        assert!(document_type("application/pdf"));
        assert!(document_type(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        ));
        assert!(!document_type("text/html; charset=utf-8"));
        assert!(!document_type("image/png"));
        assert!(document_type("text/xls;charset=utf-8"));
        assert_eq!(type_extension("text/xls;charset=utf-8"), Some("xls"));
        assert_eq!(type_extension("image/png"), None);
    }
}
