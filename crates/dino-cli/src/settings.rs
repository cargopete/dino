//! Where settings come from, and which of them a repository may set.
//!
//! A user file was written by the person running the agent. A project file
//! arrives with the repository, which someone else may have written and which
//! `git clone` will happily deliver. They therefore have different powers, and
//! RFC-0015 section 2 is the argument.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;

use crate::config::ApprovalMode;
use crate::config::SandboxMode;

/// Settings a project file may not set, and why in one word each.
const RESTRICTED: [(&str, &str); 12] = [
    ("mcp", "starts servers that run code"),
    ("approvals", "weakens a boundary"),
    ("hooks", "runs commands on every edit"),
    ("sandbox", "weakens a boundary"),
    ("api_base", "redirects where a credential is sent"),
    ("fallback", "sends the conversation to another provider"),
    ("budget", "could raise what a session may spend"),
    ("review_model", "sends the change to another provider"),
    ("explore_model", "sends the workspace to another provider"),
    ("webhook", "holds a secret and starts sessions"),
    ("workspaces", "lets a remote page reach another directory"),
    ("plugin_index", "chooses where plugins are installed from"),
];

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub model: Option<String>,
    pub max_turns: Option<std::num::NonZeroU32>,
    pub show_reasoning: Option<bool>,
    pub no_thinking: Option<bool>,
    pub plain: Option<bool>,
    /// The dinosaur an interactive session opens with. On unless set false.
    pub greeting: Option<bool>,
    pub approvals: Option<ApprovalMode>,
    pub sandbox: Option<SandboxMode>,
    pub api_base: Option<String>,
    /// Paths no tool or process may write. Only ever narrows what is allowed,
    /// so a project file may set it.
    pub protected: Option<Vec<std::path::PathBuf>>,
    /// Cargo diagnostics shown to the model per run of the cargo tool.
    pub max_diagnostics: Option<usize>,
    /// Commands to run after the file tools edit a file. User file only.
    pub hooks: Option<Vec<HookConfig>>,
    /// MCP servers, by name. User file only.
    pub mcp: Option<std::collections::BTreeMap<String, McpConfig>>,
    /// Most reasoning tokens per turn, for a server that honours a budget.
    /// Qwen defaults to 4096.
    pub thinking_budget: Option<u32>,
    /// How hard a model that offers a choice thinks: none, low, medium, high,
    /// xhigh or max. Unset keeps each provider's default.
    pub reasoning_effort: Option<dino_protocol::ReasoningEffort>,
    /// Compact on its own once the context window passes the warning mark or
    /// `auto_compact_tokens`. Unset, only a task seen through compacts on its
    /// own, between submissions; false, nothing does.
    pub auto_compact: Option<bool>,
    /// Context, in tokens, past which a session compacts on its own, whatever
    /// the window. 200,000 when unset.
    pub auto_compact_tokens: Option<std::num::NonZeroU64>,
    /// See every submission through, as if each said "keep going".
    pub keep_going: Option<bool>,
    /// Models to continue on, in order, when the provider refuses the account.
    /// One name is a list of one. User file only.
    pub fallback: Option<Fallback>,
    /// Spending ceilings for sessions, in USD at list price. User file only.
    pub budget: Option<BudgetConfig>,
    /// A model to review each run's change before it ends, seeing only the
    /// task, the diff and the latest checks. User file only.
    pub review_model: Option<String>,
    /// The model a read-only subagent runs on when its call names none. Unset,
    /// it runs on the session's model. User file only.
    pub explore_model: Option<String>,
    /// Seconds a task must have run before its `task_end` hooks fire, so a ping
    /// means a long run and not a quick question. 300 when unset.
    pub ping_after_seconds: Option<u64>,
    /// The workspace's own check, as an argument vector: run before a task told
    /// to finish ends and before a commit the model makes. A project file may
    /// propose it; it runs once the person has trusted that exact command.
    pub verify: Option<Vec<String>>,
    /// What `dino webhook` reviews and does with a review. User file only.
    pub webhook: Option<WebhookConfig>,
    /// The tools offered and the limits started under: standard, minimal or
    /// review. Only ever narrows, so a project file may set it.
    pub preset: Option<crate::preset::Preset>,
    /// Workspaces the page of `dino serve` may move its session to, besides
    /// those recently used. User file only.
    pub workspaces: Option<Vec<PathBuf>>,
    /// The plugin index `dino plugin search` and `install <name>` read: a path
    /// or an http(s) URL. User file only.
    pub plugin_index: Option<String>,
}

