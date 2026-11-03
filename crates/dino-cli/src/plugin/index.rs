//! The plugin index: a JSON list of bundles, their versions and where each is
//! fetched from, with the digest it must hash to.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;

use super::check_name;
use super::engines_match;

pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub format: u32,
    pub plugins: Vec<Listing>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listing {
    pub name: String,
    pub description: String,
    /// A path beside the index or a URL, for a front end that draws one.
    pub icon: Option<String>,
    pub versions: Vec<Release>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub version: String,
    pub engines: String,
    /// A directory relative to the index, or `git+<url>` with an optional
    /// `#<rev>`.
    pub source: String,
    /// The bundle's digest, as `dino plugin validate` prints it.
    pub checksum: String,
}

/// Where an index was read from, which is what a relative source is relative to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    File(PathBuf),
    Url(String),
}

impl Location {
    pub fn parse(text: &str) -> Self {
        if text.starts_with("http://") || text.starts_with("https://") {
            Self::Url(text.to_owned())
        } else {
            Self::File(PathBuf::from(text))
        }
    }

    fn base(&self) -> Option<&Path> {
        match self {
            Self::File(path) => path.parent(),
            Self::Url(_) => None,
        }
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => write!(f, "{}", path.display()),
            Self::Url(url) => f.write_str(url),
        }
    }
}

/// Where a bundle comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Dir(PathBuf),
    Git { url: String, rev: Option<String> },
}

impl Source {
    /// A release's source, resolved against where its index was read.
    ///
    /// # Errors
    ///
    /// Returns an error for a directory named by an index that has no directory.
    pub fn of(release: &Release, location: &Location) -> Result<Self> {
        if let Some(git) = release.source.strip_prefix("git+") {
            let (url, rev) = match git.split_once('#') {
                Some((url, rev)) => (url, Some(rev.to_owned())),
                None => (git, None),
            };
            return Ok(Self::Git {
                url: url.to_owned(),
                rev,
            });
        }
        if release.source.contains("://") {
            bail!(
                "source {:?} is a URL; a git repository is written git+<url>",
                release.source
            );
        }
        match location.base() {
            Some(base) => Ok(Self::Dir(base.join(&release.source))),
            None => bail!(
                "source {:?} is a directory, which an index at a URL cannot name",
                release.source
            ),
        }
    }
}

/// Reads an index from a file or over HTTP(S).
///
/// # Errors
///
/// Returns an error when it cannot be fetched or is not an index.
pub fn read(location: &Location) -> Result<Index> {
    let text = match location {
        Location::File(path) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the plugin index {}", path.display()))?,
        Location::Url(url) => fetch(url)?,
    };
    let index: Index =
        serde_json::from_str(&text).with_context(|| format!("{location} is not a plugin index"))?;
    if index.format != FORMAT {
        bail!(
            "{location} is index format {}; this dino reads format {FORMAT}",
            index.format
        );
    }
    Ok(index)
}

fn fetch(url: &str) -> Result<String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            // The checksums arrive with the index, so it must not come over plain HTTP.
            let client = reqwest::Client::builder().https_only(true).build()?;
            let response = client
                .get(url)
                .send()
                .await
                .with_context(|| format!("cannot fetch {url}"))?;
            if !response.status().is_success() {
                bail!("{url} answered {}", response.status());
            }
            Ok(response.text().await?)
        })
}

