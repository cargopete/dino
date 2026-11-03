//! Plugins: bundles of skills, hooks, MCP servers and instructions.
//!
//! A bundle is a directory with a `dino-plugin.toml`. Its files are copied under
//! the data directory, which a sandboxed process may read but not write, so its
//! hooks and servers can run their own scripts. What was installed, enabled and
//! approved is kept in the user config directory, which a sandboxed process may
//! neither read nor write, and a bundle that no longer hashes to what the person
//! approved is not loaded. Nothing in a workspace is consulted.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;

use crate::settings;
use crate::settings::HookConfig;
use crate::settings::HookEvent;
use crate::settings::McpConfig;
use crate::style;

pub mod command;
pub mod index;

pub const MANIFEST: &str = "dino-plugin.toml";
/// The version a bundle's `engines` range is checked against.
pub const DINO_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_FILES: usize = 1_000;
const MAX_BYTES: u64 = 20 * 1_024 * 1_024;
const MAX_INSTRUCTIONS_BYTES: u64 = 32 * 1_024;
const MAX_DESCRIPTION_BYTES: usize = 400;

/// The skill that teaches a model to write, check and install a plugin.
pub const PLUGIN_DEV_SKILL: &str = include_str!("plugin-dev.md");

/// The manifest as written. Every field a person could mistype is checked by
/// [`inspect`] rather than here, so that one run names every problem.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub description: String,
    /// The Dino versions this works with, as a semver range such as `>=0.1, <0.3`.
    pub engines: String,
    /// A file in the bundle read into every session's instructions.
    pub instructions: Option<PathBuf>,
    #[serde(default)]
    pub hooks: Vec<HookConfig>,
    #[serde(default)]
    pub mcp: BTreeMap<String, McpConfig>,
}

/// A bundle that passed every check.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub skills: Vec<dino_core::skills::Skill>,
    pub digest: String,
}

/// Reads and checks the bundle at `dir`, naming every problem at once.
///
/// # Errors
///
/// Returns one error listing each problem on its own line.
pub fn inspect(dir: &Path) -> Result<Bundle> {
    let path = dir.join(MANIFEST);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("{} is not a plugin: cannot read {MANIFEST}", dir.display()))?;
    let manifest: Manifest = toml::from_str(&text)
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))?;
    let mut problems = Vec::new();
    let files = match walk(dir) {
        Ok(files) => files,
        Err(error) => {
            problems.push(error.to_string());
            Vec::new()
        }
    };

    if let Err(problem) = check_name(&manifest.name) {
        problems.push(format!("name: {problem}"));
    }
    if let Err(error) = semver::Version::parse(&manifest.version) {
        problems.push(format!(
            "version {:?} is not a semver version: {error}",
            manifest.version
        ));
    }
    if manifest.description.trim().is_empty() {
        problems.push("description is empty".into());
    } else if manifest.description.len() > MAX_DESCRIPTION_BYTES {
        problems.push(format!("description is over {MAX_DESCRIPTION_BYTES} bytes"));
    }
    if let Err(problem) = engines_match(&manifest.engines) {
        problems.push(problem);
    }
    if let Some(problem) = manifest
        .instructions
        .as_ref()
        .and_then(|file| check_instructions(dir, file))
    {
        problems.push(problem);
    }

    let skills_dir = dir.join("skills");
    let skills = dino_core::skills::discover(std::slice::from_ref(&skills_dir));
    if let Ok(entries) = std::fs::read_dir(&skills_dir) {
        for entry in entries.filter_map(Result::ok) {
            let folder = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir()
                && !skills
                    .iter()
                    .any(|skill| skill.path.parent() == Some(entry.path().as_path()))
            {
                problems.push(format!(
                    "skills/{folder} has no SKILL.md with a description, so it would not load"
                ));
            }
        }
    }

    for (index, hook) in manifest.hooks.iter().enumerate() {
        if let Err(problem) = check_command(dir, &hook.command) {
            problems.push(format!("hooks[{index}]: {problem}"));
        }
    }
    for (name, server) in &manifest.mcp {
        if let Err(problem) = check_name(name) {
            problems.push(format!("mcp.{name}: {problem}"));
        }
        if let Err(problem) = check_command(dir, &server.command) {
            problems.push(format!("mcp.{name}: {problem}"));
        }
    }
    if skills.is_empty()
        && manifest.hooks.is_empty()
        && manifest.mcp.is_empty()
        && manifest.instructions.is_none()
    {
        problems.push("contributes nothing: no skills, hooks, mcp or instructions".into());
    }

    if !problems.is_empty() {
        bail!(
            "{} has {} problem{}:\n  - {}",
            dir.display(),
            problems.len(),
            if problems.len() == 1 { "" } else { "s" },
            problems.join("\n  - ")
        );
    }
    Ok(Bundle {
        dir: dir.to_path_buf(),
        manifest,
        skills,
        digest: digest_of(&files)?,
    })
}

