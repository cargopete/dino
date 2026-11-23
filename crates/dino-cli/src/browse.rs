//! `dino browse` and `dino crawl`: a real browser from the command line. RFC-0022.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use dino_browser::Bridge;
use dino_browser::Browser;
use dino_browser::Chromium;
use dino_browser::crawl::Config;
use dino_browser::crawl::Crawl;
use dino_browser::crawl::Render;
use dino_browser::crawl::Store;
use regex::Regex;

#[derive(Debug, Clone, clap::Args)]
pub struct Where {
    /// Drive your own browser through the Dino extension instead of a headless one.
    #[arg(long)]
    extension: bool,
    /// Port the extension dials in to.
    #[arg(long, default_value_t = dino_browser::DEFAULT_PORT)]
    port: u16,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, clap::Args)]
pub struct BrowseArgs {
    /// The page to open. Leave it out with --shared to read the tab you send from the extension.
    url: Option<String>,
    #[command(flatten)]
    browser: Where,
    /// Read the tab you send with the extension's "Send this tab to Dino".
    #[arg(long, requires = "extension", conflicts_with = "url")]
    shared: bool,
    /// Scroll until the page stops growing before reading it.
    #[arg(long)]
    scroll: bool,
    /// Print the whole reading as JSON: links, JSON-LD, meta and actions too.
    #[arg(long)]
    json: bool,
    /// Also save a PNG of the page here.
    #[arg(long, value_name = "PATH")]
    shot: Option<PathBuf>,
    /// Print the token the extension needs, and where it is kept, then exit.
    #[arg(long, exclusive = true)]
    token: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, clap::Args)]