/// Every problem with an index. Directory sources are checked whole: the bundle
/// must pass validation, name the same version and hash to the checksum. Git
/// sources are not fetched.
pub fn problems(index: &Index, location: &Location) -> Vec<String> {
    let mut problems = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for listing in &index.plugins {
        let name = &listing.name;
        if let Err(problem) = check_name(name) {
            problems.push(format!("name: {problem}"));
        }
        if !names.insert(name) {
            problems.push(format!("{name} is listed twice"));
        }
        if listing.description.trim().is_empty() {
            problems.push(format!("{name}: description is empty"));
        }
        if let (Some(icon), Some(base)) = (&listing.icon, location.base())
            && !icon.contains("://")
            && !base.join(icon).is_file()
        {
            problems.push(format!("{name}: icon {icon} does not exist"));
        }
        if listing.versions.is_empty() {
            problems.push(format!("{name} has no versions"));
        }
        let mut versions = std::collections::BTreeSet::new();
        for release in &listing.versions {
            let at = format!("{name}@{}", release.version);
            if semver::Version::parse(&release.version).is_err() {
                problems.push(format!("{at}: version is not semver"));
            }
            if !versions.insert(&release.version) {
                problems.push(format!("{at} is listed twice"));
            }
            if semver::VersionReq::parse(&release.engines).is_err() {
                problems.push(format!(
                    "{at}: engines {:?} is not a semver range",
                    release.engines
                ));
            }
            if !is_checksum(&release.checksum) {
                problems.push(format!(
                    "{at}: checksum must be sha256: and 64 lower-case hex digits"
                ));
            }
            match Source::of(release, location) {
                Err(error) => problems.push(format!("{at}: {error}")),
                Ok(Source::Git { .. }) => {}
                Ok(Source::Dir(dir)) => match super::inspect(&dir) {
                    Err(error) => problems.push(format!("{at}: {error}")),
                    Ok(bundle) => {
                        if bundle.manifest.name != *name
                            || bundle.manifest.version != release.version
                        {
                            problems.push(format!(
                                "{at}: the bundle is {}@{}",
                                bundle.manifest.name, bundle.manifest.version
                            ));
                        }
                        if bundle.digest != release.checksum {
                            problems.push(format!(
                                "{at}: the bundle hashes to {}, not the checksum",
                                bundle.digest
                            ));
                        }
                    }
                },
            }
        }
    }
    problems
}

fn is_checksum(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    })
}

/// The release `name@version` names, or for a bare name the newest made for
/// this Dino.
///
/// # Errors
///
/// Returns an error naming what the index does offer.
pub fn pick<'a>(index: &'a Index, name: &str, version: Option<&str>) -> Result<&'a Release> {
    let Some(listing) = index.plugins.iter().find(|listing| listing.name == name) else {
        bail!("the index has no plugin named {name:?}");
    };
    if let Some(version) = version {
        let Some(release) = listing
            .versions
            .iter()
            .find(|release| release.version == version)
        else {
            bail!(
                "the index has no {name}@{version}; it has {}",
                versions(listing)
            );
        };
        engines_match(&release.engines)
            .map_err(|problem| anyhow::anyhow!("{name}@{version}: {problem}"))?;
        return Ok(release);
    }
    newest(listing).with_context(|| {
        format!(
            "no version of {name} is made for this dino, {}; the index has {}",
            super::DINO_VERSION,
            versions(listing)
        )
    })
}

/// The newest release whose engines include this Dino.
pub fn newest(listing: &Listing) -> Option<&Release> {
    listing
        .versions
        .iter()
        .filter(|release| engines_match(&release.engines).is_ok())
        .filter_map(|release| Some((semver::Version::parse(&release.version).ok()?, release)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, release)| release)
}

fn versions(listing: &Listing) -> String {
    let all: Vec<_> = listing
        .versions
        .iter()
        .map(|release| format!("{} ({})", release.version, release.engines))
        .collect();
    all.join(", ")
}