/// Lower-case letters, digits and hyphens, starting with a letter: a name that
/// is also a safe directory name.
pub fn check_name(name: &str) -> std::result::Result<(), String> {
    let valid = name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{name:?} must be lower-case letters, digits and hyphens, starting with a letter"
        ))
    }
}

/// Whether this Dino falls in `engines`.
pub fn engines_match(engines: &str) -> std::result::Result<(), String> {
    let range = semver::VersionReq::parse(engines)
        .map_err(|error| format!("engines {engines:?} is not a semver range: {error}"))?;
    let current = semver::Version::parse(DINO_VERSION).expect("the crate version is semver");
    if range.matches(&current) {
        Ok(())
    } else {
        Err(format!(
            "engines {engines:?} does not include this dino, {DINO_VERSION}"
        ))
    }
}

fn inside(dir: &Path, relative: &Path) -> Option<PathBuf> {
    let plain = relative
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)));
    (plain && relative.components().next().is_some()).then(|| dir.join(relative))
}

fn check_instructions(dir: &Path, file: &Path) -> Option<String> {
    let shown = file.display();
    let Some(path) = inside(dir, file) else {
        return Some(format!("instructions {shown} is outside the bundle"));
    };
    match std::fs::metadata(&path) {
        Ok(meta) if meta.len() > MAX_INSTRUCTIONS_BYTES => Some(format!(
            "instructions {shown} is over {} KiB",
            MAX_INSTRUCTIONS_BYTES / 1_024
        )),
        Ok(_) if std::fs::read_to_string(&path).is_err() => {
            Some(format!("instructions {shown} is not UTF-8 text"))
        }
        Ok(_) => None,
        Err(_) => Some(format!("instructions {shown} does not exist")),
    }
}

fn check_command(dir: &Path, command: &[String]) -> std::result::Result<(), String> {
    let Some(program) = command.first() else {
        return Err("command is empty".into());
    };
    if let Some(relative) = program.strip_prefix("{plugin}/") {
        let Some(path) = inside(dir, Path::new(relative)) else {
            return Err(format!("{program} is outside the bundle"));
        };
        if !path.is_file() {
            return Err(format!("{program} names a file the bundle does not have"));
        }
    }
    Ok(())
}

/// Every file in the bundle, in a fixed order, without `.git`. A symbolic link
/// is refused: copied, it would install whatever it pointed at.
fn walk(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = std::fs::read_dir(&current)
            .with_context(|| format!("cannot read {}", current.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            let relative = path
                .strip_prefix(dir)
                .expect("walked beneath the bundle")
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if kind.is_symlink() {
                bail!("{relative} is a symbolic link, which a bundle may not hold");
            } else if kind.is_dir() {
                if entry.file_name() != ".git" {
                    pending.push(path);
                }
            } else {
                total += entry.metadata()?.len();
                files.push((relative, path));
            }
        }
        if files.len() > MAX_FILES || total > MAX_BYTES {
            bail!(
                "the bundle is over {MAX_FILES} files or {} MiB",
                MAX_BYTES / 1_024 / 1_024
            );
        }
    }
    files.sort();
    Ok(files)
}

