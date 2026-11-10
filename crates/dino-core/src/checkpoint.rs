//! Checkpoints of the working tree, as git objects, for `/rewind`.
//!
//! Before each submission the workspace is recorded as a commit on
//! `refs/dino/checkpoints/<conversation>/<n>`, and after it on `<n>-after`.
//! Trees are built through a temporary index, so `HEAD`, the index and the
//! stash never move. Tracked and untracked files are captured; ignored ones
//! are not.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use thiserror::Error;

const PREFIX: &str = "refs/dino/checkpoints";

/// How long a conversation's checkpoints outlive its last one.
pub const KEPT_FOR: Duration = Duration::from_secs(14 * 24 * 60 * 60);

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("not a git work tree, so there are no checkpoints")]
    NotARepository,
    #[error("{0}")]
    Failed(String),
}

/// One submission's checkpoint, as `/rewind` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub number: u32,
    pub unix_seconds: u64,
    /// What was submitted, whole.
    pub prompt: String,
}

/// Why a changed path is not restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Someone changed it after the conversation last did.
    ChangedSince,
    /// The conversation never changed it.
    NotTouched,
    Submodule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Left {
    pub path: String,
    pub why: Why,
}

/// What rewinding the files to a checkpoint does, relative to the workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub restore: Vec<String>,
    pub remove: Vec<String>,
    pub left: Vec<Left>,
    entries: HashMap<String, Entry>,
}

impl Plan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.restore.is_empty() && self.remove.is_empty()
    }
}

/// A files rewind done: what it did, what failed, and the tree it replaced.
#[derive(Debug)]
pub struct Rewound {
    pub plan: Plan,
    pub failed: Vec<(String, String)>,
    replaced: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    mode: String,
    oid: String,
}

/// The workspace's place in its repository.
struct Repo {
    workspace: PathBuf,
    top: PathBuf,
    git_dir: PathBuf,
    /// The workspace relative to `top`, with a trailing slash, or empty.
    prefix: String,
}

impl Repo {
    fn open(workspace: &Path) -> Result<Self, CheckpointError> {
        let found = git(
            workspace,
            &[
                "rev-parse",
                "--absolute-git-dir",
                "--show-toplevel",
                "--show-prefix",
            ],
        )
        .map_err(|_| CheckpointError::NotARepository)?;
        let found = String::from_utf8_lossy(&found).into_owned();
        let mut lines = found.lines();
        let (Some(git_dir), Some(top)) = (lines.next(), lines.next()) else {
            return Err(CheckpointError::NotARepository);
        };
        Ok(Self {
            workspace: workspace.to_path_buf(),
            top: PathBuf::from(top),
            git_dir: PathBuf::from(git_dir),
            prefix: lines.next().unwrap_or_default().to_owned(),
        })
    }

    fn git(&self, arguments: &[&str]) -> Result<String, CheckpointError> {
        git(&self.top, arguments).map(|out| String::from_utf8_lossy(&out).trim().to_owned())
    }

    /// The workspace's tree as it is now, written to the object store.
    fn snapshot(&self) -> Result<String, CheckpointError> {
        let index = TempIndex(self.git_dir.join(format!(
            "dino-checkpoint-{}-{}.index",
            std::process::id(),
            unix_nanos()
        )));
        // Starting from the real index keeps its stat cache, and what it stages.
        let _ = std::fs::copy(self.git_dir.join("index"), &index.0);
        let env = [("GIT_INDEX_FILE", index.0.as_os_str())];
        run(
            &self.workspace,
            &["add", "-A", "--", ".", ":(exclude).dino"],
            &env,
            None,
        )?;
        let root = String::from_utf8_lossy(&run(&self.workspace, &["write-tree"], &env, None)?)
            .trim()
            .to_owned();
        let prefix = self.prefix.trim_end_matches('/');
        if prefix.is_empty() {
            return Ok(root);
        }
        self.git(&["rev-parse", "--verify", "-q", &format!("{root}:{prefix}")])
            .or_else(|_| self.git(&["mktree"]))
    }