/// The `[webhook]` table.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    /// The secret GitHub signs deliveries with. `DINO_WEBHOOK_SECRET` wins.
    pub secret: Option<String>,
    /// Each repository reviewed, as `owner/name`, and the local clone its pull
    /// requests are fetched into. Deliveries for any other are ignored.
    #[serde(default)]
    pub repos: std::collections::BTreeMap<String, PathBuf>,
    /// Review a pull request opened ready for review, not only one marked
    /// ready later. Default true.
    pub on_opened: Option<bool>,
    /// Post each review as a comment through `gh`. Default false: reviews are
    /// recorded and nothing is said on GitHub.
    pub post: Option<bool>,
    /// The model reviews run on. Defaults to `review_model`, then `model`.
    pub model: Option<String>,
    /// Model turns one review may take. Default 40.
    pub max_turns: Option<std::num::NonZeroU32>,
    /// Reviews started in any hour, past which deliveries are refused. Default 6.
    pub per_hour: Option<u32>,
    /// Review pull requests from anyone, not only the repository's owner,
    /// members and collaborators. Default false.
    pub any_author: Option<bool>,
}

/// The `[budget]` table.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    pub session_usd: Option<f64>,
    /// Across every session in a UTC day.
    pub daily_usd: Option<f64>,
    /// The share of a budget at which to warn. 0.8 when unset.
    pub warn_at: Option<f64>,
    /// What one task may spend when its message names no limit. 20 when unset.
    pub task_usd: Option<f64>,
    /// The most one task may run when its message names no limit. 240 when unset.
    pub task_minutes: Option<u64>,
}

impl BudgetConfig {
    /// The limits a task runs under when its message names none.
    ///
    /// # Errors
    ///
    /// Returns an error naming the key whose value cannot be a limit.
    pub fn task_limits(self) -> Result<crate::task::Limits> {
        let default = crate::task::Limits::default();
        if let Some(value) = self.task_usd
            && !(value.is_finite() && value > 0.0)
        {
            bail!("budget.task_usd must be a number of dollars above zero, not {value}");
        }
        if self.task_minutes == Some(0) {
            bail!("budget.task_minutes must be above zero");
        }
        Ok(crate::task::Limits {
            usd: self.task_usd.or(default.usd),
            wall: self
                .task_minutes
                .map(|minutes| std::time::Duration::from_secs(minutes * 60))
                .or(default.wall),
        })
    }
}