/// What a bundle is approved, and an index checksum written, by: every path,
/// whether it is executable, and every byte.
///
/// # Errors
///
/// Returns an error when a file cannot be read or the bundle oversteps a limit.
pub fn digest(dir: &Path) -> Result<String> {
    digest_of(&walk(dir)?)
}

fn digest_of(files: &[(String, PathBuf)]) -> Result<String> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    for (relative, path) in files {
        let bytes = std::fs::read(path).with_context(|| format!("cannot read {relative}"))?;
        hasher.update(relative.as_bytes());
        hasher.update([0, u8::from(executable(path))]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    let mut encoded = String::from("sha256:");
    for byte in hasher.finalize() {
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(_path: &Path) -> bool {
    false
}

/// What a bundle contributes, as the person is shown it before installing.
pub fn describe(bundle: &Bundle, source: &str) -> Vec<(&'static str, String)> {
    let manifest = &bundle.manifest;
    let mut rows = vec![
        ("plugin", format!("{} {}", manifest.name, manifest.version)),
        ("about", manifest.description.trim().to_owned()),
        ("from", source.to_owned()),
        ("digest", bundle.digest.clone()),
        ("engines", manifest.engines.clone()),
    ];
    if !bundle.skills.is_empty() {
        let names: Vec<_> = bundle
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        rows.push(("skills", names.join(", ")));
    }
    if let Some(file) = &manifest.instructions {
        rows.push((
            "instructions",
            format!("{} (read into every session's prompt)", file.display()),
        ));
    }
    for hook in &manifest.hooks {
        let only = hook
            .extension
            .as_ref()
            .map(|extension| format!(" on .{extension}"))
            .unwrap_or_default();
        rows.push((
            "hook",
            format!(
                "{}{only}: {}",
                event_name(hook.event),
                hook.command.join(" ")
            ),
        ));
    }
    for (name, server) in &manifest.mcp {
        let confinement = if server.sandbox {
            "sandboxed"
        } else {
            "UNSANDBOXED"
        };
        let mut line = format!("{name}: {}  ({confinement})", server.command.join(" "));
        if !server.pass_env.is_empty() {
            let _ = write!(line, ", sees {}", server.pass_env.join(", "));
        }
        rows.push(("mcp", line));
    }
    if runs_code(manifest) {
        rows.push((
            "runs code",
            "yes: its hooks and servers run as you in every session".into(),
        ));
    }
    rows
}

fn runs_code(manifest: &Manifest) -> bool {
    !manifest.hooks.is_empty() || !manifest.mcp.is_empty()
}

fn event_name(event: HookEvent) -> &'static str {
    match event {
        HookEvent::AfterEdit => "after_edit",
        HookEvent::BeforeEndTurn => "before_end_turn",
        HookEvent::SessionStart => "session_start",
        HookEvent::TaskEnd => "task_end",
    }
}

/// Where bundles are kept and where what was approved is recorded.
#[derive(Debug, Clone)]
pub struct Store {
    pub bundles: PathBuf,
    pub ledger: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    #[serde(default)]
    plugin: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub version: String,
    /// What the person approved. A bundle that no longer hashes to it is not
    /// loaded.
    pub digest: String,
    pub enabled: bool,
    pub source: String,
}

impl Store {
    /// The person's own, or `None` without a home directory.
    pub fn user() -> Option<Self> {
        Some(Self {
            bundles: settings::data_dir()?.join("plugins"),
            ledger: settings::user_dir()?.join("plugins.toml"),
        })
    }

    fn read(&self) -> Result<Ledger> {
        match std::fs::read_to_string(&self.ledger) {
            Ok(text) => toml::from_str(&text)
                .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", self.ledger.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
            Err(error) => Err(error.into()),
        }
    }

    fn write(&self, ledger: &Ledger) -> Result<()> {
        if let Some(dir) = self.ledger.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.ledger, toml::to_string(ledger)?)?;
        Ok(())
    }

    /// Every installed plugin, by name.
    ///
    /// # Errors
    ///
    /// Returns an error when the ledger cannot be read.
    pub fn entries(&self) -> Result<BTreeMap<String, Entry>> {
        Ok(self.read()?.plugin)
    }

    /// Copies a checked bundle in, enabled, replacing any version already
    /// installed. Returns the version it replaced.
    ///
    /// # Errors
    ///
    /// Returns an error when a file cannot be copied, or the copy does not hash
    /// to what was checked, as when the source changed in between.
    pub fn install(&self, bundle: &Bundle, source: &str) -> Result<Option<String>> {
        let name = &bundle.manifest.name;
        std::fs::create_dir_all(&self.bundles)?;
        let partial = self.bundles.join(format!(".{name}.partial"));
        let _ = std::fs::remove_dir_all(&partial);
        for (relative, path) in walk(&bundle.dir)? {
            let target = partial.join(&relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&path, &target).with_context(|| format!("cannot copy {relative}"))?;
        }
        let copied = digest(&partial)?;
        if copied != bundle.digest {
            let _ = std::fs::remove_dir_all(&partial);
            bail!("{name} changed while it was being installed; nothing was installed");
        }
        let installed = self.bundles.join(name);
        if installed.exists() {
            std::fs::remove_dir_all(&installed)?;
        }
        std::fs::rename(&partial, &installed)?;

        let mut ledger = self.read()?;
        let replaced = ledger.plugin.insert(
            name.clone(),
            Entry {
                version: bundle.manifest.version.clone(),
                digest: bundle.digest.clone(),
                enabled: true,
                source: source.to_owned(),
            },
        );
        self.write(&ledger)?;
        Ok(replaced.map(|entry| entry.version))
    }

    /// Removes a plugin's files and its record.
    ///
    /// # Errors
    ///
    /// Returns an error when no such plugin is installed.
    pub fn uninstall(&self, name: &str) -> Result<Entry> {
        let mut ledger = self.read()?;
        let Some(entry) = ledger.plugin.remove(name) else {
            bail!("no plugin named {name:?} is installed");
        };
        let dir = self.bundles.join(name);
        if check_name(name).is_ok() && dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        self.write(&ledger)?;
        Ok(entry)
    }

    /// Turns a plugin on or off without touching its files.
    ///
    /// # Errors
    ///
    /// Returns an error when no such plugin is installed.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let mut ledger = self.read()?;
        let Some(entry) = ledger.plugin.get_mut(name) else {
            bail!("no plugin named {name:?} is installed");
        };
        entry.enabled = enabled;
        self.write(&ledger)
    }

    /// The installed plugin, if it may load: still hashing to what was
    /// approved, and made for this Dino.
    pub fn check(&self, name: &str, entry: &Entry) -> std::result::Result<Active, String> {
        check_name(name)?;
        let dir = self.bundles.join(name);
        match digest(&dir) {
            Ok(found) if found == entry.digest => {}
            Ok(_) => {
                return Err("its files changed since it was installed; reinstall it".into());
            }
            Err(error) => return Err(format!("cannot read its files: {error}")),
        }
        let manifest: Manifest = std::fs::read_to_string(dir.join(MANIFEST))
            .map_err(|error| error.to_string())
            .and_then(|text| toml::from_str(&text).map_err(|error| error.to_string()))?;
        engines_match(&manifest.engines)?;
        Ok(Active { dir, manifest })
    }

    /// The enabled plugins that may load, and why each other enabled one may not.
    ///
    /// # Errors
    ///
    /// Returns an error when the ledger cannot be read.
    pub fn load(&self) -> Result<(Vec<Active>, Vec<String>)> {
        let mut active = Vec::new();
        let mut refused = Vec::new();
        for (name, entry) in self.entries()? {
            if !entry.enabled {
                continue;
            }
            match self.check(&name, &entry) {
                Ok(plugin) => active.push(plugin),
                Err(why) => refused.push(format!("plugin {name} is not loaded: {why}")),
            }
        }
        Ok((active, refused))
    }
}

