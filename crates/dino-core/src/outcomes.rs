//! Noticing progress from what tests and lints report, where [`crate::loops`]
//! sees only the calls.
//!
//! Two things are said, each once a run: that the suite passes and clippy is
//! clean with nothing edited since, and that one test keeps failing however it
//! is patched. Neither stops anything.

use std::collections::HashMap;
use std::collections::HashSet;

use dino_protocol::ToolCall;
use serde_json::Value;

/// Consecutive failing test runs, with changes in between, before it is said.
const STUCK_AT: u32 = 3;
/// Failing tests named in one note.
const NAMED: usize = 3;

/// Tools that cannot change the workspace. Anything else not recognised below
/// is taken to have changed it, since a missed edit would call stale work done.
const READ_ONLY: [&str; 10] = [
    "read",
    "search",
    "list",
    "gate",
    "web_fetch",
    "progress",
    "todo",
    "remember",
    "skill",
    "report_blocker",
];
/// Flags that leave a command covering the whole workspace and all its tests.
const WHOLE: [&str; 9] = [
    "--workspace",
    "--all",
    "--all-targets",
    "--all-features",
    "--no-fail-fast",
    "--offline",
    "--locked",
    "-q",
    "--quiet",
];

#[derive(Debug, Default)]
pub struct OutcomeWatch {
    /// A whole-workspace `cargo test` passed since the last edit.
    tested: bool,
    /// A whole-workspace clippy, which checks as well, was clean since the last edit.
    linted: bool,
    said_verified: bool,
    failing: HashMap<String, Streak>,
    said_stuck: HashSet<String>,
}

/// One test's failing runs in a row, and whether anything was edited since the
/// last of them, so a rerun of unchanged code does not count.
#[derive(Debug, Default)]
struct Streak {
    runs: u32,
    changed: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Edit,
    Neutral,
    Test { whole: bool },
    Clippy { whole: bool },
}

impl OutcomeWatch {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Records a call's result, and returns a note for it when the work has
    /// just been verified or a test has just been seen failing too often.
    pub fn record(&mut self, call: &ToolCall, output: &str) -> Option<String> {
        let ran = Ran::parse(output);
        match kind(call) {
            Kind::Edit => {
                self.tested = false;
                self.linted = false;
                for streak in self.failing.values_mut() {
                    streak.changed = true;
                }
                None
            }
            Kind::Neutral => None,
            Kind::Clippy { whole } => {
                self.linted |= whole && ran.clean();
                self.verified()
            }
            Kind::Test { whole } => {
                let tests = Tests::parse(&ran.text);
                self.tested |= whole && ran.exit_code == Some(0) && tests.all_passed();
                self.stuck(&tests).or_else(|| self.verified())
            }
        }
    }

    fn verified(&mut self) -> Option<String> {
        if !(self.tested && self.linted) || self.said_verified {
            return None;
        }
        self.said_verified = true;
        Some(
            "[dino: the whole test suite passes and clippy is clean, with no edit since. The \
             work is verified: finish now, unless something specific in the task is still \
             undone.]"
                .to_owned(),
        )
    }

    fn stuck(&mut self, tests: &Tests) -> Option<String> {
        if tests.results == 0 {
            return None;
        }
        for name in &tests.passed {
            self.failing.remove(name);
        }
        let mut stuck = Vec::new();
        for name in &tests.failed {
            let streak = self.failing.entry(name.clone()).or_default();
            if streak.runs == 0 || streak.changed {
                streak.runs += 1;
            }
            streak.changed = false;
            if streak.runs >= STUCK_AT && self.said_stuck.insert(name.clone()) {
                stuck.push(format!("`{name}`"));
            }
        }
        if stuck.is_empty() {
            return None;
        }
        stuck.truncate(NAMED);
        Some(format!(
            "[dino: {} failed in {STUCK_AT} test runs in a row, with changes in between. Step \
             back before the next patch: reread the task and the failing test, or report what \
             is blocking you.]",
            stuck.join(", ")
        ))
    }
}