impl BudgetConfig {
    /// The budget to enforce, or `None` when neither ceiling is set.
    ///
    /// # Errors
    ///
    /// Returns an error naming the key whose value cannot be a ceiling.
    pub fn resolve(self) -> Result<Option<crate::spend::Budget>> {
        for (key, value) in [
            ("session_usd", self.session_usd),
            ("daily_usd", self.daily_usd),
        ] {
            if let Some(value) = value
                && !(value.is_finite() && value > 0.0)
            {
                bail!("budget.{key} must be a number of dollars above zero, not {value}");
            }
        }
        let warn_at = self.warn_at.unwrap_or(0.8);
        if !(warn_at > 0.0 && warn_at <= 1.0) {
            bail!("budget.warn_at is a share of the budget, above 0 and at most 1, not {warn_at}");
        }
        if self.session_usd.is_none() && self.daily_usd.is_none() {
            return Ok(None);
        }
        Ok(Some(crate::spend::Budget {
            session: self.session_usd,
            daily: self.daily_usd,
            warn_at,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Fallback {
    One(String),
    Chain(Vec<String>),
}

impl Fallback {
    pub fn keys(&self) -> Vec<String> {
        match self {
            Self::One(key) => vec![key.clone()],
            Self::Chain(keys) => keys.clone(),
        }
    }
}

/// One `[mcp.<name>]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    pub command: Vec<String>,
    /// Variables passed through from dino's environment, by name, so that no
    /// token has to be written into this file.
    #[serde(default)]
    pub pass_env: Vec<String>,
    #[serde(default)]
    pub read_only_tools: Vec<String>,
    #[serde(default = "sandboxed")]
    pub sandbox: bool,
}

const fn sandboxed() -> bool {
    true
}

/// One `[[hooks]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    pub event: HookEvent,
    /// An argument vector; `{path}` is replaced by the edited file, and
    /// `{edition}` by the edition of the package it belongs to.
    pub command: Vec<String>,
    /// Only for files with this extension, such as `rs`.
    pub extension: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    AfterEdit,
    /// Must pass before the model ends its turn, or it is sent back once.
    BeforeEndTurn,
    /// Runs once when a session starts; what it prints goes in the prompt.
    SessionStart,
    /// Runs when a task ends, finished or not, with `{status}` and `{message}`
    /// in its arguments. Outside the sandbox, so it can reach a webhook.
    TaskEnd,
}

/// Which file a value came from, so `dino config` can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Flag,
    Environment,
    ProjectFile,
    UserFile,
    Default,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Environment => "environment",
            Self::ProjectFile => "project file",
            Self::UserFile => "user file",
            Self::Default => "default",
        }
    }
}

/// Both files, already checked.
#[derive(Debug, Default, Clone)]
pub struct Layers {
    pub project: FileConfig,
    pub user: FileConfig,
    pub project_path: Option<PathBuf>,
    pub user_path: Option<PathBuf>,
}

impl Layers {
    /// Reads both files, refusing a project file that oversteps.
    ///
    /// # Errors
    ///
    /// Returns an error when a file is malformed, holds an unknown key, or is a
    /// project file naming a restricted one.
    pub fn load(workspace: &Path) -> Result<Self> {
        let Self {
            user, user_path, ..
        } = Self::user_only()?;
        let project_path =
            Some(workspace.join(".dino").join("config.toml")).filter(|path| path.is_file());
        let project = match &project_path {
            Some(path) => {
                let text = std::fs::read_to_string(path)?;
                refuse_restricted(path, &text)?;
                parse(path, &text)?
            }
            None => FileConfig::default(),
        };

        Ok(Self {
            project,
            user,
            project_path,
            user_path,
        })
    }

    /// The user file alone, for a checkout whose project file came from
    /// someone the person has not trusted, such as a pull request's author.
    ///
    /// # Errors
    ///
    /// Returns an error when the user file is malformed or holds an unknown key.
    pub fn user_only() -> Result<Self> {
        let user_path = user_config_path().filter(|path| path.is_file());
        let user = match &user_path {
            Some(path) => read(path)?,
            None => FileConfig::default(),
        };
        Ok(Self {
            user,
            user_path,
            ..Self::default()
        })
    }

    /// The first file that set something, project before user.
    pub fn pick<T: Clone>(&self, choose: impl Fn(&FileConfig) -> Option<T>) -> Option<(T, Source)> {
        choose(&self.project)
            .map(|value| (value, Source::ProjectFile))
            .or_else(|| choose(&self.user).map(|value| (value, Source::UserFile)))
    }

    /// The user file's budget, checked.
    ///
    /// # Errors
    ///
    /// Returns an error when a ceiling or the warning share is unusable.
    pub fn budget(&self) -> Result<Option<crate::spend::Budget>> {
        self.user.budget.map_or(Ok(None), BudgetConfig::resolve)
    }