    fn commit(
        &self,
        reference: &str,
        tree: &str,
        parent: Option<&str>,
        message: &str,
    ) -> Result<String, CheckpointError> {
        let mut arguments = vec!["commit-tree", tree];
        if let Some(parent) = parent {
            arguments.extend(["-p", parent]);
        }
        arguments.extend(["-F", "-"]);
        let env = [
            ("GIT_AUTHOR_NAME", "dino".as_ref()),
            ("GIT_AUTHOR_EMAIL", "dino@localhost".as_ref()),
            ("GIT_COMMITTER_NAME", "dino".as_ref()),
            ("GIT_COMMITTER_EMAIL", "dino@localhost".as_ref()),
        ];
        let commit = run(&self.top, &arguments, &env, Some(message.as_bytes()))?;
        let commit = String::from_utf8_lossy(&commit).trim().to_owned();
        self.git(&["update-ref", reference, &commit])?;
        Ok(commit)
    }

    /// Every file in a tree or commit, by path.
    fn files(&self, treeish: &str) -> Result<HashMap<String, Entry>, CheckpointError> {
        let listing = git(&self.top, &["ls-tree", "-r", "-z", "--full-tree", treeish])?;
        Ok(listing
            .split(|byte| *byte == 0)
            .filter_map(|record| {
                let record = std::str::from_utf8(record).ok()?;
                let (meta, path) = record.split_once('\t')?;
                let mut fields = meta.split(' ');
                let (mode, _, oid) = (fields.next()?, fields.next()?, fields.next()?);
                Some((
                    path.to_owned(),
                    Entry {
                        mode: mode.to_owned(),
                        oid: oid.to_owned(),
                    },
                ))
            })
            .collect())
    }

    /// The conversation's refs: name after the conversation's prefix, and object.
    fn refs(&self, conversation: &str) -> Result<Vec<(String, String)>, CheckpointError> {
        let base = format!("{PREFIX}/{conversation}/");
        let listing = self.git(&["for-each-ref", "--format=%(refname) %(objectname)", &base])?;
        Ok(listing
            .lines()
            .filter_map(|line| {
                let (name, object) = line.split_once(' ')?;
                Some((name.strip_prefix(&base)?.to_owned(), object.to_owned()))
            })
            .collect())
    }

    /// Numbered checkpoints, ascending: before, and after when it was recorded.
    fn numbered(
        &self,
        conversation: &str,
    ) -> Result<Vec<(u32, String, Option<String>)>, CheckpointError> {
        let refs = self.refs(conversation)?;
        let after: HashMap<&str, &str> = refs
            .iter()
            .filter_map(|(name, object)| Some((name.strip_suffix("-after")?, object.as_str())))
            .collect();
        let mut numbered: Vec<_> = refs
            .iter()
            .filter_map(|(name, object)| {
                let number = name.parse().ok()?;
                Some((
                    number,
                    object.clone(),
                    after.get(name.as_str()).map(|after| (*after).to_owned()),
                ))
            })
            .collect();
        numbered.sort_by_key(|(number, _, _)| *number);
        Ok(numbered)
    }

    fn delete(&self, references: &[String]) -> Result<(), CheckpointError> {
        if references.is_empty() {
            return Ok(());
        }
        let commands = references
            .iter()
            .fold(String::new(), |mut commands, reference| {
                let _ = writeln!(commands, "delete {reference}");
                commands
            });
        run(
            &self.top,
            &["update-ref", "--stdin"],
            &[],
            Some(commands.as_bytes()),
        )
        .map(drop)
    }
}

/// Removed on drop, with any lock git left beside it.
struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("index.lock"));
    }
}

fn check_id(conversation: &str) -> Result<(), CheckpointError> {
    let fine = !conversation.is_empty()
        && !conversation.starts_with('.')
        && conversation
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if fine {
        Ok(())
    } else {
        Err(CheckpointError::Failed(format!(
            "{conversation:?} cannot name a checkpoint"
        )))
    }
}

/// Where the conversation as it stood before checkpoint `number` is kept.
#[must_use]
pub fn state_path(workspace: &Path, conversation: &str, number: u32) -> PathBuf {
    states(workspace, conversation).join(format!("{number}.state.json"))
}