fn kind(call: &ToolCall) -> Kind {
    match call.name.as_str() {
        "cargo" => {
            let field = |name: &str| call.arguments.get(name).and_then(Value::as_str);
            let whole = field("package").is_none() && field("filter").is_none();
            match field("command") {
                Some("test") => Kind::Test { whole },
                Some("clippy") => Kind::Clippy { whole },
                Some("fmt" | "add") => Kind::Edit,
                _ => Kind::Neutral,
            }
        }
        "exec" => call
            .arguments
            .get("command")
            .and_then(Value::as_array)
            .and_then(|words| words.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
            .map_or(Kind::Edit, |words| cargo_words(&words)),
        "shell" => call
            .arguments
            .get("command")
            .and_then(Value::as_str)
            .filter(|line| !line.contains(['|', '&', ';', '>', '<', '`', '$', '(']))
            .map_or(Kind::Edit, |line| {
                cargo_words(&line.split_whitespace().collect::<Vec<_>>())
            }),
        name if READ_ONLY.contains(&name) => Kind::Neutral,
        _ => Kind::Edit,
    }
}

/// A `cargo test` or `cargo clippy` command line, or an edit for anything else.
fn cargo_words(words: &[&str]) -> Kind {
    let (subcommand, rest) = match words {
        ["cargo", subcommand, rest @ ..] => (*subcommand, rest),
        _ => return Kind::Edit,
    };
    let mut sides = rest.splitn(2, |word| *word == "--");
    let before = sides.next().unwrap_or_default();
    let after = sides.next().unwrap_or_default();
    let whole = before.iter().all(|word| WHOLE.contains(word));
    match subcommand {
        // After `--` a test binary takes filters; clippy takes only lint levels.
        "test" => Kind::Test {
            whole: whole
                && after.iter().all(|word| {
                    ["--nocapture", "--show-output", "-q", "--quiet"].contains(word)
                        || word.starts_with("--test-threads")
                        || word.starts_with("--color")
                }),
        },
        "clippy" if !before.contains(&"--fix") => Kind::Clippy {
            whole: whole && before.contains(&"--all-targets"),
        },
        _ => Kind::Edit,
    }
}

/// What a command reported, from the `cargo`, `exec` or `shell` result.
struct Ran {
    exit_code: Option<i64>,
    errors: Option<u64>,
    warnings: Option<u64>,
    text: String,
}

impl Ran {
    fn parse(output: &str) -> Self {
        let value: Value = serde_json::from_str(output).unwrap_or_default();
        let count = |name: &str| value.get("counts")?.get(name)?.as_u64();
        let text = ["output", "stdout", "stderr"]
            .iter()
            .filter_map(|field| value.get(field).and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            exit_code: value.get("exit_code").and_then(Value::as_i64),
            errors: count("errors"),
            warnings: count("warnings"),
            text,
        }
    }

    /// Exit 0 with no warnings, from the parsed counts or, for a command run
    /// by hand, from its text.
    fn clean(&self) -> bool {
        let quiet = match (self.errors, self.warnings) {
            (Some(errors), Some(warnings)) => errors == 0 && warnings == 0,
            _ => !self.text.contains("warning") && !self.text.contains("error"),
        };
        self.exit_code == Some(0) && quiet
    }
}

/// The per-test lines and summaries of a `cargo test` run.
#[derive(Debug, Default)]
struct Tests {
    results: usize,
    passed_count: u64,
    any_failed: bool,
    passed: Vec<String>,
    failed: Vec<String>,
}

impl Tests {
    fn parse(text: &str) -> Self {
        let mut tests = Self::default();
        for line in text.lines().map(str::trim) {
            if let Some(summary) = line.strip_prefix("test result: ") {
                tests.results += 1;
                tests.any_failed |= !summary.starts_with("ok.");
                tests.passed_count += summary
                    .split(';')
                    .find_map(|part| {
                        part.trim()
                            .trim_start_matches("ok.")
                            .trim()
                            .strip_suffix(" passed")
                    })
                    .and_then(|number| number.trim().parse::<u64>().ok())
                    .unwrap_or(0);
            } else if let Some(test) = line.strip_prefix("test ") {
                if let Some(name) = test.strip_suffix(" ... FAILED") {
                    tests.failed.push(name.to_owned());
                } else if let Some(name) = test.strip_suffix(" ... ok") {
                    tests.passed.push(name.to_owned());
                }
            }
        }
        tests
    }

    fn all_passed(&self) -> bool {
        self.results > 0 && !self.any_failed && self.failed.is_empty() && self.passed_count > 0
    }
}

#[cfg(test)]
mod tests {
    use super::OutcomeWatch;
    use dino_protocol::ToolCall;
    use serde_json::Value;
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "id".into(),
            name: name.into(),
            arguments,
        }
    }

    fn cargo(command: &str) -> ToolCall {
        call("cargo", json!({"command": command}))
    }

    fn patch() -> ToolCall {
        call("apply_patch", json!({"path": "src/lib.rs"}))
    }

    fn tests_output(exit_code: i32, lines: &[&str]) -> String {
        json!({"exit_code": exit_code, "output": lines.join("\n")}).to_string()
    }

    fn passing() -> String {
        tests_output(
            0,
            &[
                "running 2 tests",
                "test tests::a ... ok",
                "test tests::b ... ok",
                "",
                "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
            ],
        )
    }

    fn failing(name: &str) -> String {
        tests_output(
            101,
            &[
                "running 2 tests",
                "test tests::a ... ok",
                &format!("test {name} ... FAILED"),
                "",
                "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out",
            ],
        )
    }

    fn clean() -> String {
        json!({"exit_code": 0, "counts": {"errors": 0, "warnings": 0}, "output": ""}).to_string()
    }

    #[test]
    fn green_tests_and_clippy_are_said_once() {
        let mut watch = OutcomeWatch::default();
        assert_eq!(watch.record(&patch(), "{}"), None);
        assert_eq!(watch.record(&cargo("test"), &passing()), None);
        let note = watch.record(&cargo("clippy"), &clean()).unwrap();
        assert!(note.contains("verified"), "{note}");
        assert_eq!(watch.record(&cargo("test"), &passing()), None);
    }

    #[test]
    fn an_edit_after_the_tests_passed_undoes_it() {
        let mut watch = OutcomeWatch::default();
        watch.record(&cargo("test"), &passing());
        watch.record(&patch(), "{}");
        assert_eq!(watch.record(&cargo("clippy"), &clean()), None);
        watch.record(&cargo("fmt"), "{}");
        assert_eq!(watch.record(&cargo("test"), &passing()), None);
        watch.record(
            &call("shell", json!({"command": "sed -i s/a/b/ src/lib.rs"})),
            "{}",
        );
        assert_eq!(watch.record(&cargo("clippy"), &clean()), None);
    }

    #[test]
    fn nothing_is_said_without_a_whole_passing_test_run() {
        let mut watch = OutcomeWatch::default();
        assert_eq!(watch.record(&cargo("clippy"), &clean()), None);
        assert_eq!(watch.record(&cargo("check"), &clean()), None);
        let filtered = call("cargo", json!({"command": "test", "filter": "a"}));
        assert_eq!(watch.record(&filtered, &passing()), None);
        let none = tests_output(0, &["test result: ok. 0 passed; 0 failed; 0 ignored"]);
        assert_eq!(watch.record(&cargo("test"), &none), None);
        let piped = call("shell", json!({"command": "cargo test 2>&1 | tail"}));
        assert_eq!(watch.record(&piped, &passing()), None);
        let narrowed = call(
            "exec",
            json!({"command": ["cargo", "test", "--", "strings"]}),
        );
        assert_eq!(watch.record(&narrowed, &passing()), None);
        assert!(watch.record(&cargo("clippy"), &clean()).is_none());
    }

    #[test]
    fn clippy_with_warnings_is_not_clean() {
        let mut watch = OutcomeWatch::default();
        watch.record(&cargo("test"), &passing());
        let warned = json!({"exit_code": 0, "counts": {"errors": 0, "warnings": 1}}).to_string();
        assert_eq!(watch.record(&cargo("clippy"), &warned), None);
        let by_hand = call(
            "exec",
            json!({"command": ["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]}),
        );
        let ran = json!({"exit_code": 0, "stdout": "", "stderr": "    Finished `dev` profile"});
        assert!(watch.record(&by_hand, &ran.to_string()).is_some());
    }

    #[test]
    fn a_test_failing_three_runs_running_after_changes_is_said_once() {
        let mut watch = OutcomeWatch::default();
        let mut notes = Vec::new();
        for _ in 0..4 {
            watch.record(&patch(), "{}");
            notes.push(watch.record(&cargo("test"), &failing("tests::strings")));
        }
        assert_eq!(notes.iter().filter(|note| note.is_some()).count(), 1);
        let note = notes[2].as_deref().unwrap();
        assert!(
            note.contains("`tests::strings` failed in 3 test runs"),
            "{note}"
        );
    }

    #[test]
    fn a_pass_or_a_rerun_without_changes_does_not_count() {
        let mut watch = OutcomeWatch::default();
        watch.record(&cargo("test"), &failing("tests::b"));
        watch.record(&cargo("test"), &failing("tests::b"));
        watch.record(&patch(), "{}");
        watch.record(&cargo("test"), &failing("tests::b"));
        watch.record(&patch(), "{}");
        watch.record(&cargo("test"), &passing());
        watch.record(&patch(), "{}");
        assert_eq!(watch.record(&cargo("test"), &failing("tests::b")), None);
    }

    #[test]
    fn a_build_failure_or_another_test_s_run_neither_counts_nor_breaks_it() {
        let mut watch = OutcomeWatch::default();
        let broken = tests_output(101, &["error[E0308]: mismatched types"]);
        let other = call("cargo", json!({"command": "test", "filter": "a"}));
        let other_passing = tests_output(0, &["test tests::a ... ok", "test result: ok. 1 passed"]);
        watch.record(&cargo("test"), &failing("tests::b"));
        watch.record(&patch(), "{}");
        assert_eq!(watch.record(&cargo("test"), &broken), None);
        watch.record(&cargo("test"), &failing("tests::b"));
        watch.record(&patch(), "{}");
        assert_eq!(watch.record(&other, &other_passing), None);
        assert!(watch.record(&cargo("test"), &failing("tests::b")).is_some());
    }
}
