//! A session in a git worktree of its own, beneath `.dino/worktrees`, so two
//! sessions in one repository share neither an index nor half-finished edits.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use dino_sandbox::LinkedWorktree;

const DIR: [&str; 2] = [".dino", "worktrees"];

const DEFAULT_PREFIX: &str = "dino/";

/// A worktree made for a session, and what it was made from.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// The new checkout.
    pub root: PathBuf,
    /// Where the session runs: the checkout, or the subdirectory of it that
    /// matches where the session was started.
    pub workspace: PathBuf,
    pub branch: String,
    /// The commit it was made from, to tell whether anything was committed.
    base: String,
    /// The main checkout, which owns the branch.
    main: PathBuf,
}

/// What became of a worktree when its session ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    Removed,
    Kept,
}

impl Worktree {
    /// Makes `<main checkout>/.dino/worktrees/<name>` on a new branch from the
    /// commit `workspace` is at, prefixed with the user file's `branch_prefix`.
    /// An empty name is generated.
    ///
    /// # Errors
    ///
    /// Fails outside a git repository, before its first commit, for a name
    /// that is not a plain word, and when git refuses.
    pub fn create(workspace: &Path, name: &str) -> Result<Self> {
        let prefix = crate::settings::user_file()?
            .branch_prefix
            .unwrap_or_else(|| DEFAULT_PREFIX.to_owned());
        Self::create_with(workspace, name, &prefix)
    }

    fn create_with(workspace: &Path, name: &str, prefix: &str) -> Result<Self> {
        let name = if name.is_empty() {
            dino_core::session::SessionId::generate().to_string()
        } else {
            checked_name(name)?.to_owned()
        };
        let toplevel = git(workspace, &["rev-parse", "--show-toplevel"])
            .context("--worktree needs a git repository")?;
        let toplevel = std::fs::canonicalize(toplevel.trim())?;
        let main = LinkedWorktree::find(&toplevel)
            .and_then(|linked| linked.main_checkout())
            .unwrap_or_else(|| toplevel.clone());
        let base = git(workspace, &["rev-parse", "--verify", "HEAD"])
            .context("the repository has no commit to branch from")?
            .trim()
            .to_owned();
        let parent = DIR.iter().fold(main.clone(), |path, part| path.join(part));
        let root = parent.join(&name);
        if root.exists() {
            bail!("{} already exists", root.display());
        }
        std::fs::create_dir_all(&parent)?;
        ignore_state(&main)?;
        let branch = format!("{prefix}{name}");
        let path = root
            .to_str()
            .context("the worktree path is not valid UTF-8")?;
        git(
            &main,
            &["worktree", "add", "--quiet", "-b", &branch, path, &base],
        )?;
        ignore_state(&root)?;
        let inside = workspace.strip_prefix(&toplevel).unwrap_or(Path::new(""));
        let workspace = std::fs::canonicalize(root.join(inside)).unwrap_or_else(|_| root.clone());
        Ok(Self {
            root,
            workspace,
            branch,
            base,
            main,
        })
    }

    /// Removes the worktree and its branch when nothing in it changed and
    /// nothing was committed; otherwise leaves both for the person.
    ///
    /// # Errors
    ///
    /// Fails when git cannot say what changed or will not remove it.
    pub fn finish(&self) -> Result<Ended> {
        if !self.root.exists() {
            git(&self.main, &["worktree", "prune"])?;
            return Ok(Ended::Removed);
        }
        let status = git(&self.root, &["status", "--porcelain"])?;
        let head = git(&self.root, &["rev-parse", "HEAD"])?;
        if !status.trim().is_empty() || head.trim() != self.base {
            return Ok(Ended::Kept);
        }
        let path = self
            .root
            .to_str()
            .context("the worktree path is not valid UTF-8")?;
        git(&self.main, &["worktree", "remove", path])?;
        let tip = git(
            &self.main,
            &["rev-parse", "--verify", "--quiet", &self.branch],
        );
        if tip.is_ok_and(|tip| tip.trim() == self.base) {
            git(&self.main, &["branch", "--quiet", "-D", &self.branch])?;
        }
        Ok(Ended::Removed)
    }
}

/// A branch name and a directory name at once, so one word git and every
/// filesystem accept.
fn checked_name(name: &str) -> Result<&str> {
    let plain = name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character));
    if !plain
        || name.starts_with(['.', '-'])
        || name.strip_suffix(".lock").is_some()
        || name.contains("..")
    {
        bail!("a worktree name is letters, digits, '-', '_' and '.', such as fix-parser: {name:?}");
    }
    Ok(name)
}

/// Session state and other worktrees are not the project's.
fn ignore_state(checkout: &Path) -> Result<()> {
    let state = checkout.join(DIR[0]);
    std::fs::create_dir_all(&state)?;
    let ignore = state.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(ignore, "*\n")?;
    }
    Ok(())
}

/// Where approval rules and memory are kept for `workspace`: in a linked
/// worktree, the same place in its main checkout, since both are one
/// repository.
pub fn home(workspace: &Path) -> PathBuf {
    LinkedWorktree::find(workspace)
        .and_then(|linked| {
            let main = linked.main_checkout()?;
            let inside = std::fs::canonicalize(workspace).ok()?;
            Some(main.join(inside.strip_prefix(&linked.root).ok()?))
        })
        .unwrap_or_else(|| workspace.to_path_buf())
}

