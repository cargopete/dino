//! Commands that only read the repository, which need no approval.
//!
//! Deliberately narrow: a closed set of git subcommands, refused if any
//! argument could write a file, run a configured program or reach outside the
//! workspace, and for `shell` only a line made of those, `echo`, and filters
//! reading their standard input. Anything else asks as before.

/// Git subcommands that read and never write, whatever their arguments,
/// beyond the few [`writes_or_escapes`] refuses.
const GIT_READS: [&str; 10] = [
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "rev-list",
    "merge-base",
    "blame",
    "ls-files",
    "ls-tree",
];

/// Flags of `git branch` that list rather than create, move or delete.
const BRANCH_LISTS: [&str; 18] = [
    "--list",
    "-l",
    "--show-current",
    "-a",
    "--all",
    "-r",
    "--remotes",
    "-v",
    "-vv",
    "--verbose",
    "--merged",
    "--no-merged",
    "--contains",
    "--no-contains",
    "--sort",
    "--format",
    "--points-at",
    "--no-color",
];

/// Whether `git <arguments>` only reads the repository.
pub(crate) fn git_reads_only<S: AsRef<str>>(arguments: &[S]) -> bool {
    let Some((subcommand, rest)) = arguments.split_first() else {
        return false;
    };
    let rest: Vec<&str> = rest.iter().map(AsRef::as_ref).collect();
    if rest.iter().any(|argument| writes_or_escapes(argument)) {
        return false;
    }
    match subcommand.as_ref() {
        "branch" => branch_lists(&rest),
        subcommand => GIT_READS.contains(&subcommand),
    }
}

/// `--output` writes a file, `--ext-diff` runs a configured program, and
/// `--no-index`, `--contents` or a path outside the workspace read past it.
fn writes_or_escapes(argument: &str) -> bool {
    ["--output", "--ext-diff", "--no-index", "--contents"]
        .iter()
        .any(|flag| argument.starts_with(flag))
        || outside(argument)
}

fn outside(argument: &str) -> bool {
    argument.starts_with('/')
        || argument.starts_with('~')
        || argument.split('/').any(|part| part == "..")
}

/// Without `--list`, a bare name creates a branch.
fn branch_lists(arguments: &[&str]) -> bool {
    let listing = arguments
        .iter()
        .any(|argument| ["--list", "-l"].contains(argument));
    arguments.iter().all(|argument| {
        if argument.starts_with('-') {
            let flag = argument.split('=').next().unwrap_or_default();
            BRANCH_LISTS.contains(&flag)
        } else {
            listing
        }
    })
}

/// Whether a `shell` line only reads: read-only git commands and `echo`,
/// joined by `;`, `&&` or `||`, each piped only into filters of its output.
pub(crate) fn line_reads_only(line: &str) -> bool {
    let Some(tokens) = tokenize(line) else {
        return false;
    };
    let mut pipelines: Vec<Vec<Vec<String>>> = vec![vec![Vec::new()]];
    for token in tokens {
        match token {
            Token::Word(word) => {
                if let Some(command) = pipelines.last_mut().and_then(|stages| stages.last_mut()) {
                    command.push(word);
                }
            }
            Token::Pipe => {
                if let Some(stages) = pipelines.last_mut() {
                    stages.push(Vec::new());
                }
            }
            Token::Then => pipelines.push(vec![Vec::new()]),
        }
    }
    pipelines.iter().all(|stages| {
        let Some((first, filters)) = stages.split_first() else {
            return false;
        };
        let source = match first.split_first() {
            Some((program, arguments)) if program == "git" => git_reads_only(arguments),
            Some((program, _)) => program == "echo",
            None => false,
        };
        source && filters.iter().all(|filter| filters_input(filter))
    })
}

