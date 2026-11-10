//! The git directories a linked worktree writes outside itself.
//!
//! A linked worktree's `.git` is a file naming `<repository>/.git/worktrees/<name>`,
//! and its commits land in the repository's shared objects and refs. Confined
//! to the checkout alone, a process there cannot `git add` or `git commit`.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// A checkout made by `git worktree add`, and where its git state lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedWorktree {
    /// The checkout: the directory holding the `.git` file.
    pub root: PathBuf,
    /// Its own HEAD, index and reflog: `<common>/worktrees/<name>`.
    pub git_dir: PathBuf,
    /// The repository's `.git`, shared with every other checkout of it.
    pub common_dir: PathBuf,
}

impl LinkedWorktree {
    /// The linked worktree `path` is in, if it is in one.
    ///
    /// A `.git` file can name any directory, so this is believed only when
    /// git's own records agree: the directory it names sits in a `worktrees`
    /// directory of the repository and names this checkout back.
    #[must_use]
    pub fn find(path: &Path) -> Option<Self> {
        let path = fs::canonicalize(path).ok()?;
        let root = path
            .ancestors()
            .find(|directory| fs::symlink_metadata(directory.join(".git")).is_ok())?;
        let pointer = fs::read_to_string(root.join(".git")).ok()?;
        let git_dir = fs::canonicalize(root.join(pointer.strip_prefix("gitdir:")?.trim())).ok()?;
        let back = fs::read_to_string(git_dir.join("gitdir")).ok()?;
        if fs::canonicalize(git_dir.join(back.trim())).ok()? != root.join(".git") {
            return None;
        }
        let common = fs::read_to_string(git_dir.join("commondir")).ok()?;
        let common_dir = fs::canonicalize(git_dir.join(common.trim())).ok()?;
        if git_dir.parent()? != common_dir.join("worktrees") {
            return None;
        }
        Some(Self {
            root: root.to_path_buf(),
            git_dir,
            common_dir,
        })
    }

    /// What `git add`, `git commit` and a branch switch write. Not the
    /// repository's `config` or `hooks`, which run later outside any sandbox.
    #[must_use]
    pub fn writable(&self) -> Vec<PathBuf> {
        let mut writable = vec![self.git_dir.clone()];
        writable.extend(
            ["objects", "refs", "logs"]
                .into_iter()
                .map(|name| self.common_dir.join(name))
                .filter(|path| path.is_dir()),
        );
        writable
    }

    /// Read by git as configuration when the repository enables
    /// `extensions.worktreeConfig`, so as live as the repository's `config`.
    #[must_use]
    pub fn config(&self) -> PathBuf {
        self.git_dir.join("config.worktree")
    }

    /// The repository's main checkout, unless the repository is bare.
    #[must_use]
    pub fn main_checkout(&self) -> Option<PathBuf> {
        (self.common_dir.file_name()? == ".git")
            .then(|| self.common_dir.parent().map(Path::to_path_buf))?
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(directory: &Path, arguments: &[&str]) {
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
    }

    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("dino-linked-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::canonicalize(base).unwrap()
    }

    #[test]
    fn a_linked_worktree_is_found_from_inside_it_and_names_the_main_checkout() {
        let base = scratch("found");
        let main = base.join("main");
        fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(main.join("src")).unwrap();
        fs::write(main.join("src/lib.rs"), "").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "one"]);
        let linked = base.join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        );

        let found = LinkedWorktree::find(&linked.join("src")).unwrap();

        assert_eq!(found.root, linked);
        assert_eq!(found.git_dir, main.join(".git/worktrees/linked"));
        assert_eq!(found.common_dir, main.join(".git"));
        assert_eq!(found.main_checkout(), Some(main.clone()));
        assert!(found.writable().contains(&main.join(".git/objects")));
        assert!(!found.writable().iter().any(|path| path.ends_with("hooks")));
        assert_eq!(
            LinkedWorktree::find(&main),
            None,
            "the main checkout is not linked"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_git_file_that_names_somewhere_else_is_not_believed() {
        let base = scratch("forged");
        let elsewhere = base.join("elsewhere");
        let checkout = base.join("checkout");
        fs::create_dir_all(elsewhere.join("worktrees")).unwrap();
        fs::create_dir_all(&checkout).unwrap();
        fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", elsewhere.display()),
        )
        .unwrap();

        assert_eq!(LinkedWorktree::find(&checkout), None);
        let _ = fs::remove_dir_all(&base);
    }
}