/// Where Cargo builds in one of Dino's worktrees: the main checkout's own
/// target, so a new worktree does not rebuild everything, unless
/// `CARGO_TARGET_DIR` names another.
pub fn shared_target(workspace: &Path) -> Option<PathBuf> {
    let main = dinos(workspace)?.main_checkout()?;
    Some(match std::env::var_os("CARGO_TARGET_DIR") {
        Some(set) if !set.is_empty() => main.join(set),
        _ => main.join("target"),
    })
}

/// What the banner says about a session in a linked worktree.
pub fn describe(workspace: &Path) -> Option<String> {
    let linked = LinkedWorktree::find(workspace)?;
    let head = std::fs::read_to_string(linked.git_dir.join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/").map_or_else(
        || "a detached HEAD".to_owned(),
        |branch| format!("branch {branch}"),
    );
    let main = linked.main_checkout().map_or_else(
        || linked.common_dir.display().to_string(),
        |main| crate::render::short_path(&main, Path::new("")),
    );
    Some(format!("worktree of {main} on {branch}"))
}

/// The linked worktree `workspace` is in, when it is one of Dino's.
fn dinos(workspace: &Path) -> Option<LinkedWorktree> {
    let linked = LinkedWorktree::find(workspace)?;
    let main = linked.main_checkout()?;
    let parent = DIR.iter().fold(main, |path, part| path.join(part));
    (linked.root.parent()? == parent).then_some(linked)
}

/// Runs git for Dino itself, not for the model, and returns what it printed.
fn git(directory: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .context("cannot run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(directory: &Path, arguments: &[&str]) -> String {
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
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn repository(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dino-worktree-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("crates/one")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        run(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("crates/one/lib.rs"), "pub fn one() {}\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "one"]);
        root
    }

    #[test]
    fn an_unchanged_worktree_and_its_branch_are_removed() {
        let main = repository("unchanged");

        let worktree = Worktree::create_with(&main, "look", "dino/").unwrap();

        assert_eq!(worktree.root, main.join(".dino/worktrees/look"));
        assert_eq!(worktree.branch, "dino/look");
        assert!(worktree.root.join("crates/one/lib.rs").is_file());
        assert_eq!(
            run(&main, &["status", "--porcelain"]),
            "",
            "the main checkout stays clean"
        );
        assert_eq!(
            describe(&worktree.workspace).unwrap(),
            format!("worktree of {} on branch dino/look", main.display())
        );
        assert_eq!(worktree.finish().unwrap(), Ended::Removed);
        assert!(!worktree.root.exists());
        assert_eq!(run(&main, &["branch", "--list", "dino/look"]), "");
        let _ = std::fs::remove_dir_all(&main);
    }

    #[test]
    fn a_worktree_with_an_edit_or_a_commit_is_kept() {
        let main = repository("changed");
        let edited = Worktree::create_with(&main, "edited", "pete/").unwrap();
        let committed = Worktree::create_with(&main, "committed", "pete/").unwrap();
        std::fs::write(edited.root.join("crates/one/lib.rs"), "pub fn two() {}\n").unwrap();
        std::fs::write(committed.root.join("new.rs"), "\n").unwrap();
        run(&committed.root, &["add", "new.rs"]);
        run(&committed.root, &["commit", "-q", "-m", "two"]);

        assert_eq!(edited.finish().unwrap(), Ended::Kept);
        assert_eq!(committed.finish().unwrap(), Ended::Kept);
        assert!(edited.root.is_dir());
        assert!(run(&main, &["branch", "--list", "pete/committed"]).contains("pete/committed"));
        let _ = std::fs::remove_dir_all(&main);
    }

    #[test]
    fn a_session_started_in_a_subdirectory_continues_in_the_same_one() {
        let main = repository("inside");

        let worktree = Worktree::create_with(&main.join("crates/one"), "", "dino/").unwrap();

        assert_eq!(worktree.workspace, worktree.root.join("crates/one"));
        assert!(worktree.branch.starts_with("dino/"));
        assert_eq!(home(&worktree.workspace), main.join("crates/one"));
        assert_eq!(
            shared_target(&worktree.workspace),
            Some(main.join("target"))
        );
        assert_eq!(home(&main), main, "a main checkout is its own home");
        assert_eq!(shared_target(&main), None);
        worktree.finish().unwrap();
        let _ = std::fs::remove_dir_all(&main);
    }

    #[test]
    fn a_worktree_made_from_a_worktree_sits_beside_it_in_the_main_checkout() {
        let main = repository("nested");
        let first = Worktree::create_with(&main, "first", "dino/").unwrap();

        let second = Worktree::create_with(&first.workspace, "second", "dino/").unwrap();

        assert_eq!(second.root, main.join(".dino/worktrees/second"));
        second.finish().unwrap();
        first.finish().unwrap();
        let _ = std::fs::remove_dir_all(&main);
    }

    #[test]
    fn names_that_would_climb_out_or_confuse_git_are_refused() {
        for name in [
            "../up", "a/b", ".hidden", "-flag", "x.lock", "a..b", "sp ace",
        ] {
            assert!(checked_name(name).is_err(), "{name}");
        }
        assert_eq!(checked_name("fix-parser_2.0").unwrap(), "fix-parser_2.0");
    }
}
