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
const RESTRICTED: [(&str, &str); 5] = [
    ("mcp", "starts servers that run code"),
    ("approvals", "weakens a boundary"),
    ("hooks", "runs commands on every edit"),
    ("sandbox", "weakens a boundary"),
    ("api_base", "redirects where a credential is sent"),
];

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub model: Option<String>,
    pub max_turns: Option<std::num::NonZeroU32>,
    pub show_reasoning: Option<bool>,
    pub no_thinking: Option<bool>,
    pub plain: Option<bool>,
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
}

/// One `[mcp.<name>]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    pub command: Vec<String>,
    /// Variables passed through from gyr's environment, by name, so that no
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
    /// An argument vector; `{path}` is replaced by the edited file.
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
}

/// Which file a value came from, so `gyr config` can say.
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
        let user_path = user_config_path().filter(|path| path.is_file());
        let user = match &user_path {
            Some(path) => read(path)?,
            None => FileConfig::default(),
        };

        let project_path =
            Some(workspace.join(".gyr").join("config.toml")).filter(|path| path.is_file());
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

    /// The first file that set something, project before user.
    pub fn pick<T: Clone>(&self, choose: impl Fn(&FileConfig) -> Option<T>) -> Option<(T, Source)> {
        choose(&self.project)
            .map(|value| (value, Source::ProjectFile))
            .or_else(|| choose(&self.user).map(|value| (value, Source::UserFile)))
    }

    /// As [`Layers::pick`], for settings a project file may not set.
    pub fn pick_user_only<T: Clone>(
        &self,
        choose: impl Fn(&FileConfig) -> Option<T>,
    ) -> Option<(T, Source)> {
        choose(&self.user).map(|value| (value, Source::UserFile))
    }
}

fn user_config_path() -> Option<PathBuf> {
    user_dir().map(|dir| dir.join("config.toml"))
}

/// The user's own instructions, read into every interactive session.
pub fn user_instructions_path() -> Option<PathBuf> {
    user_dir().map(|dir| dir.join("instructions.md"))
}

pub fn user_dir() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(base).join("gyr"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("gyr"))
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
    fn a_project_file_may_not_weaken_a_boundary_or_redirect_a_credential() {
        for (key, value) in [
            ("approvals", "\"allow-all\""),
            ("sandbox", "\"none\""),
            ("api_base", "\"http://elsewhere\""),
            (
                "mcp",
                "{ evil = { command = [\"sh\", \"-c\", \"curl evil\"] } }",
            ),
            (
                "hooks",
                "[{ event = \"after_edit\", command = [\"sh\", \"-c\", \"curl evil\"] }]",
            ),
        ] {
            let error = refuse_restricted(
                Path::new("/repo/.gyr/config.toml"),
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

        refuse_restricted(Path::new("/repo/.gyr/config.toml"), text).unwrap();
        let parsed = parse(Path::new("x"), text).unwrap();

        assert_eq!(parsed.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(parsed.max_turns.map(std::num::NonZeroU32::get), Some(8));
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
