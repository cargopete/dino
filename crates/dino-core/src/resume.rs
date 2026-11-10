//! Writing a session's native state to disk, and finding it again.
//!
//! The core stores an opaque payload it never reads, per RFC-0003. What is in
//! it is the adapter's business; that it lands on disk with the right
//! permissions is this module's.

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use dino_model::SessionState;
use serde::Deserialize;
use serde::Serialize;

use crate::session::RunOutcome;
use crate::session::SinkError;

/// The state as written: the adapter's payload, and the name a person gave the
/// conversation beside it, so a file from before names existed still loads.
#[derive(Serialize)]
struct Stored<'a> {
    #[serde(flatten)]
    state: &'a SessionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

/// What a saved conversation is, without reading its history into memory.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Label {
    pub model_key: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Where a session's state lives, beside its log.
#[must_use]
pub fn state_path(workspace: &Path, session_id: &str) -> PathBuf {
    workspace
        .join(".dino")
        .join("sessions")
        .join(format!("{session_id}.state.json"))
}

/// Writes state atomically, owner-readable only.
///
/// **This file holds the conversation, and the conversation holds your source.**
/// Tool results are in it: file contents, search hits, compiler output, and
/// whatever reasoning the provider returned and the adapter retained. It goes
/// inside the workspace, in a directory git already ignores, with mode 0600 on
/// Unix. RFC-0014 section 4 says so out loud rather than in a footnote.
///
/// # Errors
///
/// Returns [`SinkError`] when the directory cannot be made or the file cannot
/// be written or renamed into place.
pub fn save(path: &Path, state: &SessionState, name: Option<&str>) -> Result<(), SinkError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            SinkError::new(format!(
                "cannot create session state directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let encoded = serde_json::to_vec_pretty(&Stored { state, name })
        .map_err(|error| SinkError::new(format!("cannot encode session state: {error}")))?;

    let temporary = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| SinkError::new(format!("cannot open {}: {error}", temporary.display())))?;
    let written = file
        .write_all(&encoded)
        .and_then(|()| file.sync_all())
        .map_err(|error| SinkError::new(format!("cannot write {}: {error}", temporary.display())));
    drop(file);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        SinkError::new(format!("cannot replace {}: {error}", path.display()))
    })
}

/// Reads state back.
///
/// # Errors
///
/// Returns [`SinkError`] when the file cannot be read or is not a state file.
pub fn load(path: &Path) -> Result<SessionState, SinkError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| SinkError::new(format!("cannot read {}: {error}", path.display())))?;
    serde_json::from_str(&text).map_err(|error| {
        SinkError::new(format!(
            "{} is not a session state: {error}",
            path.display()
        ))
    })
}

/// A saved conversation's model and name.
///
/// # Errors
///
/// Returns [`SinkError`] when the file cannot be read or is not a state file.
pub fn label(path: &Path) -> Result<Label, SinkError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| SinkError::new(format!("cannot read {}: {error}", path.display())))?;
    serde_json::from_str(&text).map_err(|error| {
        SinkError::new(format!(
            "{} is not a session state: {error}",
            path.display()
        ))
    })
}

/// The conversation `wanted` names: its id, or else the newest one given that
/// name.
///
/// # Errors
///
/// Returns [`SinkError`] when the sessions directory exists but cannot be read.
pub fn find(workspace: &Path, wanted: &str) -> Result<Option<(String, PathBuf)>, SinkError> {
    let path = state_path(workspace, wanted);
    if !wanted.contains(['/', '\\']) && path.is_file() {
        return Ok(Some((wanted.to_owned(), path)));
    }
    Ok(recent(workspace)?.into_iter().find_map(|saved| {
        let named = label(&saved.path).ok()?.name? == wanted;
        named.then_some((saved.id, saved.path))
    }))
}

/// Where a fork came from: its parent, and how many submissions of the
/// parent's it carries, when the workspace had checkpoints to count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fork {
    pub parent: String,
    pub at: Option<u32>,
}

/// A fork's record, beside its state, so the state file stays the adapter's.
#[must_use]
pub fn fork_path(workspace: &Path, session_id: &str) -> PathBuf {
    workspace
        .join(".dino")
        .join("sessions")
        .join(format!("{session_id}.fork.json"))
}