fn states(workspace: &Path, conversation: &str) -> PathBuf {
    workspace
        .join(".dino")
        .join("checkpoints")
        .join(conversation)
}

/// Records the workspace before a submission, and returns its number.
///
/// # Errors
///
/// Returns [`CheckpointError`] outside a git work tree, or when git fails.
pub fn begin(workspace: &Path, conversation: &str, prompt: &str) -> Result<u32, CheckpointError> {
    check_id(conversation)?;
    let repo = Repo::open(workspace)?;
    let number = repo
        .numbered(conversation)?
        .last()
        .map_or(1, |(number, _, _)| number + 1);
    let tree = repo.snapshot()?;
    repo.commit(
        &format!("{PREFIX}/{conversation}/{number}"),
        &tree,
        None,
        prompt,
    )?;
    Ok(number)
}

/// Records the workspace after submission `number`, so what it changed can be
/// told apart from what anyone else did.
///
/// # Errors
///
/// Returns [`CheckpointError`] outside a git work tree, or when git fails.
pub fn end(workspace: &Path, conversation: &str, number: u32) -> Result<(), CheckpointError> {
    check_id(conversation)?;
    let repo = Repo::open(workspace)?;
    let before = format!("{PREFIX}/{conversation}/{number}");
    let parent = repo.git(&["rev-parse", "--verify", "-q", &before])?;
    let tree = repo.snapshot()?;
    repo.commit(&format!("{before}-after"), &tree, Some(&parent), "after")
        .map(drop)
}

/// The conversation's checkpoints, oldest first.
///
/// # Errors
///
/// Returns [`CheckpointError`] outside a git work tree, or when git fails.
pub fn list(workspace: &Path, conversation: &str) -> Result<Vec<Checkpoint>, CheckpointError> {
    check_id(conversation)?;
    let repo = Repo::open(workspace)?;
    repo.numbered(conversation)?
        .into_iter()
        .map(|(number, commit, _)| {
            let text = repo.git(&["cat-file", "commit", &commit])?;
            let (header, prompt) = text.split_once("\n\n").unwrap_or((&text, ""));
            let unix_seconds = header
                .lines()
                .find_map(|line| line.strip_prefix("committer "))
                .and_then(|line| line.rsplit(' ').nth(1))
                .and_then(|seconds| seconds.parse().ok())
                .unwrap_or_default();
            Ok(Checkpoint {
                number,
                unix_seconds,
                prompt: prompt.to_owned(),
            })
        })
        .collect()
}

/// What rewinding the files to checkpoint `number` would do now.
///
/// # Errors
///
/// Returns [`CheckpointError`] when there is no such checkpoint, or git fails.
pub fn plan(workspace: &Path, conversation: &str, number: u32) -> Result<Plan, CheckpointError> {
    check_id(conversation)?;
    let repo = Repo::open(workspace)?;
    let now = repo.snapshot()?;
    plan_against(&repo, conversation, number, &now)
}

fn plan_against(
    repo: &Repo,
    conversation: &str,
    number: u32,
    now: &str,
) -> Result<Plan, CheckpointError> {
    let numbered = repo.numbered(conversation)?;
    let Some((_, target, _)) = numbered.iter().find(|(n, _, _)| *n == number) else {
        return Err(CheckpointError::Failed(format!(
            "there is no checkpoint {number}"
        )));
    };
    let base = repo.files(target)?;
    // What the conversation last left in each path it changed, from the
    // checkpoint on. A submission without an after was cut off, and what it
    // did cannot be told from anyone else's work, so it is left alone.
    let mut wrote: HashMap<String, Option<Entry>> = HashMap::new();
    for (_, before, after) in numbered.iter().filter(|(n, _, _)| *n >= number) {
        let Some(after) = after else { continue };
        let (before, after) = (repo.files(before)?, repo.files(after)?);
        for path in changed(&before, &after) {
            wrote.insert(path.clone(), after.get(&path).cloned());
        }
    }
    let current = repo.files(now)?;
    let mut plan = Plan::default();
    for path in changed(&base, &current) {
        if path.starts_with(".dino/") {
            continue;
        }
        let why = match wrote.get(&path) {
            None => Some(Why::NotTouched),
            Some(last) if last.as_ref() != current.get(&path) => Some(Why::ChangedSince),
            Some(_)
                if [base.get(&path), current.get(&path)]
                    .into_iter()
                    .flatten()
                    .any(|entry| entry.mode == "160000") =>
            {
                Some(Why::Submodule)
            }
            Some(_) => None,
        };
        match (why, base.get(&path)) {
            (Some(why), _) => plan.left.push(Left { path, why }),
            (None, Some(entry)) => {
                plan.entries.insert(path.clone(), entry.clone());
                plan.restore.push(path);
            }
            (None, None) => plan.remove.push(path),
        }
    }
    Ok(plan)
}

