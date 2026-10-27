//! Putting protected paths back after a process has run.
//!
//! Landlock only grants, so on Linux a process confined to the workspace can
//! still write beneath a protected path. Rather than stop the write, the
//! protected paths are copied before the process starts and put back once it
//! has finished. Issue #22 has the alternatives and why they lost.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Write as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use crate::SandboxError;

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Dir(fs::Permissions),
    File(Vec<u8>, fs::Permissions),
    Symlink(PathBuf),
}

/// Protected paths as they were before a process ran.
///
/// Restored on drop as well, so a process abandoned by cancellation still has
/// its changes undone.
#[derive(Debug)]
pub struct Snapshot {
    root: PathBuf,
    protected: Vec<PathBuf>,
    entries: BTreeMap<PathBuf, Entry>,
    restored: bool,
}

/// Refuses a protected path that could reach outside the workspace, since
/// restoring deletes whatever it finds there that was not there before.
pub(crate) fn checked(protected: &[PathBuf]) -> Result<Vec<PathBuf>, SandboxError> {
    protected
        .iter()
        .map(|path| {
            let normal = path
                .components()
                .all(|part| matches!(part, Component::Normal(_)));
            if normal && !path.as_os_str().is_empty() {
                Ok(path.clone())
            } else {
                Err(SandboxError::Profile(format!(
                    "protected path {} must be relative to the workspace, without `..`",
                    path.display()
                )))
            }
        })
        .collect()
}

impl Snapshot {
    /// # Errors
    ///
    /// Returns the I/O error when a protected path cannot be read.
    pub fn take(root: &Path, protected: &[PathBuf]) -> io::Result<Self> {
        let mut entries = BTreeMap::new();
        for path in protected {
            for (relative, metadata) in walk(root, path)? {
                let full = root.join(&relative);
                let entry = if metadata.is_symlink() {
                    Entry::Symlink(fs::read_link(&full)?)
                } else if metadata.is_dir() {
                    Entry::Dir(metadata.permissions())
                } else {
                    Entry::File(fs::read(&full)?, metadata.permissions())
                };
                entries.insert(relative, entry);
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            protected: protected.to_vec(),
            entries,
            restored: false,
        })
    }

    /// Puts every protected path back as it was, and says which ones changed.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when a path cannot be put back.
    pub fn restore(&mut self) -> io::Result<Vec<String>> {
        self.restored = true;
        let mut changed = Vec::new();

        // Shallowest first, so removing a directory also covers what is in it.
        let mut removed: Vec<PathBuf> = Vec::new();
        for path in &self.protected {
            for (relative, metadata) in walk(&self.root, path)? {
                if removed.iter().any(|gone| relative.starts_with(gone)) {
                    continue;
                }
                let expected = self.entries.get(&relative);
                let kept = match expected {
                    Some(Entry::Dir(_)) => metadata.is_dir() && !metadata.is_symlink(),
                    Some(Entry::Symlink(_)) => metadata.is_symlink(),
                    Some(Entry::File(..)) => metadata.is_file(),
                    None => false,
                };
                if !kept {
                    remove(&self.root.join(&relative), &metadata)?;
                    changed.push(relative.clone());
                    removed.push(relative);
                }
            }
        }

        // Parents sort before their children, so a directory exists before
        // anything is written into it.
        for (relative, entry) in &self.entries {
            let full = self.root.join(relative);
            if put_back(&full, entry)? && !changed.contains(relative) {
                changed.push(relative.clone());
            }
        }

        changed.sort();
        Ok(changed
            .iter()
            .map(|path| path.display().to_string())
            .collect())
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.restore();
        }
    }
}

