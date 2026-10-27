//! The workspace's git state, read once when a session starts.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

/// Status lines shown before the rest are counted rather than listed.
pub const MAX_STATUS_LINES: usize = 20;
const RECENT_COMMITS: &str = "5";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSnapshot {
    pub branch: String,
    pub main_branch: Option<String>,
    /// `git status --short`, at most [`MAX_STATUS_LINES`] of them.
    pub status: Vec<String>,
    pub status_total: usize,
    /// Subjects of the latest commits, newest first.
    pub recent: Vec<String>,
}

impl GitSnapshot {
    /// Reads the snapshot, or `None` outside a git work tree or without `git`.
    #[must_use]
    pub fn take(workspace: &Path) -> Option<Self> {
        if git(workspace, &["rev-parse", "--is-inside-work-tree"])?.trim() != "true" {
            return None;
        }
        let branch = git(workspace, &["symbolic-ref", "--short", "HEAD"]).map_or_else(
            || "(detached HEAD)".to_owned(),
            |name| name.trim().to_owned(),
        );
        let main_branch = git(
            workspace,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
        .map(|name| name.trim().trim_start_matches("origin/").to_owned())
        .or_else(|| {
            ["main", "master"].into_iter().find_map(|name| {
                git(workspace, &["rev-parse", "--verify", "--quiet", name]).map(|_| name.to_owned())
            })
        });
        let status = git(workspace, &["status", "--short"]).unwrap_or_default();
        let status: Vec<String> = status.lines().map(str::to_owned).collect();
        let recent = git(workspace, &["log", "-n", RECENT_COMMITS, "--format=%s"])
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        Some(Self {
            branch,
            main_branch,
            status_total: status.len(),
            status: status.into_iter().take(MAX_STATUS_LINES).collect(),
            recent,
        })
    }

    /// The snapshot as lines of the system prompt.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!(
            "- Git, as of the session start (it goes stale as you work): branch {}",
            self.branch
        );
        if let Some(main) = &self.main_branch {
            let _ = write!(&mut text, ", main branch {main}");
        }
        if self.status_total == 0 {
            text.push_str(", working tree clean.\n");
        } else {
            let _ = writeln!(&mut text, ", {} changed path(s):", self.status_total);
            for line in &self.status {
                let _ = writeln!(&mut text, "    {line}");
            }
            if self.status_total > self.status.len() {
                let _ = writeln!(
                    &mut text,
                    "    ... and {} more",
                    self.status_total - self.status.len()
                );
            }
        }
        if !self.recent.is_empty() {
            text.push_str("  Recent commits:\n");
            for subject in &self.recent {
                let _ = writeln!(&mut text, "    - {subject}");
            }
        }
        text
    }
}

fn git(workspace: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(arguments)
        // Otherwise `status` may rewrite the index, and this is meant to look only.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(workspace: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(workspace)
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
            .unwrap()
            .status;
        assert!(status.success(), "git {arguments:?}");
    }

    #[test]
    fn a_repository_reports_branch_changes_and_recent_commits() {
        let workspace = std::env::temp_dir().join(format!("dancho-git-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();
        run(&workspace, &["init", "-q", "-b", "main"]);
        std::fs::write(workspace.join("a.txt"), "a").unwrap();
        run(&workspace, &["add", "a.txt"]);
        run(&workspace, &["commit", "-q", "-m", "First commit"]);
        run(&workspace, &["switch", "-q", "-c", "pete/work"]);
        std::fs::write(workspace.join("b.txt"), "b").unwrap();

        let snapshot = GitSnapshot::take(&workspace).unwrap();

        assert_eq!(snapshot.branch, "pete/work");
        assert_eq!(snapshot.main_branch.as_deref(), Some("main"));
        assert_eq!(snapshot.status, ["?? b.txt"]);
        assert_eq!(snapshot.recent, ["First commit"]);
        let text = snapshot.render();
        assert!(text.contains("branch pete/work, main branch main, 1 changed path(s):"));
        assert!(text.contains("    - First commit"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn outside_a_repository_there_is_no_snapshot() {
        let workspace = std::env::temp_dir().join(format!("dancho-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();

        // The temp directory could itself sit inside a repository; only assert
        // when it does not.
        if git(&workspace, &["rev-parse", "--is-inside-work-tree"]).is_none() {
            assert_eq!(GitSnapshot::take(&workspace), None);
        }
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_long_status_is_counted_rather_than_listed() {
        let snapshot = GitSnapshot {
            branch: "main".into(),
            main_branch: None,
            status: vec!["M a".into(); MAX_STATUS_LINES],
            status_total: MAX_STATUS_LINES + 7,
            recent: Vec::new(),
        };

        assert!(snapshot.render().contains("... and 7 more"));
    }
}