/// Where `session_id` was forked from, if it was.
#[must_use]
pub fn forked_from(workspace: &Path, session_id: &str) -> Option<Fork> {
    let text = std::fs::read_to_string(fork_path(workspace, session_id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Copies conversation `parent` to `child`, with its checkpoints, and records
/// where it came from. The parent is not touched.
///
/// # Errors
///
/// Returns [`SinkError`] when the state or the record cannot be written.
pub fn fork(
    workspace: &Path,
    parent: &str,
    child: &str,
    state: &SessionState,
    name: Option<&str>,
) -> Result<Fork, SinkError> {
    save(&state_path(workspace, child), state, name)?;
    let fork = Fork {
        parent: parent.to_owned(),
        at: crate::checkpoint::copy(workspace, parent, child).ok(),
    };
    let encoded = serde_json::to_vec(&fork)
        .map_err(|error| SinkError::new(format!("cannot encode the fork: {error}")))?;
    let path = fork_path(workspace, child);
    std::fs::write(&path, encoded)
        .map_err(|error| SinkError::new(format!("cannot write {}: {error}", path.display())))?;
    Ok(fork)
}

/// Saved conversations with each fork moved under its parent, as pairs of
/// index into `saved` and depth. A fork whose parent is not in `saved` stands
/// on its own.
#[must_use]
pub fn family(workspace: &Path, saved: &[Saved]) -> Vec<(usize, usize)> {
    let parents: Vec<Option<usize>> = saved
        .iter()
        .map(|item| {
            let parent = forked_from(workspace, &item.id)?.parent;
            saved.iter().position(|other| other.id == parent)
        })
        .collect();
    let mut order: Vec<(usize, usize)> = Vec::with_capacity(saved.len());
    let mut stack: Vec<(usize, usize)> = (0..saved.len())
        .rev()
        .filter(|index| parents[*index].is_none())
        .map(|index| (index, 0))
        .collect();
    while let Some((index, depth)) = stack.pop() {
        if order.iter().any(|(seen, _)| *seen == index) {
            continue;
        }
        order.push((index, depth));
        stack.extend(
            (0..saved.len())
                .rev()
                .filter(|child| parents[*child] == Some(index))
                .map(|child| (child, depth + 1)),
        );
    }
    order
}

/// A conversation whose last run ended in an error, such as the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupted {
    pub id: String,
    pub error: String,
}

/// The conversation to offer back, when the workspace's last session, other
/// than `current_run`, ended its last submission with an error.
///
/// Each submission saves its conversation after it runs, failed or not, so the
/// newest state is the one that failed unless a later session saved another.
#[must_use]
pub fn interrupted(workspace: &Path, current_run: &str) -> Option<Interrupted> {
    let directory = workspace.join(".dino").join("sessions");
    let current = format!("{current_run}.jsonl");
    let log = std::fs::read_dir(&directory)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".jsonl") && *name != current
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .max_by_key(|(modified, _)| *modified)?
        .1;
    let error = last_outcome(&log)?.error?;
    let id = most_recent(workspace).ok()??.0;
    Some(Interrupted { id, error })
}

/// The last whole line of a log, read from its tail: a long session's log runs
/// to tens of megabytes, and only its end matters here.
fn last_outcome(path: &Path) -> Option<RunOutcome> {
    // Not a whole `SessionRecord`: its `u128` times do not survive serde's
    // buffering for an internally tagged enum.
    #[derive(Deserialize)]
    struct Last {
        record: String,
        outcome: Option<RunOutcome>,
    }
    use std::io::Read as _;
    use std::io::Seek as _;
    const TAIL: u64 = 64 * 1_024;
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(std::io::SeekFrom::Start(length.saturating_sub(TAIL)))
        .ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    let line = tail.trim_end().rsplit('\n').next()?;
    let last: Last = serde_json::from_str(line).ok()?;
    (last.record == "finished")
        .then_some(last.outcome)
        .flatten()
}

/// The most recently modified session state in a workspace, if there is one.
///
/// # Errors
///
/// Returns [`SinkError`] when the sessions directory exists but cannot be read.
pub fn most_recent(workspace: &Path) -> Result<Option<(String, PathBuf)>, SinkError> {
    Ok(recent(workspace)?
        .into_iter()
        .next()
        .map(|saved| (saved.id, saved.path)))
}

/// One saved conversation in a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub id: String,
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
}

