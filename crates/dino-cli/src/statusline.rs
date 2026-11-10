//! A status line of the person's own: a command from the user file, given the
//! session's figures as JSON, whose first line stands in for the built-in row.
//!
//! It runs outside the sandbox and is never given conversation text, not even
//! what is being typed. It runs at most once a second, on a task of its own, so
//! a slow command delays nothing the ticker draws; one that fails, prints
//! nothing or takes longer than half a second leaves the built-in row in place.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;

use crate::spinner::Status;
use crate::style;

/// The least time between two runs.
const EVERY: Duration = Duration::from_secs(1);
/// How long one run may take before it is killed and counted a failure.
const TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub struct StatusLine {
    command: Vec<String>,
    workspace: PathBuf,
    /// Where the ticker's thread starts a run, since it has no runtime of its own.
    runtime: Option<tokio::runtime::Handle>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The last run's line, None when it failed.
    line: Option<String>,
    started: Option<Instant>,
    running: bool,
}

impl StatusLine {
    pub fn new(command: Vec<String>, workspace: &Path) -> Self {
        Self {
            command,
            workspace: workspace.to_path_buf(),
            runtime: tokio::runtime::Handle::try_current().ok(),
            state: Mutex::new(State::default()),
        }
    }

    /// The one the user file names, if any.
    pub fn configured(settings: &crate::config::RunSettings) -> Option<Arc<Self>> {
        let command = settings.status_line.clone()?;
        Some(Arc::new(Self::new(command, &settings.workspace)))
    }

    /// What the last run printed, or None when it failed.
    pub fn latest(&self) -> Option<String> {
        self.state.lock().ok()?.line.clone()
    }

    /// Starts a run in the background, unless one is running or began less
    /// than a second ago.
    pub fn refresh(self: &Arc<Self>, status: &Status, doing: &str, elapsed: Duration) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.running
                || state
                    .started
                    .is_some_and(|started| started.elapsed() < EVERY)
            {
                return;
            }
            state.running = true;
            state.started = Some(Instant::now());
        }
        let input = input(status, doing, Some(elapsed), &self.workspace);
        let this = Arc::clone(self);
        runtime.spawn(async move {
            let line = this.run(&input).await;
            if let Ok(mut state) = this.state.lock() {
                state.line = line;
                state.running = false;
            }
        });
    }

    async fn run(&self, input: &Value) -> Option<String> {
        let (program, arguments) = self.command.split_first()?;
        let execution = dino_exec::process::run_fed(
            program,
            arguments,
            &self.workspace,
            &dino_sandbox::Unconfined,
            4 * 1_024,
            TIMEOUT,
            &[],
            Some(input.to_string().as_bytes()),
        )
        .await
        .ok()?;
        if execution.exit_code != Some(0) {
            return None;
        }
        first_line(&execution.stdout)
    }
}

/// Runs the command once, as a prompt is shown, and prints its line above it.
/// Nothing is printed when it fails: the prompt has no built-in row to keep.
pub async fn at_prompt(status: &Mutex<Status>) {
    if !style::enabled() {
        return;
    }
    let Some(figures) = status.lock().ok().map(|status| status.clone()) else {
        return;
    };
    let Some(line) = figures.line.as_ref() else {
        return;
    };
    let printed = line
        .run(&input(&figures, "prompt", None, &line.workspace))
        .await;
    if let Ok(mut state) = line.state.lock() {
        state.line.clone_from(&printed);
        state.started = Some(Instant::now());
    }
    if let Some(printed) = printed {
        let room = crate::markdown::terminal_width().saturating_sub(2);
        println!(
            "  {}",
            style::paint(style::FAINT, &crate::calls::cut(&printed, room))
        );
    }
}

/// The session as the command is told it. Built field by field, so nothing
/// typed or said reaches it.
fn input(status: &Status, doing: &str, elapsed: Option<Duration>, workspace: &Path) -> Value {
    let percent = status
        .window
        .filter(|_| status.context > 0)
        .map(|window| status.context * 100 / u64::from(window).max(1));
    json!({
        "dino_version": env!("CARGO_PKG_VERSION"),
        "state": doing,
        "elapsed_seconds": elapsed.map(|elapsed| elapsed.as_secs()),
        "model": status.model,
        "workspace": workspace.display().to_string(),
        "git_branch": branch(workspace),
        "context": {
            "used_tokens": status.context,
            "window_tokens": status.window,
            "used_percent": percent,
        },
        "tokens": {
            "input": status.input,
            "cached": status.cached,
            "output": status.output,
        },
        "cost_usd": status.meter.as_ref().map(|meter| meter.spent().usd),
        "queued": status.queued,
    })
}

