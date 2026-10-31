//! Cargo's text output cut to what a model acts on: progress, passing tests
//! and repeats left out, the rest as Cargo wrote it.

use std::collections::HashSet;
use std::path::Path;

/// Cargo's status verbs. It right-aligns them in twelve columns.
const STATUS_VERBS: [&str; 20] = [
    "Adding",
    "Blocking",
    "Building",
    "Checking",
    "Compiling",
    "Doc-tests",
    "Documenting",
    "Downloaded",
    "Downloading",
    "Fetching",
    "Finished",
    "Fresh",
    "Locking",
    "Packaging",
    "Removing",
    "Running",
    "Unchanged",
    "Unpacking",
    "Updating",
    "Waiting",
];

/// Diagnostics shown whole before the rest are cut to their location.
const WHOLE_DIAGNOSTICS: usize = 5;
/// Diagnostics shown at all, whole or by location.
const SHOWN_DIAGNOSTICS: usize = 25;
/// Lines of `fmt --check` differences shown; every file's heading is kept.
const FORMAT_DIFF_LINES: usize = 40;

/// What [`trim`] kept, and an account of what it did not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Trimmed {
    pub text: String,
    /// Progress, passing-test and summary lines left out.
    pub lines_left_out: usize,
    /// Diagnostics cut to their heading and location.
    pub diagnostics_shortened: usize,
    /// Diagnostics, or `fmt --check` lines, left out entirely.
    pub left_out: usize,
}

impl Trimmed {
    /// A line for the tool result, or None when nothing was cut.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.lines_left_out > 0 {
            parts.push(format!(
                "{} lines of progress and passing tests left out",
                self.lines_left_out
            ));
        }
        if self.diagnostics_shortened > 0 {
            parts.push(format!(
                "{} repeated or later diagnostics cut to their location",
                self.diagnostics_shortened
            ));
        }
        if self.left_out > 0 {
            parts.push(format!("{} more left out", self.left_out));
        }
        (!parts.is_empty()).then(|| format!("{}; pass full for all of it", parts.join(", ")))
    }
}

/// Which harness wrote the text, for the rules only it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Test,
    FormatCheck,
    Other,
}

pub fn trim(text: &str, root: &Path, source: Source) -> Trimmed {
    let prefix = format!("{}/", root.display());
    let text = text.replace(&prefix, "");
    let mut trimmed = Trimmed::default();
    let mut kept = Vec::new();
    for line in text.lines() {
        if is_progress(line) || (source == Source::Test && is_passing_test(line)) {
            trimmed.lines_left_out += 1;
        } else {
            kept.push(line);
        }
    }
    let kept = if source == Source::FormatCheck {
        format_diff(&kept, &mut trimmed)
    } else {
        diagnostics(&kept, &mut trimmed)
    };
    trimmed.text = squeeze_blank_lines(&kept);
    trimmed
}

fn is_progress(line: &str) -> bool {
    let Some((verb, rest)) = line.split_at_checked(12) else {
        return false;
    };
    let verb = verb.trim_start();
    if !rest.starts_with(' ') || !STATUS_VERBS.contains(&verb) {
        return false;
    }
    // `cargo add` names what it put in the manifest with this verb, and that is
    // the one line of its output worth reading.
    !(verb == "Adding" && rest.contains(" to "))
}

fn is_passing_test(line: &str) -> bool {
    let count = line.strip_prefix("running ").and_then(|rest| {
        rest.strip_suffix(" tests")
            .or_else(|| rest.strip_suffix(" test"))
    });
    if count.is_some_and(|count| !count.is_empty() && count.chars().all(|c| c.is_ascii_digit())) {
        return true;
    }
    // A test binary with nothing in it, or nothing left after the filter.
    (line.starts_with("test ") && line.ends_with(" ... ok"))
        || line.starts_with("test result: ok. 0 passed; 0 failed; 0 ignored;")
}