/// Every saved conversation in the workspace, newest first.
///
/// # Errors
///
/// Returns [`SinkError`] when the sessions directory exists but cannot be read.
pub fn recent(workspace: &Path) -> Result<Vec<Saved>, SinkError> {
    let directory = workspace.join(".dino").join("sessions");
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&directory)
        .map_err(|error| SinkError::new(format!("cannot read {}: {error}", directory.display())))?;

    let mut saved: Vec<Saved> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let id = path
                .file_name()?
                .to_str()?
                .strip_suffix(".state.json")?
                .to_owned();
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()?;
            Some(Saved { id, path, modified })
        })
        .collect();
    saved.sort_by_key(|item| std::cmp::Reverse(item.modified));
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::SystemTime;

    use dino_protocol::ProviderKind;
    use serde_json::json;

    use super::*;

    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dino-resume-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".dino").join("sessions")).unwrap();
        root
    }

    fn state() -> SessionState {
        SessionState {
            provider: ProviderKind::Anthropic,
            model_key: "claude-sonnet-5-5".into(),
            version: 1,
            payload: json!({"messages": []}),
        }
    }

    fn saved_at(root: &Path, id: &str, name: Option<&str>, seconds_ago: u64) {
        let path = state_path(root, id);
        save(&path, &state(), name).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(seconds_ago))
            .unwrap();
    }

    #[test]
    fn a_name_is_saved_beside_the_state_and_an_old_file_loads_without_one() {
        let root = workspace("name");
        let path = state_path(&root, "1a16-2");
        save(&path, &state(), Some("rfc wave")).unwrap();
        assert_eq!(load(&path).unwrap(), state());
        assert_eq!(label(&path).unwrap().name.as_deref(), Some("rfc wave"));

        std::fs::write(&path, serde_json::to_vec(&state()).unwrap()).unwrap();
        assert_eq!(load(&path).unwrap(), state());
        assert_eq!(
            label(&path).unwrap(),
            Label {
                model_key: "claude-sonnet-5-5".into(),
                name: None
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_takes_an_id_first_and_else_the_newest_conversation_of_that_name() {
        let root = workspace("find");
        saved_at(&root, "old", Some("wave"), 300);
        saved_at(&root, "new", Some("wave"), 100);
        saved_at(&root, "other", None, 10);
        saved_at(&root, "wave", None, 500);

        let id = |wanted: &str| find(&root, wanted).unwrap().map(|(id, _)| id);
        assert_eq!(id("old").as_deref(), Some("old"));
        assert_eq!(
            id("wave").as_deref(),
            Some("wave"),
            "an id wins over a name"
        );
        std::fs::remove_file(state_path(&root, "wave")).unwrap();
        assert_eq!(id("wave").as_deref(), Some("new"));
        assert_eq!(id("nothing"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_fork_is_a_copy_that_names_its_parent_and_is_listed_under_it() {
        let root = workspace("fork");
        saved_at(&root, "parent", Some("wave"), 300);
        saved_at(&root, "other", None, 200);
        let made = fork(&root, "parent", "child", &state(), None).unwrap();
        assert_eq!(made.parent, "parent");
        assert_eq!(load(&state_path(&root, "child")).unwrap(), state());
        assert_eq!(forked_from(&root, "child"), Some(made));
        assert_eq!(forked_from(&root, "parent"), None);
        assert_eq!(
            find(&root, "wave").unwrap().map(|(id, _)| id).as_deref(),
            Some("parent"),
            "a fork does not take its parent's name"
        );
        let grandchild = Fork {
            parent: "child".into(),
            at: Some(2),
        };
        std::fs::write(
            fork_path(&root, "grandchild"),
            serde_json::to_vec(&grandchild).unwrap(),
        )
        .unwrap();
        saved_at(&root, "grandchild", None, 0);

        let saved = recent(&root).unwrap();
        let listed: Vec<(&str, usize)> = family(&root, &saved)
            .into_iter()
            .map(|(index, depth)| (saved[index].id.as_str(), depth))
            .collect();
        assert_eq!(
            listed,
            [("other", 0), ("parent", 0), ("child", 1), ("grandchild", 2)]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn log(root: &Path, run: &str, last: &str) {
        let started = r#"{"record":"event","seq":1,"unix_millis":1,"event":{"type":"submitted","text":"go"}}"#;
        std::fs::write(
            root.join(".dino")
                .join("sessions")
                .join(format!("{run}.jsonl")),
            format!("{started}\n{last}\n"),
        )
        .unwrap();
    }

    #[test]
    fn a_last_session_that_failed_mid_task_is_offered_back_and_one_that_finished_is_not() {
        let root = workspace("interrupted");
        saved_at(&root, "1a19-1", None, 60);
        log(
            &root,
            "1a19-1",
            r#"{"record":"finished","seq":2,"unix_millis":2,"outcome":{"stop_reason":null,"model_turns":61,"error":"model transport failed: error sending request"}}"#,
        );
        assert_eq!(
            interrupted(&root, "1a20-2"),
            Some(Interrupted {
                id: "1a19-1".into(),
                error: "model transport failed: error sending request".into(),
            })
        );
        // The session just started is not the last one.
        assert_eq!(interrupted(&root, "1a19-1"), None);

        log(
            &root,
            "1a19-1",
            r#"{"record":"finished","seq":2,"unix_millis":2,"outcome":{"stop_reason":"end_turn","model_turns":3,"error":null}}"#,
        );
        assert_eq!(interrupted(&root, "1a20-2"), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