fn changed(from: &HashMap<String, Entry>, to: &HashMap<String, Entry>) -> BTreeSet<String> {
    from.keys()
        .chain(to.keys())
        .filter(|path| from.get(*path) != to.get(*path))
        .cloned()
        .collect()
}

/// Rewinds the files the conversation changed since checkpoint `number`, and
/// leaves everything else as it is.
///
/// # Errors
///
/// Returns [`CheckpointError`] when there is no such checkpoint, or git fails.
/// A path that cannot be written is in [`Rewound::failed`] instead.
pub fn rewind_files(
    workspace: &Path,
    conversation: &str,
    number: u32,
) -> Result<Rewound, CheckpointError> {
    check_id(conversation)?;
    let repo = Repo::open(workspace)?;
    let now = repo.snapshot()?;
    let plan = plan_against(&repo, conversation, number, &now)?;
    let mut failed = Vec::new();
    // Removals first, so a directory that was a file can become one again.
    for path in &plan.remove {
        if let Err(error) = remove(workspace, path) {
            failed.push((path.clone(), error.to_string()));
        }
    }
    for path in &plan.restore {
        if let Err(error) = write(&repo, workspace, path, &plan.entries[path]) {
            failed.push((path.clone(), error.to_string()));
        }
    }
    Ok(Rewound {
        plan,
        failed,
        replaced: now,
    })
}

/// Records a files rewind as checkpoint `number`, so rewinding to it puts back
/// what the rewind replaced, and what it restored counts as the conversation's.
///
/// # Errors
///
/// Returns [`CheckpointError`] when git fails.
pub fn record_rewind(
    workspace: &Path,
    conversation: &str,
    number: u32,
    prompt: &str,
    rewound: &Rewound,
) -> Result<(), CheckpointError> {
    check_id(conversation)?;
    let repo = Repo::open(workspace)?;
    let before = format!("{PREFIX}/{conversation}/{number}");
    let parent = repo.commit(&before, &rewound.replaced, None, prompt)?;
    let tree = repo.snapshot()?;
    repo.commit(&format!("{before}-after"), &tree, Some(&parent), "after")
        .map(drop)
}

/// The number the next checkpoint will take.
///
/// # Errors
///
/// Returns [`CheckpointError`] outside a git work tree, or when git fails.
pub fn next(workspace: &Path, conversation: &str) -> Result<u32, CheckpointError> {
    check_id(conversation)?;
    Ok(Repo::open(workspace)?
        .numbered(conversation)?
        .last()
        .map_or(1, |(number, _, _)| number + 1))
}

/// Drops checkpoint `from` and every later one, with the conversations kept
/// beside them, as a conversation rewound to `from` no longer has them.
///
/// # Errors
///
/// Returns [`CheckpointError`] outside a git work tree, or when git fails.
pub fn truncate(workspace: &Path, conversation: &str, from: u32) -> Result<(), CheckpointError> {
    check_id(conversation)?;
    let repo = Repo::open(workspace)?;
    let mut doomed = Vec::new();
    for (number, _, after) in repo.numbered(conversation)? {
        if number >= from {
            doomed.push(format!("{PREFIX}/{conversation}/{number}"));
            if after.is_some() {
                doomed.push(format!("{PREFIX}/{conversation}/{number}-after"));
            }
            let _ = std::fs::remove_file(state_path(workspace, conversation, number));
        }
    }
    repo.delete(&doomed)
}