/// The branch checked out, read from `HEAD` rather than by running git once a
/// second. None when detached or not a repository.
fn branch(workspace: &Path) -> Option<String> {
    let dot = workspace
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|dot| dot.exists())?;
    let dir = if dot.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        dot.parent()?
            .join(text.trim().strip_prefix("gitdir:")?.trim())
    } else {
        dot
    };
    let head = std::fs::read_to_string(dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned)
}

/// The first line of `output` with escape sequences and other control
/// characters taken out, since they would throw the row's width and could
/// move the cursor. None when nothing is left.
fn first_line(output: &str) -> Option<String> {
    let line = output.lines().next()?;
    let mut clean = String::with_capacity(line.len());
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            // A CSI sequence ends at its first letter; anything else is one character.
            if characters.clone().next() == Some('[') {
                for next in characters.by_ref().skip(1) {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                characters.next();
            }
        } else if !character.is_control() {
            clean.push(character);
        }
    }
    let clean = clean.trim();
    (!clean.is_empty()).then(|| clean.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Status {
        Status {
            model: "Qwen3.7-Max".into(),
            window: Some(262_144),
            context: 65_536,
            input: 18_200,
            output: 1_100,
            typed: "the secret plan".into(),
            ..Status::default()
        }
    }

    fn workspace(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dino-statusline-{}-{name}", std::process::id()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/pete/x\n").unwrap();
        root
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn the_command_is_told_the_figures_and_nothing_typed() {
        let root = workspace("input");
        let input = input(&status(), "thinking", None, &root);

        assert_eq!(input["model"], "Qwen3.7-Max");
        assert_eq!(input["state"], "thinking");
        assert_eq!(input["git_branch"], "pete/x");
        assert_eq!(input["context"]["used_percent"], 25);
        assert_eq!(input["tokens"]["output"], 1_100);
        assert!(!input.to_string().contains("secret"), "{input}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_first_line_is_kept_and_escapes_are_taken_out() {
        assert_eq!(
            first_line("\u{1b}[1;32mmain\u{1b}[0m · 25%\u{7}\nsecond").as_deref(),
            Some("main · 25%")
        );
        assert_eq!(first_line("\n"), None);
        assert_eq!(first_line(""), None);
    }

    #[tokio::test]
    async fn a_run_reads_the_session_from_standard_input() {
        let root = workspace("run");
        let line = StatusLine::new(
            sh("tr -d '\\n' | sed 's/.*\"git_branch\":\"\\([^\"]*\\)\".*/on \\1/'"),
            &root,
        );

        let printed = line.run(&input(&status(), "prompt", None, &root)).await;

        assert_eq!(printed.as_deref(), Some("on pete/x"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_slow_or_failing_command_gives_no_line() {
        let root = workspace("slow");
        let input = input(&status(), "prompt", None, &root);

        let started = Instant::now();
        let slow = StatusLine::new(sh("sleep 5; echo late"), &root);
        assert_eq!(slow.run(&input).await, None);
        assert!(started.elapsed() < Duration::from_secs(2), "it was killed");

        let failing = StatusLine::new(sh("echo half; exit 1"), &root);
        assert_eq!(failing.run(&input).await, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refreshing_runs_at_most_once_a_second() {
        let root = workspace("refresh");
        let line = Arc::new(StatusLine::new(sh("echo x >> runs; echo ok"), &root));

        line.refresh(&status(), "running", Duration::ZERO);
        line.refresh(&status(), "running", Duration::ZERO);
        for _ in 0..100 {
            if line.latest().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        line.refresh(&status(), "running", Duration::ZERO);

        assert_eq!(line.latest().as_deref(), Some("ok"));
        let runs = std::fs::read_to_string(root.join("runs")).unwrap();
        assert_eq!(runs.lines().count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
