//! Custom slash commands: Markdown files a person invokes by name.
//!
//! `~/.config/dino/commands/<name>.md`, a plugin's `commands/` and the
//! workspace's `.dino/commands/`, later ones shadowing earlier. A repository
//! ships its own, so nothing in one can widen approvals or the sandbox, and a
//! `!` line in a body is sent as text, never run.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::preset::Preset;

/// The largest command file read; a prompt longer than this is not a command.
const MAX_COMMAND_BYTES: u64 = 64 * 1_024;

/// Front matter a command may carry. Anything else, such as Claude Code's
/// `allowed-tools`, is named at startup and ignored.
const KEYS: [&str; 4] = ["description", "argument-hint", "model", "preset"];

/// Names of the loaded commands, for telling a command from prose wherever a
/// line is read, including while a run is under way.
static NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    User,
    Plugin(String),
    Workspace,
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Self::User => "yours".to_owned(),
            Self::Plugin(name) => format!("plugin {name}"),
            Self::Workspace => ".dino/commands".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    model: Option<String>,
    preset: Option<Preset>,
    body: String,
    pub source: Source,
}

/// A command invoked, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub name: String,
    pub text: String,
    /// The model it asks for, which means a new conversation unless answering.
    pub model: Option<String>,
    /// `preset: review`: the submission runs read-only, below the model.
    pub read_only: bool,
    /// Said to the person before it is sent.
    pub notes: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Commands {
    list: Vec<Command>,
}

/// Every command found, and what a person should hear about them at startup.
#[derive(Debug, Default)]
pub struct Loaded {
    pub commands: Commands,
    pub notes: Vec<String>,
}

/// Reads the commands in `dirs`, in order of increasing precedence. A name a
/// built-in command already has is left out.
pub fn load(dirs: &[(Source, PathBuf)], is_builtin: impl Fn(&str) -> bool) -> Loaded {
    let mut loaded = Loaded::default();
    for (source, dir) in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
            .collect();
        paths.sort();
        for path in paths {
            let shown = match (source, path.file_name()) {
                (Source::Workspace, Some(file)) => {
                    format!(".dino/commands/{}", file.to_string_lossy())
                }
                _ => crate::render::short_path(&path, Path::new("")),
            };
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !valid_name(name) {
                loaded.notes.push(format!(
                    "{shown}: a command name is letters, digits, - and _; not loaded"
                ));
                continue;
            }
            if is_builtin(name) {
                loaded
                    .notes
                    .push(format!("{shown}: /{name} is built in, which wins"));
                continue;
            }
            let read = std::fs::metadata(&path)
                .ok()
                .filter(|metadata| metadata.len() <= MAX_COMMAND_BYTES)
                .and_then(|_| std::fs::read_to_string(&path).ok());
            let Some(text) = read else {
                loaded.notes.push(format!(
                    "{shown}: unreadable, or over {} KiB; not loaded",
                    MAX_COMMAND_BYTES / 1_024
                ));
                continue;
            };
            let (command, ignored) = parse(name, &text, source.clone());
            if !ignored.is_empty() {
                loaded.notes.push(format!(
                    "{shown}: ignoring {}; a command cannot change approvals, tools or the \
                     sandbox",
                    ignored.join(", ")
                ));
            }
            if let Some(index) = loaded
                .commands
                .list
                .iter()
                .position(|known| known.name == name)
            {
                let shadowed = loaded.commands.list.remove(index);
                loaded.notes.push(format!(
                    "/{name} from {} shadows {}",
                    command.source.label(),
                    shadowed.source.label()
                ));
            }
            loaded.commands.list.push(command);
        }
    }
    loaded.commands.list.sort_by(|a, b| a.name.cmp(&b.name));
    loaded
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A command from its file's text, and the front matter keys it ignores.
fn parse(name: &str, text: &str, source: Source) -> (Command, Vec<String>) {
    let (front, body) = dino_core::skills::split_front_matter(text);
    let field =
        |key: &str| dino_core::skills::front_field(front, key).filter(|value| !value.is_empty());
    let mut ignored: Vec<String> = front
        .lines()
        .filter(|line| !line.starts_with([' ', '\t', '-', '#']))
        .filter_map(|line| line.split_once(':').map(|(key, _)| key.trim()))
        .filter(|key| !key.is_empty() && !KEYS.contains(key))
        .map(str::to_owned)
        .collect();
    let preset = field("preset").and_then(|value| {
        let parsed = <Preset as clap::ValueEnum>::from_str(&value, true).ok();
        if parsed.is_none() {
            ignored.push(format!("preset {value}"));
        }
        parsed
    });
    let command = Command {
        name: name.to_owned(),
        description: field("description"),
        argument_hint: field("argument-hint"),
        model: field("model"),
        preset,
        body: body.trim().to_owned(),
        source,
    };
    (command, ignored)
}

impl Commands {
    pub fn iter(&self) -> impl Iterator<Item = &Command> {
        self.list.iter()
    }

    /// Makes these the commands every line is read against.
    pub fn register(&self) {
        if let Ok(mut names) = NAMES.lock() {
            *names = self
                .list
                .iter()
                .map(|command| command.name.clone())
                .collect();
        }
    }

    /// The command `line` invokes, expanded with its arguments.
    pub fn invoke(&self, line: &str) -> Option<Invocation> {
        let (name, arguments) = invoked(line)?;
        let command = self.list.iter().find(|command| command.name == name)?;
        let mut notes = Vec::new();
        if command
            .body
            .lines()
            .any(|line| line.trim_start().starts_with('!') || line.contains("!`"))
        {
            notes.push(format!(
                "/{name} has ! lines, which are sent as text; Dino does not run them"
            ));
        }
        if let Some(preset) = command.preset.filter(|preset| *preset != Preset::Review) {
            notes.push(format!(
                "/{name} names preset {}, which a running session cannot change; only review \
                 has an effect, running a command read-only",
                preset.name()
            ));
        }
        Some(Invocation {
            name: name.to_owned(),
            text: expand(&command.body, arguments),
            model: command.model.clone(),
            read_only: command.preset == Some(Preset::Review),
            notes,
        })
    }
}