/// Every path at and beneath `path`, without following symlinks. Nothing
/// when it does not exist.
fn walk(root: &Path, path: &Path) -> io::Result<Vec<(PathBuf, fs::Metadata)>> {
    let mut found = Vec::new();
    let mut pending = vec![path.to_path_buf()];
    while let Some(relative) = pending.pop() {
        let metadata = match fs::symlink_metadata(root.join(&relative)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_dir() {
            for child in fs::read_dir(root.join(&relative))? {
                pending.push(relative.join(child?.file_name()));
            }
        }
        found.push((relative, metadata));
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(found)
}

fn remove(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.is_dir() && !metadata.is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Makes one path match its entry, and says whether it had to.
fn put_back(path: &Path, entry: &Entry) -> io::Result<bool> {
    let current = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    match entry {
        Entry::Dir(permissions) => {
            if current.is_none() {
                fs::create_dir(path)?;
            } else if current
                .as_ref()
                .is_some_and(|metadata| metadata.permissions() == *permissions)
            {
                return Ok(false);
            }
            fs::set_permissions(path, permissions.clone())?;
        }
        Entry::File(contents, permissions) => {
            if let Some(metadata) = &current {
                if metadata.permissions() == *permissions && fs::read(path)? == *contents {
                    return Ok(false);
                }
                fs::remove_file(path)?;
            }
            // `create_new` refuses a symlink left in the way, so this cannot be
            // steered into writing somewhere else.
            fs::File::options()
                .write(true)
                .create_new(true)
                .open(path)?
                .write_all(contents)?;
            fs::set_permissions(path, permissions.clone())?;
        }
        Entry::Symlink(target) => {
            if current.is_some() {
                if fs::read_link(path)? == *target {
                    return Ok(false);
                }
                fs::remove_file(path)?;
            }
            symlink(target, path)?;
        }
    }
    Ok(true)
}

#[cfg(unix)]
fn symlink(target: &Path, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, path: &Path) -> io::Result<()> {
    Err(io::Error::other(format!(
        "cannot restore the symlink {} on this platform",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Workspace(PathBuf);

    impl Workspace {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("dancho-restore-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join("tests/fixtures")).unwrap();
            fs::write(path.join("tests/spec.rs"), "kept\n").unwrap();
            fs::write(path.join("tests/fixtures/input.txt"), "data\n").unwrap();
            fs::write(path.join("lib.rs"), "mine\n").unwrap();
            Self(path)
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn protected() -> Vec<PathBuf> {
        vec![PathBuf::from("tests"), PathBuf::from("missing")]
    }

    #[test]
    fn changes_beneath_a_protected_path_are_undone_and_named() {
        let workspace = Workspace::new("undone");
        let root = &workspace.0;
        let mut snapshot = Snapshot::take(root, &protected()).unwrap();

        fs::write(root.join("tests/spec.rs"), "changed\n").unwrap();
        fs::write(root.join("tests/added.rs"), "new\n").unwrap();
        fs::remove_dir_all(root.join("tests/fixtures")).unwrap();
        fs::create_dir(root.join("missing")).unwrap();
        fs::write(root.join("lib.rs"), "changed too\n").unwrap();

        let changed = snapshot.restore().unwrap();

        assert_eq!(
            fs::read_to_string(root.join("tests/spec.rs")).unwrap(),
            "kept\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("tests/fixtures/input.txt")).unwrap(),
            "data\n"
        );
        assert!(!root.join("tests/added.rs").exists());
        assert!(!root.join("missing").exists());
        assert_eq!(
            fs::read_to_string(root.join("lib.rs")).unwrap(),
            "changed too\n"
        );
        assert_eq!(
            changed,
            [
                "missing",
                "tests/added.rs",
                "tests/fixtures",
                "tests/fixtures/input.txt",
                "tests/spec.rs"
            ]
        );
    }

    #[test]
    fn nothing_changed_reports_nothing() {
        let workspace = Workspace::new("unchanged");
        let mut snapshot = Snapshot::take(&workspace.0, &protected()).unwrap();

        assert!(snapshot.restore().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_protected_directory_swapped_for_a_symlink_is_not_written_through() {
        let workspace = Workspace::new("swapped");
        let root = &workspace.0;
        let outside = Workspace::new("outside");
        let mut snapshot = Snapshot::take(root, &protected()).unwrap();

        fs::remove_dir_all(root.join("tests")).unwrap();
        std::os::unix::fs::symlink(&outside.0, root.join("tests")).unwrap();
        snapshot.restore().unwrap();

        assert!(
            !fs::symlink_metadata(root.join("tests"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(root.join("tests/spec.rs")).unwrap(),
            "kept\n"
        );
        assert_eq!(
            fs::read_to_string(outside.0.join("lib.rs")).unwrap(),
            "mine\n"
        );
        assert!(outside.0.join("tests/spec.rs").exists());
    }

    #[test]
    fn a_dropped_snapshot_still_restores() {
        let workspace = Workspace::new("dropped");
        let root = &workspace.0;
        let snapshot = Snapshot::take(root, &protected()).unwrap();

        fs::write(root.join("tests/spec.rs"), "changed\n").unwrap();
        drop(snapshot);

        assert_eq!(
            fs::read_to_string(root.join("tests/spec.rs")).unwrap(),
            "kept\n"
        );
    }

    #[test]
    fn a_path_that_could_leave_the_workspace_is_refused() {
        for path in ["../outside", "/etc", "", "tests/../.."] {
            assert!(checked(&[PathBuf::from(path)]).is_err(), "{path}");
        }
        assert!(checked(&[PathBuf::from("tests/golden")]).is_ok());
    }
}