/// A program that reads its standard input and writes only its output.
fn filters_input(command: &[String]) -> bool {
    let Some((program, arguments)) = command.split_first() else {
        return false;
    };
    let numeric = |argument: &String| argument.chars().all(|c| c.is_ascii_digit());
    match program.as_str() {
        "head" | "tail" | "wc" | "cut" | "uniq" => arguments
            .iter()
            .all(|argument| argument.starts_with('-') || numeric(argument)),
        // A pattern or a file in the workspace; never a flag that recurses or
        // reads patterns from elsewhere.
        "grep" => arguments.iter().all(|argument| {
            if let Some(flag) = argument.strip_prefix("--") {
                !["file", "recursive", "dereference-recursive", "directories"]
                    .iter()
                    .any(|refused| flag.starts_with(refused))
            } else if let Some(cluster) = argument.strip_prefix('-') {
                !cluster.contains(['r', 'R', 'f', 'd'])
            } else {
                !outside(argument)
            }
        }),
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Pipe,
    /// `;`, `&&` or `||`.
    Then,
}

/// Splits a line the way `sh` would, or `None` when it holds anything that
/// expands, redirects, globs, substitutes or backgrounds.
fn tokenize(line: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                tokens.extend(word.take().map(Token::Word));
            }
            ';' | '|' | '&' => {
                tokens.extend(word.take().map(Token::Word));
                let doubled = chars.next_if_eq(&c).is_some();
                tokens.push(match (c, doubled) {
                    ('|', false) => Token::Pipe,
                    (';', false) | ('&' | '|', true) => Token::Then,
                    _ => return None,
                });
            }
            '\'' => {
                let text = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => text.push(c),
                    }
                }
            }
            '"' => {
                let text = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' | '\\' | '!' => return None,
                        c => text.push(c),
                    }
                }
            }
            '~' if word.is_none() => return None,
            c if c.is_ascii_alphanumeric() || "-_./:@^~=,+%".contains(c) => {
                word.get_or_insert_with(String::new).push(c);
            }
            _ => return None,
        }
    }
    tokens.extend(word.take().map(Token::Word));
    Some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(line: &str) -> bool {
        git_reads_only(&line.split_whitespace().collect::<Vec<_>>())
    }

    #[test]
    fn reading_subcommands_are_read_only_and_writing_ones_are_not() {
        for line in [
            "status --short",
            "log --oneline -15 main..HEAD",
            "diff --stat origin/main...HEAD -- Cargo.lock",
            "show origin/main:docs/README.md",
            "rev-list --left-right --count origin/main...HEAD",
            "merge-base --is-ancestor HEAD origin/main",
            "branch --list pete/*",
            "branch -a --sort=-committerdate",
            "branch --show-current",
        ] {
            assert!(git(line), "{line}");
        }
        for line in [
            "",
            "commit -m x",
            "push",
            "checkout src/lib.rs",
            "branch pete/new",
            "branch -D pete/old",
            "diff --output=patch.txt",
            "diff --ext-diff",
            "diff --no-index /etc/passwd /dev/null",
            "diff /etc/passwd src/lib.rs",
            "blame --contents=/etc/passwd src/lib.rs",
            "-C /elsewhere status",
            "-c core.pager=evil log",
        ] {
            assert!(!git(line), "{line}");
        }
    }

    #[test]
    fn a_line_of_git_reads_and_output_filters_is_read_only() {
        for line in [
            "git status --short; git log --oneline -15; git diff --stat",
            "git diff src/feed.rs | grep \"^[-+]\" | head",
            "git log --format='%h %s' -3 && echo ok",
            "git blame -L 297,297 --porcelain src/lease.rs | head -1",
            "git show HEAD~1 | tail -n 5 | wc -l",
            "git rev-list --count HEAD || echo none",
        ] {
            assert!(line_reads_only(line), "{line}");
        }
    }

    #[test]
    fn a_line_that_could_write_run_or_expand_anything_asks() {
        for line in [
            "git status > out.txt",
            "git log $(rm -rf src)",
            "git log `id`",
            "git diff | sh",
            "git diff | tee out.txt",
            "git status & rm x",
            "git status\nrm x",
            "git log ~/elsewhere",
            "git log | grep -r secret ~",
            "git log | grep -f /etc/patterns",
            "git log | uniq - out.txt",
            "cat src/lib.rs",
            "git log *",
            "git log \"$HOME\"",
            "git status;",
            "git log | ",
            "git log 'unterminated",
        ] {
            assert!(!line_reads_only(line), "{line}");
        }
    }
}