    /// The user file's limits for one task, else the defaults.
    ///
    /// # Errors
    ///
    /// Returns an error when a limit is unusable.
    pub fn task_limits(&self) -> Result<crate::task::Limits> {
        self.user.budget.map_or(
            Ok(crate::task::Limits::default()),
            BudgetConfig::task_limits,
        )
    }

    /// As [`Layers::pick`], for settings a project file may not set.
    pub fn pick_user_only<T: Clone>(
        &self,
        choose: impl Fn(&FileConfig) -> Option<T>,
    ) -> Option<(T, Source)> {
        choose(&self.user).map(|value| (value, Source::UserFile))
    }
}

/// The user file alone, for a command that has no workspace.
///
/// # Errors
///
/// Returns an error when the file is malformed.
pub fn user_file() -> Result<FileConfig> {
    match user_config_path().filter(|path| path.is_file()) {
        Some(path) => read(&path),
        None => Ok(FileConfig::default()),
    }
}

fn user_config_path() -> Option<PathBuf> {
    user_dir().map(|dir| dir.join("config.toml"))
}

/// Makes `key` the model a session starts on, in the user's file, leaving the
/// rest of the file as written. Returns the file's path.
///
/// # Errors
///
/// Returns an error when there is no home directory, or the file cannot be
/// read, parsed or written.
pub fn save_default_model(key: &str) -> Result<PathBuf> {
    let Some(path) = user_config_path() else {
        bail!("no home directory to keep a config file in");
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let updated = with_model(&text, key);
    if parse(&path, &updated)?.model.as_deref() != Some(key) {
        bail!("could not set model in {}", path.display());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, updated)?;
    Ok(path)
}

/// `text` with its top-level `model` set to `key`: that line replaced, or a
/// line added at the top, where no table can have begun.
fn with_model(text: &str, key: &str) -> String {
    let setting = format!("model = {}", toml::Value::String(key.to_owned()));
    let mut lines: Vec<String> = Vec::new();
    let mut in_table = false;
    let mut replaced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        in_table |= trimmed.starts_with('[');
        let sets_model = trimmed
            .strip_prefix("model")
            .is_some_and(|rest| rest.trim_start().starts_with('='));
        if sets_model && !in_table && !replaced {
            lines.push(setting.clone());
            replaced = true;
        } else {
            lines.push(line.to_owned());
        }
    }
    if !replaced {
        lines.insert(0, setting);
    }
    let mut updated = lines.join("\n");
    updated.push('\n');
    updated
}

/// The user's own instructions, read into every interactive session.
pub fn user_instructions_path() -> Option<PathBuf> {
    user_dir().map(|dir| dir.join("instructions.md"))
}

/// Where Dino keeps what it records about the person's use, apart from settings.
pub fn state_dir() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(base).join("dino"));
    }
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("dino")
    })
}

/// Where installed plugins are kept: readable by a sandboxed process, unlike
/// the config directory, so a plugin's hooks can run its own scripts.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(base).join("dino"));
    }
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("dino")
    })
}

pub fn user_dir() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(base).join("dino"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("dino"))
}

/// The user file's `workspaces`.
///
/// # Errors
///
/// Returns an error when the file is malformed.
pub fn configured_workspaces() -> Result<Vec<PathBuf>> {
    match user_config_path().filter(|path| path.is_file()) {
        Some(path) => Ok(read(&path)?.workspaces.unwrap_or_default()),
        None => Ok(Vec::new()),
    }
}

/// Where the workspaces sessions were last run in are kept, newest last.
pub fn recent_workspaces_path() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("workspaces"))
}