/// Whether `line` invokes a loaded command, by the registered names.
pub fn claims(line: &str) -> bool {
    invoked(line).is_some_and(|(name, _)| {
        NAMES
            .lock()
            .is_ok_and(|names| names.iter().any(|known| known == name))
    })
}

/// The name after the slash and the arguments after the name.
fn invoked(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim().strip_prefix('/')?;
    let name = rest.split_whitespace().next()?;
    valid_name(name).then(|| (name, rest[name.len()..].trim()))
}

/// The body with `$ARGUMENTS` and `$1`, `$2`, ... filled in. A body that uses
/// neither has the arguments appended, so they are never lost.
pub fn expand(body: &str, arguments: &str) -> String {
    let words = split_arguments(arguments);
    let mut text = String::with_capacity(body.len() + arguments.len());
    let mut used = false;
    let mut rest = body;
    while let Some(index) = rest.find('$') {
        text.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        if let Some(tail) = after.strip_prefix("ARGUMENTS") {
            text.push_str(arguments);
            used = true;
            rest = tail;
            continue;
        }
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        match after[..digits].parse::<usize>() {
            Ok(position) if position > 0 => {
                if let Some(word) = words.get(position - 1) {
                    text.push_str(word);
                }
                used = true;
                rest = &after[digits..];
            }
            _ => {
                text.push('$');
                rest = after;
            }
        }
    }
    text.push_str(rest);
    if !used && !arguments.is_empty() {
        text.push_str("\n\n");
        text.push_str(arguments);
    }
    text
}

/// Words split on whitespace, with quotes holding a word together.
fn split_arguments(arguments: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in arguments.chars() {
        match (quote, c) {
            (Some(open), _) if c == open => quote = None,
            (Some(_), _) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dino-commands-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, text) in files {
            std::fs::write(dir.join(file), text).unwrap();
        }
        dir
    }

    #[test]
    fn arguments_fill_the_body_whole_and_by_position() {
        assert_eq!(
            expand(
                "Review PR $1 for $2. All: $ARGUMENTS",
                "42 \"error handling\""
            ),
            "Review PR 42 for error handling. All: 42 \"error handling\""
        );
        assert_eq!(expand("costs $5 and $0", ""), "costs  and $0");
        assert_eq!(
            expand("Explain this.", "src/lib.rs"),
            "Explain this.\n\nsrc/lib.rs"
        );
        assert_eq!(expand("Plain $HOME text", ""), "Plain $HOME text");
    }

    #[test]
    fn a_workspace_command_shadows_a_user_one_and_says_so() {
        let user = dir(
            "user",
            &[
                (
                    "review.md",
                    "---\ndescription: mine\n---\nReview carefully.",
                ),
                ("tidy.md", "Tidy $ARGUMENTS"),
            ],
        );
        let workspace = dir(
            "workspace",
            &[
                ("review.md", "---\ndescription: theirs\n---\nApprove it."),
                ("help.md", "Not a help."),
                ("bad name.md", "Nope."),
            ],
        );

        let loaded = load(
            &[
                (Source::User, user.clone()),
                (Source::Workspace, workspace.clone()),
            ],
            |name| name == "help",
        );

        let names: Vec<&str> = loaded.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["review", "tidy"]);
        let review = loaded.commands.invoke("/review").unwrap();
        assert_eq!(review.text, "Approve it.");
        let notes = loaded.notes.join("\n");
        assert!(
            notes.contains("/review from .dino/commands shadows yours"),
            "{notes}"
        );
        assert!(notes.contains("/help is built in"), "{notes}");
        assert!(notes.contains("letters, digits"), "{notes}");
        let _ = std::fs::remove_dir_all(&user);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn front_matter_cannot_widen_anything_and_bang_lines_are_not_run() {
        let workspace = dir(
            "front",
            &[(
                "ship.md",
                "---\ndescription: Ship it\nallowed-tools: Bash(git push:*)\npreset: review\n\
                 model: claude-haiku\n---\n!`git push --force`\nThen say $1.",
            )],
        );

        let loaded = load(&[(Source::Workspace, workspace.clone())], |_| false);
        let invoked = loaded.commands.invoke("/ship  done ").unwrap();

        assert!(
            loaded.notes[0].contains("ignoring allowed-tools"),
            "{:?}",
            loaded.notes
        );
        assert_eq!(invoked.text, "!`git push --force`\nThen say done.");
        assert!(invoked.read_only);
        assert_eq!(invoked.model.as_deref(), Some("claude-haiku"));
        assert!(
            invoked.notes[0].contains("Dino does not run them"),
            "{:?}",
            invoked.notes
        );
        let command = loaded.commands.iter().next().unwrap();
        assert_eq!(command.description.as_deref(), Some("Ship it"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn only_a_slash_and_a_known_name_invoke_a_command() {
        let workspace = dir("invoke", &[("fix.md", "Fix $ARGUMENTS")]);
        let loaded = load(&[(Source::Workspace, workspace.clone())], |_| false);

        assert!(loaded.commands.invoke("fix the thing").is_none());
        assert!(loaded.commands.invoke("/fixes").is_none());
        assert!(loaded.commands.invoke("/usr/bin/fix").is_none());
        assert_eq!(
            loaded.commands.invoke("/fix the build").unwrap().text,
            "Fix the build"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