/// Gives conversation `to` every checkpoint `from` has, and returns how many.
///
/// # Errors
///
/// Returns [`CheckpointError`] outside a git work tree, or when git fails.
pub fn copy(workspace: &Path, from: &str, to: &str) -> Result<u32, CheckpointError> {
    check_id(from)?;
    check_id(to)?;
    let repo = Repo::open(workspace)?;
    let refs = repo.refs(from)?;
    let commands = refs
        .iter()
        .fold(String::new(), |mut commands, (name, object)| {
            let _ = writeln!(commands, "update {PREFIX}/{to}/{name} {object}");
            commands
        });
    if !commands.is_empty() {
        run(
            &repo.top,
            &["update-ref", "--stdin"],
            &[],
            Some(commands.as_bytes()),
        )?;
    }
    let mut count = 0;
    for (name, _) in &refs {
        let Ok(number) = name.parse::<u32>() else {
            continue;
        };
        count += 1;
        let source = state_path(workspace, from, number);
        if source.is_file() {
            let target = state_path(workspace, to, number);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| CheckpointError::Failed(error.to_string()))?;
            }
            std::fs::copy(&source, &target)
                .map_err(|error| CheckpointError::Failed(error.to_string()))?;
        }
    }
    Ok(count)
}

/// Removes the checkpoints of every conversation whose newest is older than
/// `age`, and returns how many conversations went.
///
/// # Errors
///
/// Returns [`CheckpointError`] outside a git work tree, or when git fails.
pub fn prune(workspace: &Path, age: Duration) -> Result<usize, CheckpointError> {
    let repo = Repo::open(workspace)?;
    let listing = repo.git(&[
        "for-each-ref",
        "--format=%(refname) %(committerdate:unix)",
        PREFIX,
    ])?;
    let mut newest: HashMap<&str, (u64, Vec<String>)> = HashMap::new();
    for line in listing.lines() {
        let Some((name, seconds)) = line.split_once(' ') else {
            continue;
        };
        let Some(conversation) = name
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|rest| rest.split('/').next())
        else {
            continue;
        };
        let entry = newest.entry(conversation).or_default();
        entry.0 = entry.0.max(seconds.parse().unwrap_or_default());
        entry.1.push(name.to_owned());
    }
    let cutoff = SystemTime::now()
        .checked_sub(age)
        .and_then(|cutoff| cutoff.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |cutoff| cutoff.as_secs());
    let mut pruned = 0;
    for (conversation, (seconds, references)) in newest {
        if seconds < cutoff {
            repo.delete(&references)?;
            let _ = std::fs::remove_dir_all(states(workspace, conversation));
            pruned += 1;
        }
    }
    Ok(pruned)
}

fn safe(workspace: &Path, path: &str) -> std::io::Result<PathBuf> {
    let relative = Path::new(path);
    if !relative
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Err(std::io::Error::other("not a plain relative path"));
    }
    let mut ancestor = workspace.to_path_buf();
    for part in relative.parent().into_iter().flat_map(Path::components) {
        ancestor.push(part);
        if ancestor
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(std::io::Error::other(
                "a parent directory is a symbolic link",
            ));
        }
    }
    Ok(workspace.join(relative))
}

fn remove(workspace: &Path, path: &str) -> std::io::Result<()> {
    let full = safe(workspace, path)?;
    match std::fs::remove_file(&full) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut parent = full.parent();
    while let Some(directory) = parent.filter(|directory| *directory != workspace) {
        if std::fs::remove_dir(directory).is_err() {
            break;
        }
        parent = directory.parent();
    }
    Ok(())
}

fn write(repo: &Repo, workspace: &Path, path: &str, entry: &Entry) -> std::io::Result<()> {
    let full = safe(workspace, path)?;
    let bytes = git(&repo.top, &["cat-file", "blob", &entry.oid])
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = full.symlink_metadata() {
        if meta.is_dir() {
            std::fs::remove_dir(&full)?;
        } else if meta.file_type().is_symlink() || entry.mode == "120000" {
            std::fs::remove_file(&full)?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if entry.mode == "120000" {
            use std::os::unix::ffi::OsStrExt as _;
            return std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&bytes), &full);
        }
        std::fs::write(&full, &bytes)?;
        let mut permissions = std::fs::metadata(&full)?.permissions();
        let mode = permissions.mode();
        permissions.set_mode(if entry.mode == "100755" {
            mode | ((mode & 0o444) >> 2)
        } else {
            mode & !0o111
        });
        std::fs::set_permissions(&full, permissions)
    }
    #[cfg(not(unix))]
    std::fs::write(&full, &bytes)
}

