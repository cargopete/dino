//! One task: a submission, the budget it runs under, and what happens when it
//! ends.
//!
//! The person says the limits in the message ("max $50", "max 1 hour"); with
//! none named, a default applies. A limit is checked before each model request,
//! so a task that reaches one stops before spending more, and is then asked once
//! what is done and what is left.

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use dino_core::Agent;
use dino_core::AgentError;
use dino_core::RunResult;
use dino_core::ToolSet;
use dino_model::ModelSession;
use dino_protocol::StopReason;
use tokio_util::sync::CancellationToken;

use crate::spend::Meter;
use crate::spend::Spent;
use crate::spend::usd;

/// What a task may spend. `None` is no limit on that axis, and a task that
/// names one limit has no other.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub usd: Option<f64>,
    pub wall: Option<Duration>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            usd: Some(20.0),
            wall: Some(Duration::from_secs(4 * 3_600)),
        }
    }
}

impl Limits {
    #[must_use]
    pub fn describe(&self) -> String {
        match (self.usd, self.wall) {
            (Some(dollars), Some(wall)) => format!("{} and {}", usd(dollars), duration_text(wall)),
            (Some(dollars), None) => format!("{}, no time limit", usd(dollars)),
            (None, Some(wall)) => format!("{}, no dollar limit", duration_text(wall)),
            (None, None) => "no limit".to_owned(),
        }
    }
}

/// The limits a message names after the word `max`, or `None` when it names
/// none: `max $50`, `max 10 dollars`, `max 1 hour`, `max 90 minutes`, `max 2h`.
#[must_use]
pub fn named(message: &str) -> Option<Limits> {
    let words: Vec<&str> = message.split_whitespace().collect();
    let (mut dollars, mut wall) = (None, None);
    for (index, word) in words.iter().enumerate() {
        if !matches!(trim(word).to_ascii_lowercase().as_str(), "max" | "maximum") {
            continue;
        }
        let Some(amount) = words.get(index + 1).map(|word| trim_keeping_dollar(word)) else {
            continue;
        };
        if let Some(rest) = amount.strip_prefix('$') {
            if let Some(number) = number(rest) {
                dollars.get_or_insert(number);
            }
            continue;
        }
        let split = amount
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(amount.len());
        let (figure, glued) = amount.split_at(split);
        let Some(figure) = number(figure) else {
            continue;
        };
        let unit = if glued.is_empty() {
            words
                .get(index + 2)
                .map(|word| trim(word).to_ascii_lowercase())
        } else {
            Some(glued.to_ascii_lowercase())
        };
        match unit.as_deref() {
            Some("h" | "hr" | "hrs" | "hour" | "hours") => {
                wall.get_or_insert(Duration::from_secs_f64(figure * 3_600.0));
            }
            Some("m" | "min" | "mins" | "minute" | "minutes") => {
                wall.get_or_insert(Duration::from_secs_f64(figure * 60.0));
            }
            Some("usd" | "dollar" | "dollars") => {
                dollars.get_or_insert(figure);
            }
            _ => {}
        }
    }
    (dollars.is_some() || wall.is_some()).then_some(Limits { usd: dollars, wall })
}

fn is_punctuation(c: char) -> bool {
    matches!(
        c,
        ',' | '.' | ';' | ':' | '(' | ')' | '"' | '\'' | '!' | '?'
    )
}

fn trim(word: &str) -> &str {
    word.trim_matches(is_punctuation)
}

fn trim_keeping_dollar(word: &str) -> &str {
    word.trim_start_matches(|c| is_punctuation(c) && c != '$')
        .trim_end_matches(is_punctuation)
}

/// A positive amount, thousands separators allowed.
fn number(text: &str) -> Option<f64> {
    text.replace(',', "")
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
}

/// Whether the person said to see the task through: a limit named, or words
/// that mean it. Only then is a turn that ends after real work sent back.
#[must_use]
pub fn wants_to_finish(message: &str) -> bool {
    const PHRASES: [&str; 14] = [
        "keep going",
        "don't stop",
        "do not stop",
        "however long it takes",
        "until it's done",
        "until it is done",
        "until done",
        "until it's finished",
        "until it is finished",
        "until finished",
        "until complete",
        "to completion",
        "all the way",
        "finish the",
    ];
    let lower = message.to_lowercase().replace('\u{2019}', "'");
    named(message).is_some()
        || PHRASES.iter().any(|phrase| lower.contains(phrase))
        || ["finish this", "finish it", "finish off"]
            .iter()
            .any(|phrase| lower.contains(phrase))
}