/// An installed, enabled plugin whose files are the ones approved.
#[derive(Debug, Clone)]
pub struct Active {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Active {
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.dir.join("skills")
    }

    fn expand(&self, command: &[String]) -> Vec<String> {
        let dir = self.dir.display().to_string();
        command
            .iter()
            .map(|word| word.replace("{plugin}", &dir))
            .collect()
    }

    pub fn hooks(&self, event: HookEvent) -> Vec<dino_exec::Hook> {
        self.manifest
            .hooks
            .iter()
            .filter(|hook| hook.event == event)
            .map(|hook| dino_exec::Hook {
                command: self.expand(&hook.command),
                extension: hook.extension.clone(),
            })
            .collect()
    }

    pub fn mcp(&self) -> Vec<dino_exec::McpServer> {
        self.manifest
            .mcp
            .iter()
            .map(|(name, server)| dino_exec::McpServer {
                name: name.clone(),
                command: self.expand(&server.command),
                pass_env: server.pass_env.clone(),
                read_only_tools: server.read_only_tools.clone(),
                sandboxed: server.sandbox,
            })
            .collect()
    }

    pub fn instructions(&self) -> Option<dino_core::prompt::Instructions> {
        let file = self.manifest.instructions.as_ref()?;
        let text = std::fs::read_to_string(inside(&self.dir, file)?).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| {
            dino_core::prompt::Instructions::whole(
                format!("plugin {}: {}", self.name(), file.display()),
                text.to_owned(),
            )
        })
    }
}

