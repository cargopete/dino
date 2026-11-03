//! Which tracked files a command wrote, so an edit made around the file tools
//! is named to the model rather than passing unnoticed.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;

/// Tracked files that differ from the index, with when each was last written.
pub(crate) type Modified = HashMap<String, Option<SystemTime>>;

/// The modified tracked files beneath `root`, or `None` outside a repository.
pub(crate) async fn modified(root: &Path) -> Option<Modified> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("git")
            .args(["ls-files", "--modified", "-z"])
            .current_dir(root)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let listed = String::from_utf8(output.stdout).ok()?;
    Some(
        listed
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(|path| {
                let written = std::fs::metadata(root.join(path))
                    .and_then(|metadata| metadata.modified())
                    .ok();
                (path.to_owned(), written)
            })
            .collect(),
    )
}

/// How many written files the note names before counting the rest.
const NAMED: usize = 5;

/// What to tell the model when a command wrote tracked files, if it did.
pub(crate) fn note(before: &Modified, after: &Modified) -> Option<String> {
    let mut written: Vec<&str> = after
        .iter()
        .filter(|(path, when)| before.get(*path) != Some(when))
        .map(|(path, _)| path.as_str())
        .collect();
    if written.is_empty() {
        return None;
    }
    written.sort_unstable();
    let mut named = written
        .iter()
        .take(NAMED)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if written.len() > NAMED {
        let _ = write!(named, " and {} more", written.len() - NAMED);
    }
    Some(format!(
        "this line wrote tracked files ({named}). An edit made through shell skips the \
         after-edit hooks, the stale-file check and the diff in the session log: change files \
         with apply_patch or write_file, and leave shell to programs that write them, such as \
         formatters"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_newly_modified_or_written_again_is_named_and_others_are_not() {
        let then = SystemTime::UNIX_EPOCH;
        let later = then + Duration::from_secs(1);
        let before: Modified = [
            ("a.rs".to_owned(), Some(then)),
            ("b.rs".to_owned(), Some(then)),
        ]
        .into_iter()
        .collect();
        let after: Modified = [
            ("a.rs".to_owned(), Some(then)),
            ("b.rs".to_owned(), Some(later)),
            ("c.rs".to_owned(), Some(later)),
        ]
        .into_iter()
        .collect();

        let note = note(&before, &after).unwrap();
        assert!(note.contains("(b.rs, c.rs)"), "{note}");
        assert_eq!(super::note(&before, &before), None);
    }
}