fn git(directory: &Path, arguments: &[&str]) -> Result<Vec<u8>, CheckpointError> {
    run(directory, arguments, &[], None)
}

fn run(
    directory: &Path,
    arguments: &[&str],
    env: &[(&str, &std::ffi::OsStr)],
    input: Option<&[u8]>,
) -> Result<Vec<u8>, CheckpointError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .args(["-c", "commit.gpgsign=false"])
        .args(arguments)
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let failed =
        |error: std::io::Error| CheckpointError::Failed(format!("cannot run git: {error}"));
    let mut child = command.spawn().map_err(failed)?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input).map_err(failed)?;
    }
    let output = child.wait_with_output().map_err(failed)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(CheckpointError::Failed(format!(
            "git {} failed: {}",
            arguments.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(directory: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {arguments:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A repository with `kept.txt` and `gone.txt` committed, `.gitignore`
    /// ignoring `target/`, and `loose.txt` untracked.
    fn repository(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dino-checkpoint-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        sh(&root, &["init", "-q", "-b", "main"]);
        put(&root, ".gitignore", "target/\n");
        put(&root, "kept.txt", "kept\n");
        put(&root, "gone.txt", "gone\n");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-q", "-m", "First"]);
        put(&root, "loose.txt", "loose\n");
        root
    }

    fn put(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(root: &Path, path: &str) -> Option<String> {
        std::fs::read_to_string(root.join(path)).ok()
    }

    /// One submission's worth of work between checkpoints.
    fn submission(root: &Path, prompt: &str, work: impl FnOnce()) -> u32 {
        let number = begin(root, "c1", prompt).unwrap();
        work();
        end(root, "c1", number).unwrap();
        number
    }

    #[test]
    fn a_checkpoint_moves_neither_head_nor_the_index_nor_the_stash() {
        let root = repository("still");
        put(&root, "kept.txt", "staged\n");
        sh(&root, &["add", "kept.txt"]);
        put(&root, "kept.txt", "staged then changed\n");
        let head = sh(&root, &["rev-parse", "HEAD"]);
        let index = std::fs::read(root.join(".git/index")).unwrap();
        let status = sh(&root, &["status", "--porcelain"]);

        assert_eq!(begin(&root, "c1", "first").unwrap(), 1);
        end(&root, "c1", 1).unwrap();
        assert_eq!(begin(&root, "c1", "second").unwrap(), 2);

        assert_eq!(sh(&root, &["rev-parse", "HEAD"]), head);
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(sh(&root, &["status", "--porcelain"]), status);
        assert_eq!(sh(&root, &["stash", "list"]), "");
        assert_eq!(sh(&root, &["branch", "--list"]).lines().count(), 1);
        let leftovers: Vec<_> = std::fs::read_dir(root.join(".git"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("dino-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        let files = Repo::open(&root)
            .unwrap()
            .files("refs/dino/checkpoints/c1/1")
            .unwrap();
        let mut paths: Vec<_> = files.keys().map(String::as_str).collect();
        paths.sort_unstable();
        assert_eq!(paths, [".gitignore", "gone.txt", "kept.txt", "loose.txt"]);
        let listed = list(&root, "c1").unwrap();
        assert_eq!(
            listed.iter().map(|c| c.prompt.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(listed[0].unix_seconds > 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rewind_restores_edits_deletions_and_new_files_from_the_conversation() {
        let root = repository("restore");
        submission(&root, "warm up", || put(&root, "kept.txt", "kept, once\n"));
        let second = submission(&root, "break things", || {
            put(&root, "kept.txt", "rewritten\n");
            std::fs::remove_file(root.join("gone.txt")).unwrap();
            put(&root, "src/new/deep.rs", "fn new() {}\n");
            put(&root, "loose.txt", "loose, edited\n");
            put(&root, "target/debug/out", "built\n");
        });
        submission(&root, "more", || {
            put(&root, "kept.txt", "rewritten twice\n");
        });

        let preview = plan(&root, "c1", second).unwrap();
        assert_eq!(preview.restore, ["gone.txt", "kept.txt", "loose.txt"]);
        assert_eq!(preview.remove, ["src/new/deep.rs"]);
        assert_eq!(preview.left, []);

        let rewound = rewind_files(&root, "c1", second).unwrap();
        assert!(rewound.failed.is_empty(), "{:?}", rewound.failed);
        assert_eq!(read(&root, "kept.txt").as_deref(), Some("kept, once\n"));
        assert_eq!(read(&root, "gone.txt").as_deref(), Some("gone\n"));
        assert_eq!(read(&root, "loose.txt").as_deref(), Some("loose\n"));
        assert!(!root.join("src").exists(), "emptied directories go too");
        assert_eq!(
            read(&root, "target/debug/out").as_deref(),
            Some("built\n"),
            "ignored files were never captured and are not touched"
        );
        assert_eq!(
            sh(&root, &["rev-parse", "HEAD"]),
            sh(&root, &["rev-parse", "main"])
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn what_someone_else_changed_is_named_and_left_alone() {
        let root = repository("others");
        let first = submission(&root, "edit", || {
            put(&root, "kept.txt", "by dino\n");
            put(&root, "gone.txt", "by dino\n");
        });
        // Between submissions, by the person.
        put(&root, "gone.txt", "by the person, after dino\n");
        put(&root, "notes.txt", "the person's own\n");
        submission(&root, "nothing", || {});
        put(&root, "loose.txt", "the person again\n");

        let rewound = rewind_files(&root, "c1", first).unwrap();
        assert_eq!(rewound.plan.restore, ["kept.txt"]);
        assert_eq!(
            rewound.plan.left,
            [
                Left {
                    path: "gone.txt".into(),
                    why: Why::ChangedSince
                },
                Left {
                    path: "loose.txt".into(),
                    why: Why::NotTouched
                },
                Left {
                    path: "notes.txt".into(),
                    why: Why::NotTouched
                },
            ]
        );
        assert_eq!(read(&root, "kept.txt").as_deref(), Some("kept\n"));
        assert_eq!(
            read(&root, "gone.txt").as_deref(),
            Some("by the person, after dino\n")
        );
        assert_eq!(
            read(&root, "notes.txt").as_deref(),
            Some("the person's own\n")
        );
        assert_eq!(
            read(&root, "loose.txt").as_deref(),
            Some("the person again\n")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_submission_cut_off_before_its_after_is_not_taken_for_the_conversations() {
        let root = repository("cut");
        let first = begin(&root, "c1", "crashed").unwrap();
        put(&root, "kept.txt", "half done\n");

        let preview = plan(&root, "c1", first).unwrap();
        assert!(preview.is_empty());
        assert_eq!(
            preview.left,
            [Left {
                path: "kept.txt".into(),
                why: Why::NotTouched
            }]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_recorded_rewind_can_itself_be_rewound_and_counts_as_the_conversations() {
        let root = repository("undo");
        let first = submission(&root, "one", || put(&root, "kept.txt", "one\n"));
        submission(&root, "two", || put(&root, "kept.txt", "two\n"));
        let third = submission(&root, "three", || put(&root, "kept.txt", "three\n"));

        let rewound = rewind_files(&root, "c1", third).unwrap();
        assert_eq!(read(&root, "kept.txt").as_deref(), Some("two\n"));
        let undo = next(&root, "c1").unwrap();
        record_rewind(&root, "c1", undo, "(rewound to 3)", &rewound).unwrap();

        // Further back, through the rewind.
        let further = rewind_files(&root, "c1", first).unwrap();
        assert_eq!(read(&root, "kept.txt").as_deref(), Some("kept\n"));
        let again = next(&root, "c1").unwrap();
        record_rewind(&root, "c1", again, "(rewound to 1)", &further).unwrap();

        // And the rewind undone.
        rewind_files(&root, "c1", undo).unwrap();
        assert_eq!(read(&root, "kept.txt").as_deref(), Some("three\n"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn modes_and_symbolic_links_come_back_as_they_were() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = repository("modes");
        put(&root, "run.sh", "#!/bin/sh\n");
        let mut permissions = std::fs::metadata(root.join("run.sh"))
            .unwrap()
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(root.join("run.sh"), permissions).unwrap();
        std::os::unix::fs::symlink("kept.txt", root.join("link")).unwrap();
        let first = submission(&root, "mess", || {
            std::fs::remove_file(root.join("run.sh")).unwrap();
            put(&root, "run.sh", "plain now\n");
            std::fs::remove_file(root.join("link")).unwrap();
            put(&root, "link", "a file now\n");
        });

        rewind_files(&root, "c1", first).unwrap();
        let mode = std::fs::metadata(root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "{mode:o}");
        assert_eq!(read(&root, "run.sh").as_deref(), Some("#!/bin/sh\n"));
        assert_eq!(
            std::fs::read_link(root.join("link")).unwrap(),
            Path::new("kept.txt")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_workspace_below_the_top_of_its_repository_is_checkpointed_on_its_own() {
        let root = repository("below");
        put(&root, "crate/lib.rs", "lib\n");
        put(&root, "elsewhere.txt", "elsewhere\n");
        let workspace = root.join("crate");
        let first = begin(&workspace, "c1", "edit").unwrap();
        put(&root, "crate/lib.rs", "changed\n");
        put(&root, "crate/.dino/sessions/c1.state.json", "{}");
        put(&root, "elsewhere.txt", "outside the workspace\n");
        end(&workspace, "c1", first).unwrap();

        let rewound = rewind_files(&workspace, "c1", first).unwrap();
        assert_eq!(rewound.plan.restore, ["lib.rs"]);
        assert_eq!(rewound.plan.left, []);
        assert_eq!(read(&root, "crate/lib.rs").as_deref(), Some("lib\n"));
        assert_eq!(
            read(&root, "elsewhere.txt").as_deref(),
            Some("outside the workspace\n")
        );
        assert!(root.join("crate/.dino/sessions/c1.state.json").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checkpoints_are_truncated_copied_and_pruned_with_their_conversations() {
        let root = repository("keep");
        for prompt in ["one", "two", "three"] {
            let number = submission(&root, prompt, || {});
            put(
                &root,
                &format!(".dino/checkpoints/c1/{number}.state.json"),
                "{}",
            );
        }

        assert_eq!(copy(&root, "c1", "c2").unwrap(), 3);
        assert!(state_path(&root, "c2", 3).is_file());
        truncate(&root, "c1", 2).unwrap();
        assert_eq!(next(&root, "c1").unwrap(), 2);
        assert!(!state_path(&root, "c1", 2).exists());
        assert!(state_path(&root, "c1", 1).is_file());
        assert_eq!(list(&root, "c2").unwrap().len(), 3, "a copy is its own");

        assert_eq!(prune(&root, KEPT_FOR).unwrap(), 0);
        let old = "1000000000 +0000";
        let tree = Repo::open(&root).unwrap().snapshot().unwrap();
        let commit = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["commit-tree", &tree, "-m", "old"])
            .env("GIT_COMMITTER_DATE", old)
            .env("GIT_AUTHOR_DATE", old)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        let commit = String::from_utf8_lossy(&commit.stdout).trim().to_owned();
        sh(
            &root,
            &["update-ref", "refs/dino/checkpoints/old/1", &commit],
        );
        put(&root, ".dino/checkpoints/old/1.state.json", "{}");

        assert_eq!(prune(&root, KEPT_FOR).unwrap(), 1);
        assert!(list(&root, "old").unwrap().is_empty());
        assert!(!root.join(".dino/checkpoints/old").exists());
        assert_eq!(list(&root, "c2").unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn outside_a_repository_there_are_no_checkpoints_and_odd_names_are_refused() {
        let root =
            std::env::temp_dir().join(format!("dino-checkpoint-none-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        if git(&root, &["rev-parse", "--is-inside-work-tree"]).is_err() {
            assert!(begin(&root, "c1", "x").is_err());
        }
        assert!(begin(&root, "../c1", "x").is_err());
        assert!(begin(&root, "c1/2", "x").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