/// The person's plugins for this process, read once; why any enabled one was
/// left out is said once.
pub fn active() -> &'static [Active] {
    static ACTIVE: OnceLock<Vec<Active>> = OnceLock::new();
    ACTIVE.get_or_init(|| {
        let Some(store) = Store::user() else {
            return Vec::new();
        };
        match store.load() {
            Ok((active, refused)) => {
                for why in refused {
                    eprintln!("{}", style::paint(style::WARN, &why));
                }
                active
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    style::paint(style::WARN, &format!("plugins: {error:#}"))
                );
                Vec::new()
            }
        }
    })
}

/// Writes a bundle that passes [`inspect`] as it stands, into `parent/name`.
///
/// # Errors
///
/// Returns an error for an unusable name, or when the directory exists.
pub fn scaffold(parent: &Path, name: &str) -> Result<PathBuf> {
    check_name(name).map_err(|problem| anyhow::anyhow!("name: {problem}"))?;
    let dir = parent.join(name);
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    let current = semver::Version::parse(DINO_VERSION).expect("the crate version is semver");
    let manifest = format!(
        r#"name = "{name}"
version = "0.1.0"
description = "What {name} is for, in one line."
# The Dino versions this works with, as a semver range.
engines = ">={major}.{minor}"

# A file read into every session's instructions.
# instructions = "INSTRUCTIONS.md"

# Hooks take the fields of [[hooks]] in config.toml. {{plugin}} is this bundle's
# installed directory; {{path}} and {{edition}} are as for any hook.
# [[hooks]]
# event = "after_edit"
# extension = "rs"
# command = ["{{plugin}}/bin/check.sh", "{{path}}"]

# MCP servers take the fields of [mcp.<name>] in config.toml.
# [mcp.{name}]
# command = ["{{plugin}}/bin/server"]
"#,
        major = current.major,
        minor = current.minor,
    );
    let skill = format!(
        "---\nname: {name}\ndescription: When to use this skill, in one sentence a task can be \
         matched against.\n---\n\n# {name}\n\nWhat to do, in order.\n"
    );
    std::fs::create_dir_all(dir.join("skills").join(name))?;
    std::fs::write(dir.join(MANIFEST), manifest)?;
    std::fs::write(dir.join("skills").join(name).join("SKILL.md"), skill)?;
    Ok(dir)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh directory for one test, removed when dropped.
    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub(crate) fn new(label: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("dino-plugin-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn store(&self) -> Store {
            Store {
                bundles: self.0.join("data/plugins"),
                ledger: self.0.join("config/plugins.toml"),
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn bundle(parent: &Path, name: &str, version: &str) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(dir.join("skills/tidy")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(
            dir.join(MANIFEST),
            format!(
                "name = \"{name}\"\nversion = \"{version}\"\ndescription = \"Tidies.\"\n\
                 engines = \">=0.1\"\ninstructions = \"INSTRUCTIONS.md\"\n\n\
                 [[hooks]]\nevent = \"after_edit\"\nextension = \"rs\"\n\
                 command = [\"{{plugin}}/bin/tidy.sh\", \"{{path}}\"]\n\n\
                 [mcp.docs]\ncommand = [\"{{plugin}}/bin/tidy.sh\", \"serve\"]\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("skills/tidy/SKILL.md"),
            "---\nname: tidy\ndescription: Tidy a crate.\n---\nTidy it.\n",
        )
        .unwrap();
        std::fs::write(dir.join("bin/tidy.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("INSTRUCTIONS.md"), "Keep it tidy.\n").unwrap();
        dir
    }

    #[test]
    fn a_scaffolded_bundle_passes_validation_as_it_stands() {
        let scratch = Scratch::new("scaffold");

        let dir = scaffold(&scratch.0, "my-plugin").unwrap();
        let bundle = inspect(&dir).unwrap();

        assert_eq!(bundle.manifest.name, "my-plugin");
        assert_eq!(bundle.skills.len(), 1);
        assert!(scaffold(&scratch.0, "my-plugin").is_err());
        assert!(scaffold(&scratch.0, "../escape").is_err());
    }

    #[test]
    fn validation_names_every_problem_at_once() {
        let scratch = Scratch::new("problems");
        let dir = scratch.0.join("bad");
        std::fs::create_dir_all(dir.join("skills/nodesc")).unwrap();
        std::fs::write(dir.join("skills/nodesc/SKILL.md"), "no front matter\n").unwrap();
        std::fs::write(
            dir.join(MANIFEST),
            "name = \"Bad_Name\"\nversion = \"one\"\ndescription = \"\"\nengines = \"<0.0.1\"\n\
             instructions = \"../outside.md\"\n\n[[hooks]]\nevent = \"after_edit\"\n\
             command = [\"{plugin}/missing.sh\"]\n\n[mcp.empty]\ncommand = []\n",
        )
        .unwrap();

        let message = inspect(&dir).unwrap_err().to_string();

        for expected in [
            "8 problems",
            "name:",
            "version \"one\"",
            "description is empty",
            "does not include this dino",
            "outside the bundle",
            "skills/nodesc",
            "hooks[0]: {plugin}/missing.sh",
            "mcp.empty: command is empty",
        ] {
            assert!(message.contains(expected), "{expected}: {message}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_in_a_bundle_is_refused() {
        let scratch = Scratch::new("symlink");
        let dir = bundle(&scratch.0, "linked", "0.1.0");
        std::os::unix::fs::symlink("/etc/passwd", dir.join("passwd")).unwrap();

        let message = inspect(&dir).unwrap_err().to_string();

        assert!(message.contains("symbolic link"), "{message}");
    }

    #[test]
    fn the_digest_covers_paths_and_contents_but_not_git() {
        let scratch = Scratch::new("digest");
        let dir = bundle(&scratch.0, "tidy", "0.1.0");
        let first = digest(&dir).unwrap();

        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref").unwrap();
        assert_eq!(digest(&dir).unwrap(), first);

        std::fs::write(dir.join("INSTRUCTIONS.md"), "Keep it untidy.\n").unwrap();
        assert_ne!(digest(&dir).unwrap(), first);
        assert!(first.starts_with("sha256:") && first.len() == 71, "{first}");
    }

    #[test]
    fn an_installed_plugin_contributes_with_its_directory_in_place_of_the_placeholder() {
        let scratch = Scratch::new("install");
        let store = scratch.store();
        let bundle = inspect(&bundle(&scratch.0, "tidy", "0.1.0")).unwrap();

        assert_eq!(store.install(&bundle, "here").unwrap(), None);
        let (active, refused) = store.load().unwrap();

        assert!(refused.is_empty(), "{refused:?}");
        let [plugin] = active.as_slice() else {
            panic!("{active:?}")
        };
        let installed = store.bundles.join("tidy").display().to_string();
        assert_eq!(
            plugin.hooks(HookEvent::AfterEdit)[0].command,
            [format!("{installed}/bin/tidy.sh"), "{path}".to_owned()]
        );
        assert!(plugin.hooks(HookEvent::TaskEnd).is_empty());
        assert_eq!(plugin.mcp()[0].name, "docs");
        assert!(plugin.mcp()[0].sandboxed);
        assert_eq!(plugin.instructions().unwrap().text, "Keep it tidy.");
        assert_eq!(
            dino_core::skills::discover(&[plugin.skills_dir()])[0].name,
            "tidy"
        );
    }

    #[test]
    fn a_plugin_whose_files_changed_after_approval_is_not_loaded() {
        let scratch = Scratch::new("tamper");
        let store = scratch.store();
        let bundle = inspect(&bundle(&scratch.0, "tidy", "0.1.0")).unwrap();
        store.install(&bundle, "here").unwrap();

        std::fs::write(
            store.bundles.join("tidy/bin/tidy.sh"),
            "#!/bin/sh\ncurl evil\n",
        )
        .unwrap();
        let (active, refused) = store.load().unwrap();

        assert!(active.is_empty());
        assert!(
            refused[0].contains("changed since it was installed"),
            "{refused:?}"
        );
    }

    #[test]
    fn disabling_keeps_the_files_and_stops_the_loading_and_uninstalling_removes_both() {
        let scratch = Scratch::new("lifecycle");
        let store = scratch.store();
        let first = inspect(&bundle(&scratch.0, "tidy", "0.1.0")).unwrap();
        store.install(&first, "here").unwrap();

        store.set_enabled("tidy", false).unwrap();
        assert!(store.load().unwrap().0.is_empty());
        assert!(store.bundles.join("tidy").is_dir());
        store.set_enabled("tidy", true).unwrap();
        assert_eq!(store.load().unwrap().0.len(), 1);

        let newer = scratch.0.join("newer");
        let second = inspect(&bundle(&newer, "tidy", "0.2.0")).unwrap();
        assert_eq!(
            store.install(&second, "there").unwrap().as_deref(),
            Some("0.1.0")
        );
        assert_eq!(store.entries().unwrap()["tidy"].version, "0.2.0");

        store.uninstall("tidy").unwrap();
        assert!(!store.bundles.join("tidy").exists());
        assert!(store.entries().unwrap().is_empty());
        assert!(store.uninstall("tidy").is_err());
        assert!(store.set_enabled("tidy", true).is_err());
    }

    #[test]
    fn what_a_bundle_runs_is_shown_before_it_is_installed() {
        let scratch = Scratch::new("describe");
        let bundle = inspect(&bundle(&scratch.0, "tidy", "0.1.0")).unwrap();

        let rows = describe(&bundle, "./tidy");
        let shown: Vec<String> = rows
            .iter()
            .map(|(label, value)| format!("{label} {value}"))
            .collect();

        assert!(
            shown
                .iter()
                .any(|row| row == "hook after_edit on .rs: {plugin}/bin/tidy.sh {path}"),
            "{shown:?}"
        );
        assert!(
            shown
                .iter()
                .any(|row| row == "mcp docs: {plugin}/bin/tidy.sh serve  (sandboxed)"),
            "{shown:?}"
        );
        assert!(shown.iter().any(|row| row.starts_with("runs code yes")));
        assert!(
            shown
                .iter()
                .any(|row| *row == format!("digest {}", bundle.digest))
        );
    }
}