#[must_use]
pub fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        _ => format!("{}h {:02}m", seconds / 3_600, seconds % 3_600 / 60),
    }
}

/// The task in flight, if any, and the limits for the next.
#[derive(Debug, Default)]
pub struct Task {
    defaults: Limits,
    state: Mutex<Option<Running>>,
}

#[derive(Debug)]
struct Running {
    started: Instant,
    spent_before: f64,
    limits: Limits,
    stopped: Option<String>,
    lifted: bool,
}

impl Task {
    #[must_use]
    pub fn new(defaults: Limits) -> Self {
        Self {
            defaults,
            state: Mutex::new(None),
        }
    }

    /// Starts the clock, under the limits `message` names or else the defaults.
    /// Says whether the message named them.
    pub fn begin(&self, message: &str, spent: f64) -> (Limits, bool) {
        let named = named(message);
        let limits = named.unwrap_or(self.defaults);
        if let Ok(mut state) = self.state.lock() {
            *state = Some(Running {
                started: Instant::now(),
                spent_before: spent,
                limits,
                stopped: None,
                lifted: false,
            });
        }
        (limits, named.is_some())
    }

    /// Lets the next request go, or says which limit stopped the task.
    ///
    /// # Errors
    ///
    /// Returns the sentence the task ends with when a limit is reached.
    pub fn admit(&self, spent: f64) -> Result<(), String> {
        let Ok(mut state) = self.state.lock() else {
            return Ok(());
        };
        let Some(running) = state.as_mut().filter(|running| !running.lifted) else {
            return Ok(());
        };
        let used = spent - running.spent_before;
        let elapsed = running.started.elapsed();
        let reason = match (running.limits.usd, running.limits.wall) {
            (Some(cap), _) if used >= cap => Some(format!(
                "the task has spent {} of its {} budget at list price",
                usd(used),
                usd(cap)
            )),
            (_, Some(cap)) if elapsed >= cap => Some(format!(
                "the task has run for {} of its {} limit",
                duration_text(elapsed),
                duration_text(cap)
            )),
            _ => None,
        };
        match reason {
            Some(reason) => {
                running.stopped = Some(reason.clone());
                Err(format!("stopped: {reason}"))
            }
            None => Ok(()),
        }
    }

    /// Why the task in flight was stopped, if a limit stopped it.
    #[must_use]
    pub fn stopped(&self) -> Option<String> {
        self.state
            .lock()
            .ok()?
            .as_ref()
            .and_then(|running| running.stopped.clone())
    }

    /// Stops enforcing the limits, for the one request that says what is left.
    pub fn lift(&self) {
        if let Ok(mut state) = self.state.lock()
            && let Some(running) = state.as_mut()
        {
            running.lifted = true;
        }
    }

    /// Ends the task and says how long it ran.
    pub fn end(&self) -> Option<Duration> {
        self.state
            .lock()
            .ok()?
            .take()
            .map(|running| running.started.elapsed())
    }
}

/// Asks a task a limit stopped what is done and what is left, with changes and
/// processes refused, so the request cannot start more work.
pub async fn wrap_up<S: ModelSession>(
    agent: &mut Agent<S, ToolSet>,
    meter: &Meter,
    planning: &AtomicBool,
    reason: &str,
    cancel: &CancellationToken,
) -> Option<RunResult> {
    meter.task().lift();
    agent.set_keep_going(false);
    let was_planning = planning.swap(true, Ordering::Relaxed);
    let result = agent
        .run_cancellable(
            format!(
                "The task budget is spent: {reason}. Start nothing new and call no tools. Say \
                 what is done, what is left, and where to pick it up."
            ),
            cancel,
        )
        .await;
    planning.store(was_planning, Ordering::Relaxed);
    result.ok()
}

/// A command to run when a task ends, for a task that ran at least `after`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndHook {
    pub hook: dino_exec::Hook,
    pub after: Duration,
}

/// What a task came to, for a ping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: &'static str,
    pub detail: String,
}

impl Outcome {
    #[must_use]
    pub fn of(result: &Result<RunResult, AgentError>, stopped: Option<String>) -> Self {
        if let Some(reason) = stopped {
            return Self {
                status: "stopped",
                detail: reason,
            };
        }
        match result {
            Ok(run) => Self {
                status: match run.stop_reason {
                    StopReason::EndTurn => "finished",
                    StopReason::Cancelled => "cancelled",
                    _ => "stopped",
                },
                detail: last_paragraph(&run.text),
            },
            Err(error) => Self {
                status: "failed",
                detail: error.to_string(),
            },
        }
    }
}