pub struct CrawlArgs {
    /// Pages to start from. Leave them out to carry on a crawl already in --db.
    seeds: Vec<String>,
    /// The crawl's state and results. Running the same command again resumes it.
    #[arg(long, default_value = "crawl.sqlite")]
    db: PathBuf,
    #[command(flatten)]
    browser: Where,
    /// Tabs open at once.
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
    /// Least seconds between two visits to one host; robots.txt may ask for more.
    #[arg(long, default_value_t = 1.0)]
    delay: f64,
    /// Links followed from a seed.
    #[arg(long, default_value_t = 3)]
    depth: u32,
    /// Stop once this many pages are kept.
    #[arg(long)]
    max_pages: Option<u64>,
    /// Follow links off the seeds' hosts.
    #[arg(long)]
    any_host: bool,
    /// Only follow URLs starting with this. Repeatable.
    #[arg(long = "prefix")]
    prefixes: Vec<String>,
    /// Only follow URLs matching this regex. Repeatable.
    #[arg(long)]
    include: Vec<String>,
    /// Never follow URLs matching this regex. Repeatable.
    #[arg(long)]
    exclude: Vec<String>,
    /// Scroll each page until it stops growing, for listings.
    #[arg(long)]
    scroll: bool,
    /// When to draw a page in the browser: auto (only when its HTML is a
    /// shell a script fills in), always, or never.
    #[arg(long, value_enum, default_value_t = RenderArg::Auto)]
    render: RenderArg,
    /// Do not save linked PDFs, spreadsheets and archives.
    #[arg(long)]
    no_files: bool,
    /// Do not read the seeds' sitemaps at the start.
    #[arg(long)]
    no_sitemaps: bool,
    /// Write the kept pages as JSON lines to this path ("-" for stdout) and exit.
    #[arg(long, value_name = "PATH")]
    export: Option<PathBuf>,
    /// Print the crawl's counts and recent failures and exit.
    #[arg(long, conflicts_with = "export")]
    status: bool,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum RenderArg {
    Auto,
    Always,
    Never,
}

async fn bridge(port: u16) -> Result<Bridge> {
    let bridge = Bridge::listen(port)
        .await
        .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;
    eprintln!(
        "waiting for the Dino extension on 127.0.0.1:{port} (token in {})",
        dino_browser::token_path().display()
    );
    bridge.wait(Duration::from_secs(300)).await?;
    Ok(bridge)
}

pub async fn browse(args: BrowseArgs) -> Result<ExitCode> {
    if args.token {
        println!("{}", dino_browser::token()?);
        eprintln!("kept in {}", dino_browser::token_path().display());
        return Ok(ExitCode::SUCCESS);
    }
    let (browser, page) = if args.browser.extension {
        let bridge = bridge(args.browser.port).await?;
        let page = if args.shared {
            eprintln!("press \"Send this tab to Dino\" in the extension");
            bridge.shared_tab().await?
        } else {
            bridge.new_page().await?
        };
        (Browser::Extension(bridge), page)
    } else {
        let chromium = Chromium::launch().await?;
        let page = chromium.new_page().await?;
        (Browser::Chromium(Box::new(chromium)), page)
    };
    if let Some(url) = &args.url {
        let landing = page.goto(url).await?;
        if let Some(status) = landing.status.filter(|status| *status >= 400) {
            eprintln!("{} answered {status}", landing.url);
        }
    }
    if args.scroll {
        let grew = page.scroll_all(30).await?;
        eprintln!("scrolled; the page grew {grew} times");
    }
    let reading = page.read().await?;
    if let Some(path) = &args.shot {
        std::fs::write(path, page.screenshot(true).await?)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    let mut out = std::io::stdout().lock();
    if args.json {
        serde_json::to_writer_pretty(&mut out, &reading)?;
        writeln!(out)?;
    } else {
        writeln!(
            out,
            "# {}\n<{}>\n\n{}",
            reading.title, reading.url, reading.markdown
        )?;
        eprintln!(
            "{} links, {} actions, {} JSON-LD blocks",
            reading.links.len(),
            reading.actions.len(),
            reading.jsonld.len()
        );
    }
    // A shared tab is the person's own; leave it open.
    if !args.shared {
        let _ = page.close().await;
    }
    drop(browser);
    Ok(ExitCode::SUCCESS)
}

pub async fn crawl(args: CrawlArgs) -> Result<ExitCode> {
    if args.status || args.export.is_some() {
        let store =
            Store::open(&args.db).with_context(|| format!("cannot open {}", args.db.display()))?;
        if let Some(path) = &args.export {
            if path.as_os_str() == "-" {
                store
                    .export(&mut std::io::stdout().lock())
                    .map_err(|error| anyhow!(error))?;
            } else {
                let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
                store.export(&mut file).map_err(|error| anyhow!(error))?;
                file.flush()?;
                eprintln!("wrote {}", path.display());
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&store.counts()?)?);
            for (url, why) in store.failures(20)? {
                println!("failed {url}: {why}");
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    let compile = |patterns: &[String]| {
        patterns
            .iter()
            .map(|p| Regex::new(p).with_context(|| format!("bad regex {p}")))
            .collect::<Result<Vec<_>>>()
    };
    let extension = if args.browser.extension {
        Some(bridge(args.browser.port).await?)
    } else {
        None
    };
    let config = Config {
        seeds: args.seeds,
        db: args.db.clone(),
        concurrency: args.concurrency,
        delay: Duration::from_secs_f64(args.delay.max(0.0)),
        depth: args.depth,
        max_pages: args.max_pages,
        any_host: args.any_host,
        prefixes: args.prefixes,
        include: compile(&args.include)?,
        exclude: compile(&args.exclude)?,
        scroll: args.scroll,
        extension,
        render: match args.render {
            // Scrolling needs a page drawn in a browser.
            RenderArg::Auto if args.scroll => Render::Always,
            RenderArg::Auto => Render::Auto,
            RenderArg::Always => Render::Always,
            RenderArg::Never => Render::Never,
        },
        files: !args.no_files,
        sitemaps: !args.no_sitemaps,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let crawl = Arc::new(Crawl::new(config, stop.clone()).map_err(|error| anyhow!(error))?);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!(
                "stopping after the open pages; ctrl-c again to stop now (the crawl resumes either way)"
            );
            stop.store(true, Ordering::SeqCst);
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        }
    });
    let summary = crawl.run().await.map_err(|error| anyhow!(error))?;
    let counts = summary.counts;
    eprintln!(
        "{} kept ({} of them files), {} failed, {} skipped, {} still to go, in {}s; results in {}",
        counts.done,
        counts.files,
        counts.failed,
        counts.skipped,
        counts.queued + counts.waiting + counts.taken,
        summary.seconds,
        args.db.display()
    );
    Ok(ExitCode::SUCCESS)
}