/// A line a rendered diagnostic starts with, such as `error[E0308]: ...`.
fn heading_level(line: &str) -> Option<&'static str> {
    let level = if line.starts_with("error") {
        "error"
    } else if line.starts_with("warning") {
        "warning"
    } else {
        return None;
    };
    let rest = &line[level.len()..];
    let rest = match rest.strip_prefix('[') {
        Some(code) => &code[code.find(']')? + 1..],
        None => rest,
    };
    rest.starts_with(": ").then_some(level)
}

/// Cargo's tally of warnings, which repeats what the diagnostics above it say.
fn is_tally(line: &str) -> bool {
    line.starts_with("warning: `") && line.contains(" generated ") && line.contains(" warning")
}

/// The heading with its names taken out, so the same lint at two sites matches.
fn kind(heading: &str) -> String {
    heading
        .split('`')
        .step_by(2)
        .collect::<Vec<_>>()
        .join("`…`")
}

/// Removes duplicate diagnostics and cuts later and repeated ones to their
/// heading and location.
fn diagnostics<'a>(lines: &[&'a str], trimmed: &mut Trimmed) -> Vec<&'a str> {
    let mut kept = Vec::with_capacity(lines.len());
    let mut seen: HashSet<Vec<&str>> = HashSet::new();
    let mut kinds: HashSet<String> = HashSet::new();
    let (mut whole, mut shown) = (0, 0);
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if is_tally(line) {
            trimmed.lines_left_out += 1;
            index += 1;
            continue;
        }
        let Some(level) = heading_level(line) else {
            kept.push(line);
            index += 1;
            continue;
        };
        let end = lines[index + 1..]
            .iter()
            .position(|next| next.is_empty() || heading_level(next).is_some())
            .map_or(lines.len(), |offset| index + 1 + offset);
        let block = &lines[index..end];
        index = end;
        // One line, such as `error: could not compile`, is a verdict, not a diagnostic.
        if block.len() == 1 {
            kept.push(line);
            continue;
        }
        if !seen.insert(block.to_vec()) {
            trimmed.lines_left_out += block.len();
            continue;
        }
        if shown == SHOWN_DIAGNOSTICS {
            trimmed.left_out += 1;
            continue;
        }
        shown += 1;
        let new_kind = kinds.insert(kind(line));
        if whole < WHOLE_DIAGNOSTICS && (level == "error" || new_kind) {
            whole += 1;
            kept.extend_from_slice(block);
        } else {
            trimmed.diagnostics_shortened += 1;
            kept.push(line);
            kept.extend(
                block
                    .iter()
                    .find(|line| line.trim_start().starts_with("-->")),
            );
        }
        kept.push("");
    }
    kept
}

/// Keeps every file `fmt --check` names and the first of its differences.
fn format_diff<'a>(lines: &[&'a str], trimmed: &mut Trimmed) -> Vec<&'a str> {
    let mut body = 0;
    lines
        .iter()
        .copied()
        .filter(|line| {
            if line.starts_with("Diff in ") || body < FORMAT_DIFF_LINES {
                body += usize::from(!line.starts_with("Diff in "));
                true
            } else {
                trimmed.left_out += 1;
                false
            }
        })
        .collect()
}