/// The listings whose name or description holds `query`, ignoring case.
pub fn search<'a>(index: &'a Index, query: &str) -> Vec<&'a Listing> {
    let query = query.to_lowercase();
    index
        .plugins
        .iter()
        .filter(|listing| {
            listing.name.contains(&query) || listing.description.to_lowercase().contains(&query)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::Scratch;
    use super::super::tests::bundle;
    use super::*;

    fn index_with(scratch: &Scratch, checksum: &str) -> Location {
        let path = scratch.0.join("index.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "format": 1,
                "plugins": [{
                    "name": "tidy",
                    "description": "Tidies a crate.",
                    "versions": [
                        {"version": "0.1.0", "engines": ">=0.1", "source": "bundles/tidy", "checksum": checksum},
                        {"version": "9.0.0", "engines": ">=99", "source": "git+https://example.invalid/tidy.git#v9", "checksum": checksum}
                    ]
                }]
            })
            .to_string(),
        )
        .unwrap();
        Location::File(path)
    }

    #[test]
    fn a_local_index_with_the_right_checksum_has_no_problems() {
        let scratch = Scratch::new("index-good");
        let dir = bundle(&scratch.0.join("bundles"), "tidy", "0.1.0");
        let location = index_with(&scratch, &super::super::digest(&dir).unwrap());

        let index = read(&location).unwrap();

        assert_eq!(problems(&index, &location), Vec::<String>::new());
    }

    #[test]
    fn a_checksum_that_does_not_match_the_bundle_is_a_problem() {
        let scratch = Scratch::new("index-bad");
        bundle(&scratch.0.join("bundles"), "tidy", "0.1.0");
        let location = index_with(&scratch, &format!("sha256:{}", "0".repeat(64)));

        let found = problems(&read(&location).unwrap(), &location);

        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("not the checksum"), "{found:?}");
    }

    #[test]
    fn a_malformed_index_names_each_fault() {
        let index: Index = serde_json::from_value(serde_json::json!({
            "format": 1,
            "plugins": [
                {"name": "Tidy", "description": "", "versions": []},
                {"name": "dup", "description": "d", "versions": [
                    {"version": "x", "engines": "??", "source": "https://plain/url", "checksum": "md5:1"}
                ]},
                {"name": "dup", "description": "d", "versions": [
                    {"version": "1.0.0", "engines": "*", "source": "git+https://h/r.git", "checksum": format!("sha256:{}", "a".repeat(64))}
                ]}
            ]
        }))
        .unwrap();

        let found = problems(&index, &Location::Url("https://h/index.json".into()));

        for expected in [
            "name:",
            "Tidy: description is empty",
            "Tidy has no versions",
            "dup is listed twice",
            "dup@x: version is not semver",
            "dup@x: engines",
            "dup@x: checksum",
            "dup@x: source \"https://plain/url\" is a URL",
        ] {
            assert!(
                found.iter().any(|problem| problem.starts_with(expected)),
                "{expected}: {found:?}"
            );
        }
        assert_eq!(found.len(), 8, "{found:?}");
    }

    #[test]
    fn a_bare_name_picks_the_newest_version_made_for_this_dino() {
        let scratch = Scratch::new("index-pick");
        let location = index_with(&scratch, "sha256:x");
        let index = read(&location).unwrap();

        assert_eq!(pick(&index, "tidy", None).unwrap().version, "0.1.0");
        assert_eq!(
            pick(&index, "tidy", Some("0.1.0")).unwrap().version,
            "0.1.0"
        );
        let refused = pick(&index, "tidy", Some("9.0.0")).unwrap_err().to_string();
        assert!(refused.contains("does not include this dino"), "{refused}");
        assert!(
            pick(&index, "tidy", Some("0.2.0"))
                .unwrap_err()
                .to_string()
                .contains("0.1.0 (>=0.1)")
        );
        assert!(pick(&index, "untidy", None).is_err());
        assert_eq!(
            Source::of(&index.plugins[0].versions[1], &location).unwrap(),
            Source::Git {
                url: "https://example.invalid/tidy.git".into(),
                rev: Some("v9".into())
            }
        );
        assert_eq!(search(&index, "CRATE").len(), 1);
        assert!(search(&index, "lint").is_empty());
    }

    #[test]
    fn the_shipped_example_index_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/index.json");
        let location = Location::File(path);

        let index = read(&location).unwrap();

        assert_eq!(problems(&index, &location), Vec::<String>::new());
        assert!(!index.plugins.is_empty());
    }
}
