//! `dino plugin`: the install lifecycle, creating a bundle, and the index.

use std::io::IsTerminal as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

use super::Store;
use super::index;
use super::index::Location;
use super::index::Source;
use crate::render::status_block;
use crate::settings;
use crate::style;

#[derive(Debug, clap::Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    command: PluginCommand,
}

#[derive(Debug, clap::Subcommand)]
enum PluginCommand {
    /// Install from a directory, a git URL, or the index as name[@version].
    /// Shows what the plugin contributes and asks first.
    Install {
        source: String,
        /// The index to read a name from. Defaults to `plugin_index` in the
        /// user file.
        #[arg(long)]
        index: Option<String>,
        /// Install only a bundle with this digest. Without a terminal to ask
        /// at, this is the confirmation, and it is required.
        #[arg(long)]
        checksum: Option<String>,
    },
    /// Remove an installed plugin and its files.
    Uninstall { name: String },
    /// Show installed plugins and whether each will load.
    List,
    /// Load an installed plugin again from the next session.
    Enable { name: String },
    /// Stop loading an installed plugin, keeping its files.
    Disable { name: String },
    /// Write a new bundle to start from.
    New {
        name: String,
        /// Where to create it. Defaults to the current directory.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Check a bundle, and print what it contributes and its digest.
    Validate { dir: PathBuf },
    /// List the index, or the plugins in it matching a word.
    Search {
        query: Option<String>,
        #[arg(long)]
        index: Option<String>,
    },
    /// Check an index, and every bundle it names by directory.
    ValidateIndex { index: String },
}

/// Runs one `dino plugin` command.
///
/// # Errors
///
/// Returns an error when the command cannot do what it was asked.
pub fn run(args: PluginArgs) -> Result<ExitCode> {
    style::enable(false);
    let store = || Store::user().context("no home directory to keep plugins in");
    match args.command {
        PluginCommand::Install {
            source,
            index,
            checksum,
        } => install(
            &store()?,
            &source,
            index.as_deref(),
            checksum.as_deref(),
            std::io::stdin().is_terminal(),
        ),
        PluginCommand::Uninstall { name } => {
            let entry = store()?.uninstall(&name)?;
            println!("uninstalled {name} {}", entry.version);
            Ok(ExitCode::SUCCESS)
        }
        PluginCommand::List => list(&store()?),
        PluginCommand::Enable { name } => {
            let store = store()?;
            store.set_enabled(&name, true)?;
            let entry = &store.entries()?[&name];
            match store.check(&name, entry) {
                Ok(_) => println!("enabled {name}; it loads from the next session"),
                Err(why) => println!("enabled {name}, but it will not load: {why}"),
            }
            Ok(ExitCode::SUCCESS)
        }
        PluginCommand::Disable { name } => {
            store()?.set_enabled(&name, false)?;
            println!("disabled {name}; its files are kept");
            Ok(ExitCode::SUCCESS)
        }
        PluginCommand::New { name, dir } => {
            let parent = match dir {
                Some(dir) => dir,
                None => std::env::current_dir()?,
            };
            let created = super::scaffold(&parent, &name)?;
            println!(
                "created {}\nedit {}, then `dino plugin validate {}` and `dino plugin install {}`",
                created.display(),
                created.join(super::MANIFEST).display(),
                created.display(),
                created.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        PluginCommand::Validate { dir } => {
            let bundle = super::inspect(&dir)?;
            print!(
                "{}",
                status_block(&super::describe(&bundle, &dir.display().to_string()))
            );
            println!("valid");
            Ok(ExitCode::SUCCESS)
        }
        PluginCommand::Search { query, index } => search(query.as_deref(), index.as_deref()),
        PluginCommand::ValidateIndex { index } => validate_index(&Location::parse(&index)),
    }
}

/// What the argument to `install` names.
#[derive(Debug, PartialEq, Eq)]
enum Wanted {
    Dir(PathBuf),
    Git {
        url: String,
        rev: Option<String>,
    },
    Named {
        name: String,
        version: Option<String>,
    },
}

fn wanted(argument: &str) -> Wanted {
    if let Some(git) = argument.strip_prefix("git+") {
        let (url, rev) = match git.split_once('#') {
            Some((url, rev)) => (url.to_owned(), Some(rev.to_owned())),
            None => (git.to_owned(), None),
        };
        return Wanted::Git { url, rev };
    }
    if argument.contains("://")
        || argument.starts_with("git@")
        || Path::new(argument)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("git"))
    {
        return Wanted::Git {
            url: argument.to_owned(),
            rev: None,
        };
    }
    if argument.contains('/') || argument.starts_with('.') {
        return Wanted::Dir(PathBuf::from(argument));
    }
    match argument.split_once('@') {
        Some((name, version)) => Wanted::Named {
            name: name.to_owned(),
            version: Some(version.to_owned()),
        },
        None => Wanted::Named {
            name: argument.to_owned(),
            version: None,
        },
    }
}

/// A bundle on disk, removed afterwards when it was fetched for the install.
struct Fetched {
    dir: PathBuf,
    temporary: bool,
}

impl Drop for Fetched {
    fn drop(&mut self) {
        if self.temporary {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn fetch(source: Source) -> Result<Fetched> {
    match source {
        Source::Dir(dir) => Ok(Fetched {
            dir,
            temporary: false,
        }),
        Source::Git { url, rev } => clone(&url, rev.as_deref()),
    }
}

fn clone(url: &str, rev: Option<&str>) -> Result<Fetched> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let fetched = Fetched {
        dir: std::env::temp_dir().join(format!("dino-plugin-{}-{nanos}", std::process::id())),
        temporary: true,
    };
    let git = |arguments: &[&std::ffi::OsStr]| {
        std::process::Command::new("git")
            .args(arguments)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .status()
            .context("cannot run git")
    };
    let status = git(&[
        "clone".as_ref(),
        "--quiet".as_ref(),
        "--".as_ref(),
        url.as_ref(),
        fetched.dir.as_os_str(),
    ])?;
    if !status.success() {
        bail!("git could not clone {url}");
    }
    if let Some(rev) = rev {
        let status = git(&[
            "-C".as_ref(),
            fetched.dir.as_os_str(),
            "checkout".as_ref(),
            "--quiet".as_ref(),
            "--detach".as_ref(),
            rev.as_ref(),
        ])?;
        if !status.success() {
            bail!("{url} has no revision {rev}");
        }
    }
    Ok(fetched)
}

/// The index a flag names, else the user file's.
fn index_location(flag: Option<&str>) -> Result<Location> {
    if let Some(flag) = flag {
        return Ok(Location::parse(flag));
    }
    match settings::user_file()?.plugin_index {
        Some(index) => Ok(Location::parse(&index)),
        None => bail!(
            "no plugin index: pass --index, or set plugin_index in {}",
            settings::user_dir().map_or_else(
                || "your config".into(),
                |dir| dir.join("config.toml").display().to_string()
            )
        ),
    }
}

fn install(
    store: &Store,
    argument: &str,
    index_flag: Option<&str>,
    checksum: Option<&str>,
    terminal: bool,
) -> Result<ExitCode> {
    let (fetched, release, label) = match wanted(argument) {
        Wanted::Dir(dir) => {
            let dir = std::fs::canonicalize(&dir)
                .with_context(|| format!("no bundle at {}", dir.display()))?;
            let label = dir.display().to_string();
            (fetch(Source::Dir(dir))?, None, label)
        }
        Wanted::Git { url, rev } => {
            let label = match &rev {
                Some(rev) => format!("git+{url}#{rev}"),
                None => format!("git+{url}"),
            };
            (clone(&url, rev.as_deref())?, None, label)
        }
        Wanted::Named { name, version } => {
            let location = index_location(index_flag)?;
            let index = index::read(&location)?;
            let release = index::pick(&index, &name, version.as_deref())?.clone();
            let fetched = fetch(Source::of(&release, &location)?)?;
            let label = format!("{name}@{} in {location}", release.version);
            (fetched, Some((name, release)), label)
        }
    };
    let bundle = super::inspect(&fetched.dir)?;
    if let Some((name, release)) = &release {
        if bundle.digest != release.checksum {
            bail!(
                "{name}@{} hashes to {}, not the index's checksum {}; nothing was installed",
                release.version,
                bundle.digest,
                release.checksum
            );
        }
        if bundle.manifest.name != *name || bundle.manifest.version != release.version {
            bail!(
                "the index's {name}@{} is a bundle of {}@{}; nothing was installed",
                release.version,
                bundle.manifest.name,
                bundle.manifest.version
            );
        }
    }

    let name = bundle.manifest.name.clone();
    eprint!("{}", status_block(&super::describe(&bundle, &label)));
    if let Some(installed) = store.entries()?.get(&name) {
        eprintln!(
            "  {}",
            style::paint(
                style::WARN,
                &format!("replaces {name} {}, installed now", installed.version)
            )
        );
    }
    if let Some(checksum) = checksum
        && checksum != bundle.digest
    {
        bail!(
            "{name} hashes to {}, not {checksum}; nothing was installed",
            bundle.digest
        );
    }
    let confirmed = if terminal {
        crate::approve::confirm_plugin(&name)
    } else if checksum.is_some() {
        true
    } else {
        bail!(
            "no terminal to ask at. Read what it contributes above, then pass --checksum {} \
             to install exactly this",
            bundle.digest
        );
    };
    if !confirmed {
        println!("not installed");
        return Ok(ExitCode::FAILURE);
    }
    let replaced = store.install(&bundle, &label)?;
    let version = &bundle.manifest.version;
    match replaced {
        Some(old) => {
            println!(
                "installed {name} {version} in place of {old}; it loads from the next session"
            );
        }
        None => println!("installed {name} {version}; it loads from the next session"),
    }
    Ok(ExitCode::SUCCESS)
}

fn list(store: &Store) -> Result<ExitCode> {
    let entries = store.entries()?;
    if entries.is_empty() {
        println!("no plugins installed; `dino plugin search` lists the index");
        return Ok(ExitCode::SUCCESS);
    }
    for (name, entry) in &entries {
        let state = if entry.enabled {
            match store.check(name, entry) {
                Ok(active) => loads(&active),
                Err(why) => style::paint(style::WARN, &format!("not loaded: {why}")),
            }
        } else {
            style::paint(style::FAINT, "disabled")
        };
        println!("{name} {}  {state}", entry.version);
        println!(
            "  {}",
            style::paint(style::FAINT, &format!("from {}", entry.source))
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn loads(active: &super::Active) -> String {
    let skills = dino_core::skills::discover(&[active.skills_dir()]).len();
    let manifest = &active.manifest;
    style::paint(
        style::OK,
        &format!(
            "loads: skills {skills}, hooks {}, mcp {}{}",
            manifest.hooks.len(),
            manifest.mcp.len(),
            if manifest.instructions.is_some() {
                ", instructions"
            } else {
                ""
            }
        ),
    )
}

fn search(query: Option<&str>, index_flag: Option<&str>) -> Result<ExitCode> {
    let location = index_location(index_flag)?;
    let index = index::read(&location)?;
    let installed = Store::user()
        .map(|store| store.entries())
        .transpose()?
        .unwrap_or_default();
    let found = index::search(&index, query.unwrap_or(""));
    if found.is_empty() {
        println!("nothing in {location} matches");
        return Ok(ExitCode::SUCCESS);
    }
    for listing in found {
        let newest = index::newest(listing).map_or_else(
            || style::paint(style::WARN, "none for this dino"),
            |release| release.version.clone(),
        );
        let mark = installed
            .get(&listing.name)
            .map(|entry| format!("  (installed {})", entry.version))
            .unwrap_or_default();
        println!(
            "{}  {newest}  {}{mark}",
            style::paint(style::EMBER, &listing.name),
            listing.description.trim()
        );
        let versions: Vec<_> = listing
            .versions
            .iter()
            .map(|release| format!("{} ({})", release.version, release.engines))
            .collect();
        println!(
            "  {}",
            style::paint(style::FAINT, &format!("versions {}", versions.join(", ")))
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn validate_index(location: &Location) -> Result<ExitCode> {
    let index = index::read(location)?;
    let problems = index::problems(&index, location);
    if !problems.is_empty() {
        println!("{location} has {} problems:", problems.len());
        for problem in problems {
            println!("  - {problem}");
        }
        return Ok(ExitCode::FAILURE);
    }
    let releases: Vec<_> = index
        .plugins
        .iter()
        .flat_map(|listing| &listing.versions)
        .collect();
    let git = releases
        .iter()
        .filter(|release| release.source.starts_with("git+"))
        .count();
    println!(
        "valid: {} plugins, {} versions{}",
        index.plugins.len(),
        releases.len(),
        if git == 0 {
            String::new()
        } else {
            format!("; {git} git sources not fetched, so their checksums are unchecked")
        }
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::super::tests::Scratch;
    use super::super::tests::bundle;
    use super::*;

    #[test]
    fn an_install_argument_is_a_directory_a_repository_or_a_name() {
        assert_eq!(wanted("./tidy"), Wanted::Dir("./tidy".into()));
        assert_eq!(wanted("/abs/tidy"), Wanted::Dir("/abs/tidy".into()));
        assert_eq!(
            wanted("git+https://h/tidy.git#v1"),
            Wanted::Git {
                url: "https://h/tidy.git".into(),
                rev: Some("v1".into())
            }
        );
        assert_eq!(
            wanted("https://h/tidy.git"),
            Wanted::Git {
                url: "https://h/tidy.git".into(),
                rev: None
            }
        );
        assert_eq!(
            wanted("tidy@0.2.0"),
            Wanted::Named {
                name: "tidy".into(),
                version: Some("0.2.0".into())
            }
        );
        assert_eq!(
            wanted("tidy"),
            Wanted::Named {
                name: "tidy".into(),
                version: None
            }
        );
    }

    fn git(dir: &Path, arguments: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(arguments)
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?}");
    }

    #[test]
    fn a_bundle_cloned_from_a_repository_hashes_as_its_directory_does() {
        let scratch = Scratch::new("clone");
        let dir = bundle(&scratch.0, "tidy", "0.1.0");
        let expected = super::super::digest(&dir).unwrap();
        git(&dir, &["init", "--quiet"]);
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "--quiet", "-m", "tidy"]);
        git(&dir, &["tag", "v0.1.0"]);
        let url = format!("file://{}", dir.display());

        let fetched = clone(&url, Some("v0.1.0")).unwrap();
        let bundle = super::super::inspect(&fetched.dir).unwrap();

        assert_eq!(bundle.digest, expected);
        let path = fetched.dir.clone();
        drop(fetched);
        assert!(!path.exists());
        assert!(clone(&url, Some("v9")).is_err());
    }

    #[test]
    fn without_a_terminal_an_install_needs_the_checksum_it_will_get() {
        let scratch = Scratch::new("headless");
        let store = scratch.store();
        let dir = bundle(&scratch.0, "tidy", "0.1.0");
        let digest = super::super::digest(&dir).unwrap();
        let argument = dir.display().to_string();

        let refused = install(&store, &argument, None, None, false)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains(&format!("--checksum {digest}")),
            "{refused}"
        );
        let wrong = format!("sha256:{}", "0".repeat(64));
        assert!(install(&store, &argument, None, Some(&wrong), false).is_err());
        assert!(store.entries().unwrap().is_empty());

        assert_eq!(
            install(&store, &argument, None, Some(&digest), false).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(store.entries().unwrap()["tidy"].digest, digest);
    }

    #[test]
    fn installing_by_name_from_an_index_refuses_a_bundle_that_does_not_match_its_checksum() {
        let scratch = Scratch::new("by-name");
        let store = scratch.store();
        let dir = bundle(&scratch.0.join("bundles"), "tidy", "0.1.0");
        let digest = super::super::digest(&dir).unwrap();
        let write_index = |checksum: &str| {
            let path = scratch.0.join("index.json");
            std::fs::write(
                &path,
                serde_json::json!({"format": 1, "plugins": [{"name": "tidy", "description": "d",
                    "versions": [{"version": "0.1.0", "engines": "*", "source": "bundles/tidy", "checksum": checksum}]}]})
                .to_string(),
            )
            .unwrap();
            path.display().to_string()
        };

        let index = write_index(&format!("sha256:{}", "0".repeat(64)));
        let refused = install(&store, "tidy@0.1.0", Some(&index), Some(&digest), false)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("not the index's checksum"), "{refused}");
        assert!(store.entries().unwrap().is_empty());

        let index = write_index(&digest);
        install(&store, "tidy", Some(&index), Some(&digest), false).unwrap();
        assert_eq!(store.entries().unwrap()["tidy"].version, "0.1.0");
        assert!(
            store.entries().unwrap()["tidy"]
                .source
                .starts_with("tidy@0.1.0 in ")
        );
    }
}