fn last_paragraph(text: &str) -> String {
    let paragraph = text
        .trim()
        .rsplit("\n\n")
        .next()
        .unwrap_or_default()
        .replace('\n', " ");
    dino_core::text::capped_head(&paragraph, 300)
}

/// The line a person reads where the ping lands.
#[must_use]
pub fn ping_message(
    outcome: &Outcome,
    elapsed: Duration,
    workspace: &std::path::Path,
    model: &str,
    spent: Spent,
) -> String {
    let place = workspace
        .file_name()
        .map_or_else(|| "workspace".into(), |name| name.to_string_lossy());
    // A model that ends its turn has stopped, which is not a check that the
    // work is done; what it says is its own account.
    let (ended, said) = match outcome.status {
        "finished" => ("ended its turn", ", saying: "),
        status => (status, ": "),
    };
    let mut line = format!(
        "dino {ended} after {} in {place} on {model} ({})",
        duration_text(elapsed),
        usd(spent.usd)
    );
    if !outcome.detail.is_empty() {
        line.push_str(said);
        line.push_str(&outcome.detail);
    }
    line
}

/// Ends the task in flight and pings for it, for a run that produced `result`.
/// Does nothing when no task was begun.
pub async fn conclude(
    hooks: &[EndHook],
    workspace: &std::path::Path,
    model: &str,
    meter: &Meter,
    result: &Result<RunResult, AgentError>,
    stopped: Option<String>,
) {
    let Some(elapsed) = meter.task().end() else {
        return;
    };
    let outcome = Outcome::of(result, stopped);
    ping(hooks, elapsed, workspace, model, meter.spent(), &outcome).await;
}

/// Runs the task-end hooks that apply to a task of this length, and says on
/// standard error which failed.
pub async fn ping(
    hooks: &[EndHook],
    elapsed: Duration,
    workspace: &std::path::Path,
    model: &str,
    spent: Spent,
    outcome: &Outcome,
) {
    let due: Vec<dino_exec::Hook> = hooks
        .iter()
        .filter(|end| elapsed >= end.after)
        .map(|end| end.hook.clone())
        .collect();
    if due.is_empty() {
        return;
    }
    let ended = dino_exec::TaskEnd {
        status: outcome.status.to_owned(),
        message: ping_message(outcome, elapsed, workspace, model, spent),
    };
    for problem in dino_exec::run_task_end_hooks(&due, workspace, &ended).await {
        eprintln!("{}", crate::style::paint(crate::style::WARN, &problem));
    }
}

/// The note a model gets when its branch lacks what the main branch holds.
#[must_use]
pub fn behind_note(behind: &dino_core::git::Behind) -> String {
    let plural = if behind.commits == 1 { "" } else { "s" };
    format!(
        "[dino: this branch is {} commit{plural} behind {}, fetched just now. Newest there: {}. \
         Work merged there may overlap this task, and a measurement or decision that has landed \
         is not redone because a local check passed. Before adding code, read what landed \
         (`git log HEAD..{}`). Then stop and tell the person the branch is behind, unless they \
         already told you to rebase or merge; if they did, bring the branch up to date first.]",
        behind.commits,
        behind.upstream,
        behind.subjects.join("; "),
        behind.upstream
    )
}