fn squeeze_blank_lines(lines: &[&str]) -> String {
    let mut text = String::new();
    let mut blank = true;
    for line in lines {
        if line.trim().is_empty() {
            if !blank {
                text.push('\n');
            }
            blank = true;
        } else {
            text.push_str(line);
            text.push('\n');
            blank = false;
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    const ROOT: &str = "/work/fixture";

    fn run(text: &str, source: Source) -> Trimmed {
        trim(text, Path::new(ROOT), source)
    }

    #[test]
    fn progress_and_passing_tests_go_and_failures_stay() {
        let text = "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n\nrunning 2 tests\ntest a ... ok\ntest b ... FAILED\n\nfailures:\n\n---- b stdout ----\n\nthread 'b' panicked at tests/t.rs:3:5:\nboom\n\nfailures:\n    b\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n   Compiling fixture v0.1.0 (/work/fixture)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.25s\n     Running unittests src/lib.rs (target/debug/deps/fixture-1)\n     Running tests/t.rs (target/debug/deps/t-2)\n   Doc-tests fixture\nerror: test failed, to rerun pass `--test t`\n";

        let trimmed = run(text, Source::Test);

        assert_eq!(
            trimmed.text,
            "test b ... FAILED\n\nfailures:\n\n---- b stdout ----\n\nthread 'b' panicked at tests/t.rs:3:5:\nboom\n\nfailures:\n    b\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\nerror: test failed, to rerun pass `--test t`\n"
        );
        assert_eq!(trimmed.lines_left_out, 9);
        assert!(trimmed.summary().unwrap().contains("pass full"));
    }

    #[test]
    fn a_program_line_that_only_looks_like_progress_is_kept() {
        let text = "Compiling the report\n  Finished: 3 rows\ntest x ... ok\n";

        let trimmed = run(text, Source::Other);

        assert_eq!(trimmed.text, text);
        assert_eq!(trimmed.summary(), None);
    }

    #[test]
    fn what_cargo_add_put_in_the_manifest_is_kept() {
        let text = "    Updating crates.io index\n      Adding serde v1.0.228 to dependencies\n             Features:\n             + derive\n     Locking 3 packages to latest compatible versions\n      Adding serde_derive v1.0.228\n";

        let trimmed = run(text, Source::Other);

        assert_eq!(
            trimmed.text,
            "      Adding serde v1.0.228 to dependencies\n             Features:\n             + derive\n"
        );
    }

    fn warning(name: &str, line: u32) -> String {
        format!(
            "warning: unused variable: `{name}`\n --> src/lib.rs:{line}:9\n  |\n{line} |     let {name} = 1;\n  |         ^ help: prefix it with an underscore\n  |\n  = note: `#[warn(unused_variables)]` on by default\n\n"
        )
    }

    #[test]
    fn a_repeated_warning_is_cut_to_its_location_and_a_duplicate_dropped() {
        let text = format!(
            "{}{}{}warning: `fixture` (lib) generated 2 warnings\nerror: could not compile `fixture`\n",
            warning("a", 3),
            warning("b", 7),
            warning("a", 3)
        );

        let trimmed = run(&text, Source::Other);

        assert_eq!(
            trimmed.text,
            format!(
                "{}warning: unused variable: `b`\n --> src/lib.rs:7:9\n\nerror: could not compile `fixture`\n",
                warning("a", 3)
            )
        );
        assert_eq!(trimmed.diagnostics_shortened, 1);
    }

    #[test]
    fn errors_beyond_the_first_few_are_counted_not_shown() {
        let text = (1..=SHOWN_DIAGNOSTICS + 3).fold(String::new(), |mut text, n| {
            write!(
                &mut text,
                "error[E0308]: mismatched types
 --> src/lib.rs:{n}:1
  |
  = note: n

"
            )
            .unwrap();
            text
        });

        let trimmed = run(&text, Source::Test);

        assert_eq!(trimmed.text.matches("= note").count(), WHOLE_DIAGNOSTICS);
        assert_eq!(
            trimmed.diagnostics_shortened,
            SHOWN_DIAGNOSTICS - WHOLE_DIAGNOSTICS
        );
        assert_eq!(trimmed.left_out, 3);
        assert!(trimmed.text.contains("src/lib.rs:25:1"));
        assert!(!trimmed.text.contains("src/lib.rs:26:1"));
    }

    #[test]
    fn a_format_check_keeps_every_file_and_the_first_differences() {
        let body = (0..FORMAT_DIFF_LINES).fold(String::new(), |mut body, n| {
            write!(
                &mut body,
                "-a{n}
+b{n}
"
            )
            .unwrap();
            body
        });
        let text = format!(
            "Diff in /work/fixture/src/a.rs:1:\n{body}Diff in /work/fixture/src/b.rs:9:\n-x\n+y\n"
        );

        let trimmed = run(&text, Source::FormatCheck);

        assert!(trimmed.text.starts_with("Diff in src/a.rs:1:\n-a0\n"));
        assert!(trimmed.text.ends_with("Diff in src/b.rs:9:\n"));
        assert_eq!(trimmed.left_out, FORMAT_DIFF_LINES + 2);
    }
}