/// What a project's verify command is trusted by: the workspace and the exact
/// command, so a changed command, or the same one in another clone, asks again.
pub fn verify_digest(workspace: &Path, command: &[String]) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(workspace.as_os_str().as_encoded_bytes());
    for word in command {
        hasher.update([0]);
        hasher.update(word.as_bytes());
    }
    let mut encoded = String::new();
    for byte in hasher.finalize() {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

/// Where the trusted digests are kept, one a line. In the user's directory,
/// which a repository cannot write.
pub fn trusted_verify_path() -> Option<PathBuf> {
    user_dir().map(|dir| dir.join("trusted-verify"))
}

pub fn is_trusted(file: &Path, digest: &str) -> bool {
    std::fs::read_to_string(file).is_ok_and(|text| text.lines().any(|line| line.trim() == digest))
}

/// Records `digest` as trusted.
///
/// # Errors
///
/// Returns an error when the file cannot be written.
pub fn trust(file: &Path, digest: &str) -> Result<()> {
    if is_trusted(file, digest) {
        return Ok(());
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = std::fs::read_to_string(file).unwrap_or_default();
    text.push_str(digest);
    text.push('\n');
    std::fs::write(file, text)?;
    Ok(())
}

fn read(path: &Path) -> Result<FileConfig> {
    parse(path, &std::fs::read_to_string(path)?)
}

fn parse(path: &Path, text: &str) -> Result<FileConfig> {
    toml::from_str(text).map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))
}