/// `text`, with a note added when the workspace's branch is behind its remote
/// main after a fetch, so a long task does not rebuild what has already landed.
pub async fn note_if_behind(workspace: &std::path::Path, text: String) -> String {
    let path = workspace.to_path_buf();
    let found = tokio::task::spawn_blocking(move || dino_core::git::behind(&path, true))
        .await
        .ok()
        .flatten();
    let Some(behind) = found else {
        return text;
    };
    eprintln!(
        "{}",
        crate::style::paint(
            crate::style::WARN,
            &format!(
                "  this branch is {} commit(s) behind {}",
                behind.commits, behind.upstream
            )
        )
    );
    format!("{text}\n\n{}", behind_note(&behind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_that_is_behind_is_told_to_read_what_landed_before_building() {
        let note = behind_note(&dino_core::git::Behind {
            upstream: "origin/main".into(),
            commits: 60,
            subjects: vec!["Stop S0".into(), "Meet the gate for a narrow set".into()],
        });

        assert!(note.contains("60 commits behind origin/main"), "{note}");
        assert!(note.contains("Stop S0; Meet the gate"), "{note}");
        assert!(note.contains("git log HEAD..origin/main"), "{note}");
        assert!(note.contains("stop and tell the person"), "{note}");
    }

    fn limits(message: &str) -> Option<Limits> {
        named(message)
    }

    #[test]
    fn a_message_names_dollars_and_time_after_max() {
        assert_eq!(
            limits("finish this RFC, however long it takes, max $50"),
            Some(Limits {
                usd: Some(50.0),
                wall: None
            })
        );
        assert_eq!(
            limits("fix X, max 1 hour, max $10."),
            Some(Limits {
                usd: Some(10.0),
                wall: Some(Duration::from_secs(3_600))
            })
        );
        assert_eq!(
            limits("Max 90 minutes please").and_then(|found| found.wall),
            Some(Duration::from_secs(5_400))
        );
        assert_eq!(
            limits("max 2h").and_then(|found| found.wall),
            Some(Duration::from_secs(7_200))
        );
        assert_eq!(
            limits("max $1,500").and_then(|found| found.usd),
            Some(1_500.0)
        );
        assert_eq!(
            limits("max 10 dollars").and_then(|found| found.usd),
            Some(10.0)
        );
    }

    #[test]
    fn a_max_that_is_not_a_limit_names_none() {
        for message in [
            "raise max_turns to 5",
            "take the max 5 files",
            "max(a, b) is wrong",
            "the max is $0",
            "set the max",
            "fix it",
        ] {
            assert_eq!(limits(message), None, "{message}");
        }
    }

    #[test]
    fn naming_one_limit_leaves_the_other_open_and_naming_none_uses_the_defaults() {
        let task = Task::new(Limits::default());
        let (found, said) = task.begin("fix X, max $5", 0.0);
        assert!(said);
        assert_eq!(found.wall, None);
        let (found, said) = task.begin("fix X", 0.0);
        assert!(!said);
        assert_eq!(found, Limits::default());
    }

    #[test]
    fn a_task_is_stopped_by_dollars_spent_since_it_began() {
        let task = Task::new(Limits::default());
        task.begin("max $5", 100.0);

        assert_eq!(task.admit(104.0), Ok(()));
        let stopped = task.admit(105.5).unwrap_err();

        assert!(stopped.contains("spent $5.50 of its $5.00"), "{stopped}");
        assert_eq!(task.stopped().as_deref(), stopped.strip_prefix("stopped: "));
    }

    #[test]
    fn a_task_is_stopped_by_the_clock_and_lifting_lets_one_request_through() {
        let task = Task::new(Limits {
            usd: None,
            wall: Some(Duration::ZERO),
        });
        task.begin("go", 0.0);

        let stopped = task.admit(0.0).unwrap_err();
        assert!(stopped.contains("limit"), "{stopped}");

        task.lift();
        assert_eq!(task.admit(0.0), Ok(()));
        assert!(task.end().is_some());
        assert_eq!(task.admit(1e9), Ok(()), "no task, no limit");
    }

    #[test]
    fn a_limit_or_a_phrase_means_see_it_through() {
        for message in [
            "continue the RFC wave, keep going until the wave is finished",
            "finish the RFC, max $50",
            "Don\u{2019}t stop until it\u{2019}s done",
            "finish this off",
        ] {
            assert!(wants_to_finish(message), "{message}");
        }
        for message in ["what does this function do?", "letsgoo", "fix the typo"] {
            assert!(!wants_to_finish(message), "{message}");
        }
    }

    #[test]
    fn durations_read_as_a_person_would_say_them() {
        assert_eq!(duration_text(Duration::from_secs(42)), "42s");
        assert_eq!(duration_text(Duration::from_secs(36 * 60 + 5)), "36m");
        assert_eq!(duration_text(Duration::from_secs(3_600 + 5 * 60)), "1h 05m");
    }

    #[test]
    fn the_ping_says_how_the_task_ended() {
        let outcome = Outcome {
            status: "failed",
            detail: "model transport failed".into(),
        };
        let spent = Spent {
            usd: 3.1,
            unpriced: 0,
        };

        let line = ping_message(
            &outcome,
            Duration::from_secs(36 * 60),
            std::path::Path::new("/work/nuthatch"),
            "claude-sonnet",
            spent,
        );

        assert_eq!(
            line,
            "dino failed after 36m in nuthatch on claude-sonnet ($3.10): model transport failed"
        );
        let finished = Outcome {
            status: "finished",
            detail: "All tests pass.".into(),
        };
        let line = ping_message(
            &finished,
            Duration::from_secs(36 * 60),
            std::path::Path::new("/work/nuthatch"),
            "claude-sonnet",
            spent,
        );
        assert_eq!(
            line,
            "dino ended its turn after 36m in nuthatch on claude-sonnet ($3.10), saying: \
             All tests pass."
        );
    }
}