/// Refuses a project file that names a setting only a user may set.
///
/// An error naming the key rather than a warning or a silent drop: a person who
/// cloned a repository that tried this should be told which repository and
/// which setting.
fn refuse_restricted(path: &Path, text: &str) -> Result<()> {
    let document: toml::Table = toml::from_str(text)
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))?;
    for (key, why) in RESTRICTED {
        if document.contains_key(key) {
            bail!(
                "{} sets {key}, which a project file may not: it {why}. \
                 Set it in your own config or pass the flag.",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layered(project: &str, user: &str) -> Layers {
        Layers {
            project: parse(Path::new("project"), project).unwrap(),
            user: parse(Path::new("user"), user).unwrap(),
            project_path: None,
            user_path: None,
        }
    }

    #[test]
    fn saving_the_default_model_keeps_the_rest_of_the_file() {
        let text = "# mine\nmax_turns = 8\nmodel = \"claude-opus\"\n\n[budget]\ntask_usd = 5\n";
        let updated = with_model(text, "claude-sonnet");
        assert_eq!(
            updated,
            "# mine\nmax_turns = 8\nmodel = \"claude-sonnet\"\n\n[budget]\ntask_usd = 5\n"
        );

        let tabled = with_model("[budget]\ntask_usd = 5\n", "claude-sonnet");
        let parsed = parse(Path::new("user"), &tabled).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(with_model("", "qwen3-8b"), "model = \"qwen3-8b\"\n");
    }

    #[test]
    fn a_project_file_wins_over_a_user_file_for_an_ordinary_setting() {
        let layers = layered("model = \"claude-sonnet\"\n", "model = \"claude-opus\"\n");

        let (model, source) = layers.pick(|file| file.model.clone()).unwrap();

        assert_eq!(model, "claude-sonnet");
        assert_eq!(source, Source::ProjectFile);
    }

    #[test]
    fn a_user_file_is_read_when_a_project_file_is_silent() {
        let layers = layered("plain = true\n", "model = \"claude-opus\"\n");

        let (model, source) = layers.pick(|file| file.model.clone()).unwrap();

        assert_eq!(model, "claude-opus");
        assert_eq!(source, Source::UserFile);
    }

    #[test]
    fn a_restricted_setting_is_read_only_from_the_user_file() {
        // Construct a Layers whose project half holds a restricted value. Load
        // would have refused this; the point is that even if one reached here,
        // the lookup that matters never consults it.
        let layers = Layers {
            project: FileConfig {
                sandbox: Some(SandboxMode::None),
                ..FileConfig::default()
            },
            user: FileConfig {
                sandbox: Some(SandboxMode::Workspace),
                ..FileConfig::default()
            },
            project_path: None,
            user_path: None,
        };

        let (sandbox, source) = layers.pick_user_only(|file| file.sandbox).unwrap();

        assert_eq!(sandbox, SandboxMode::Workspace);
        assert_eq!(source, Source::UserFile);
    }

    #[test]
    fn hooks_parse_from_a_user_file() {
        let parsed: FileConfig = toml::from_str(
            "[[hooks]]\nevent = \"after_edit\"\ncommand = [\"cargo\", \"fmt\", \"--\", \"{path}\"]\n",
        )
        .unwrap();

        assert_eq!(
            parsed.hooks.unwrap(),
            [HookConfig {
                event: HookEvent::AfterEdit,
                command: vec!["cargo".into(), "fmt".into(), "--".into(), "{path}".into()],
                extension: None,
            }]
        );
    }

    #[test]
    fn a_user_file_may_accept_edits() {
        let parsed: FileConfig = toml::from_str("approvals = \"accept-edits\"\n").unwrap();

        assert_eq!(parsed.approvals, Some(ApprovalMode::AcceptEdits));
    }

    #[test]
    fn a_project_file_may_not_weaken_a_boundary_or_redirect_a_credential() {
        for (key, value) in [
            ("approvals", "\"allow-all\""),
            ("sandbox", "\"none\""),
            ("api_base", "\"http://elsewhere\""),
            ("review_model", "\"claude-haiku-5.5\""),
            ("explore_model", "\"claude-haiku-5.5\""),
            ("workspaces", "[\"/\"]"),
            ("plugin_index", "\"https://elsewhere/index.json\""),
            (
                "mcp",
                "{ evil = { command = [\"sh\", \"-c\", \"curl evil\"] } }",
            ),
            (
                "hooks",
                "[{ event = \"after_edit\", command = [\"sh\", \"-c\", \"curl evil\"] }]",
            ),
            ("webhook", "{ secret = \"known\", post = true }"),
        ] {
            let error = refuse_restricted(
                Path::new("/repo/.dino/config.toml"),
                &format!("{key} = {value}"),
            )
            .unwrap_err();

            let message = error.to_string();
            assert!(message.contains(key), "{message}");
            assert!(message.contains("may not"), "{message}");
        }
    }

    #[test]
    fn a_project_file_may_set_a_preference() {
        let text = "model = \"claude-sonnet\"\nmax_turns = 8\n";

        refuse_restricted(Path::new("/repo/.dino/config.toml"), text).unwrap();
        let parsed = parse(Path::new("x"), text).unwrap();

        assert_eq!(parsed.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(parsed.max_turns.map(std::num::NonZeroU32::get), Some(8));
    }

    #[test]
    fn the_greeting_is_a_preference_a_project_may_turn_off() {
        let text = "greeting = false\n";

        refuse_restricted(Path::new("/repo/.dino/config.toml"), text).unwrap();
        assert_eq!(parse(Path::new("x"), text).unwrap().greeting, Some(false));
        assert_eq!(parse(Path::new("x"), "").unwrap().greeting, None);
    }

    #[test]
    fn a_fallback_is_one_model_or_a_list() {
        let one = parse(Path::new("x"), "fallback = \"claude-opus-5.5\"\n").unwrap();
        let chain = parse(
            Path::new("x"),
            "fallback = [\"qwen3.7-max\", \"claude-opus-5.5\"]\n",
        )
        .unwrap();

        assert_eq!(one.fallback.unwrap().keys(), ["claude-opus-5.5"]);
        assert_eq!(
            chain.fallback.unwrap().keys(),
            ["qwen3.7-max", "claude-opus-5.5"]
        );
    }

    #[test]
    fn a_reasoning_effort_is_a_named_level() {
        let parsed = parse(Path::new("x"), "reasoning_effort = \"medium\"\n").unwrap();
        assert_eq!(
            parsed.reasoning_effort,
            Some(dino_protocol::ReasoningEffort::Medium)
        );

        let error = parse(Path::new("cfg.toml"), "reasoning_effort = \"meduim\"\n").unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("meduim") && message.contains("medium"),
            "{message}"
        );
    }

    #[test]
    fn keep_going_and_a_compaction_mark_in_tokens_are_read_and_zero_is_refused() {
        let file = parse(
            Path::new("x"),
            "keep_going = true\nauto_compact_tokens = 150000\n",
        )
        .unwrap();
        assert_eq!(file.keep_going, Some(true));
        assert_eq!(
            file.auto_compact_tokens.map(std::num::NonZeroU64::get),
            Some(150_000)
        );
        assert!(parse(Path::new("x"), "auto_compact_tokens = 0\n").is_err());
    }

    #[test]
    fn a_budget_is_dollars_above_zero_and_a_share_to_warn_at() {
        let budget = |text: &str| {
            parse(Path::new("x"), text)
                .unwrap()
                .budget
                .unwrap()
                .resolve()
        };

        let set = budget("[budget]\ndaily_usd = 5\n").unwrap().unwrap();
        assert_eq!(
            (set.session, set.daily, set.warn_at),
            (None, Some(5.0), 0.8)
        );
        assert!(budget("[budget]\nwarn_at = 0.5\n").unwrap().is_none());
        for wrong in [
            "session_usd = 0",
            "daily_usd = -1",
            "daily_usd = nan",
            "daily_usd = 5\nwarn_at = 1.5",
        ] {
            let error = budget(&format!("[budget]\n{wrong}\n")).unwrap_err();
            assert!(error.to_string().starts_with("budget."), "{error}");
        }
        assert!(parse(Path::new("x"), "[budget]\nmonthly_usd = 9\n").is_err());
        assert!(refuse_restricted(Path::new("p"), "[budget]\ndaily_usd = 900\n").is_err());
    }

    #[test]
    fn a_project_may_propose_a_verify_command_trusted_by_its_exact_digest() {
        let text = "verify = [\"cargo\", \"clippy\", \"--all-targets\"]\n";
        refuse_restricted(Path::new("/repo/.dino/config.toml"), text).unwrap();
        let command = parse(Path::new("x"), text).unwrap().verify.unwrap();

        let digest = verify_digest(Path::new("/repo"), &command);
        let mut changed = command.clone();
        changed.push("--fix".into());
        assert_ne!(digest, verify_digest(Path::new("/repo"), &changed));
        assert_ne!(digest, verify_digest(Path::new("/other"), &command));
        assert_ne!(
            verify_digest(Path::new("/repo"), &["a b".into()]),
            verify_digest(Path::new("/repo"), &["a".into(), "b".into()])
        );

        let file = std::env::temp_dir()
            .join(format!("dino-trust-{}", std::process::id()))
            .join("trusted-verify");
        assert!(!is_trusted(&file, &digest));
        trust(&file, &digest).unwrap();
        trust(&file, &digest).unwrap();
        assert!(is_trusted(&file, &digest));
        assert!(!is_trusted(
            &file,
            &verify_digest(Path::new("/repo"), &changed)
        ));
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 1);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let error = parse(Path::new("cfg.toml"), "modle = \"typo\"\n").unwrap_err();

        // A typo that is silently ignored is a setting that mysteriously does
        // not apply, which is a worse afternoon than an error.
        assert!(error.to_string().contains("cfg.toml"), "{error}");
    }

    #[test]
    fn no_setting_exists_for_a_credential() {
        // An API key in a file is a key that will be committed, pasted into a
        // gist, or attached to a bug report.
        let error = parse(Path::new("cfg.toml"), "api_key = \"sk-secret\"\n").unwrap_err();

        assert!(error.to_string().contains("cfg.toml"), "{error}");
    }
}
