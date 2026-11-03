//! What a page may choose between: the saved sessions of the workspace it is
//! in, and the workspaces the person listed or has used. A page picks from
//! these lists; a path or id it names is only ever matched against them.

use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::Serialize;

/// Sessions listed to a page, newest first.
const LISTED: usize = 50;

/// Recently used workspaces kept, and offered.
const RECENT: usize = 10;

/// Where the workspaces a page may move to come from.
#[derive(Debug, Clone, Default)]
pub struct Workspaces {
    /// `workspaces` in the user file.
    pub configured: Vec<PathBuf>,
    /// The file of recently used workspaces, one a line, newest last.
    pub recent: Option<PathBuf>,
}

/// One workspace a page may pick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Place {
    pub path: PathBuf,
    pub current: bool,
    pub configured: bool,
    pub recent: bool,
}

impl Workspaces {
    /// The current workspace, then the configured ones, then the recent ones
    /// not already listed. Only directories that exist now.
    #[must_use]
    pub fn offered(&self, current: &Path) -> Vec<Place> {
        let mut places = vec![Place {
            path: current.to_path_buf(),
            current: true,
            configured: false,
            recent: false,
        }];
        let configured = self.configured.iter().filter_map(|path| resolve(path));
        let recent = self
            .recent
            .as_deref()
            .map(recent)
            .unwrap_or_default()
            .into_iter()
            .rev()
            .filter_map(|path| resolve(&path))
            .take(RECENT);
        for (path, is_configured) in configured
            .map(|path| (path, true))
            .chain(recent.map(|path| (path, false)))
        {
            match places.iter_mut().find(|place| place.path == path) {
                Some(place) => {
                    place.configured |= is_configured;
                    place.recent |= !is_configured;
                }
                None => places.push(Place {
                    path,
                    current: false,
                    configured: is_configured,
                    recent: !is_configured,
                }),
            }
        }
        places
    }
}

/// `path` canonical, with a leading `~/` read as the home directory, or `None`
/// when it is not a directory.
fn resolve(path: &Path) -> Option<PathBuf> {
    let expanded = match path.strip_prefix("~") {
        Ok(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        Err(_) => path.to_path_buf(),
    };
    std::fs::canonicalize(expanded)
        .ok()
        .filter(|path| path.is_dir())
}

fn recent(file: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Records `workspace` as just used.
///
/// # Errors
///
/// Returns an error when the file cannot be written.
pub fn note_workspace(file: &Path, workspace: &Path) -> std::io::Result<()> {
    let mut paths = recent(file);
    paths.retain(|path| path != workspace);
    paths.push(workspace.to_path_buf());
    let keep = paths.len().saturating_sub(RECENT * 2);
    let mut text = String::new();
    for path in &paths[keep..] {
        if let Some(path) = path.to_str().filter(|path| !path.contains('\n')) {
            text.push_str(path);
            text.push('\n');
        }
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file, text)
}

/// One session of a workspace: a saved conversation, a log, or both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SavedSession {
    pub id: String,
    /// What `/rename` called it.
    pub name: Option<String>,
    pub model: Option<String>,
    pub age_seconds: u64,
    /// A saved conversation, which a session can continue.
    pub resumable: bool,
    /// A log, whose trajectory can be read.
    pub traceable: bool,
}

/// Whether `id` could name a session file, and nothing outside the directory.
fn plausible(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The workspace's sessions, newest first, at most [`LISTED`].
#[must_use]
pub fn sessions(root: &Path) -> Vec<SavedSession> {
    let directory = root.join(".dino").join("sessions");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut found: std::collections::BTreeMap<String, (SystemTime, bool, bool)> =
        std::collections::BTreeMap::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let (id, resumable) = if let Some(id) = name.strip_suffix(".state.json") {
            (id, true)
        } else if let Some(id) = name.strip_suffix(".jsonl") {
            (id, false)
        } else {
            continue;
        };
        if !plausible(id) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|data| data.modified()) else {
            continue;
        };
        let slot = found
            .entry(id.to_owned())
            .or_insert((SystemTime::UNIX_EPOCH, false, false));
        slot.0 = slot.0.max(modified);
        if resumable {
            slot.1 = true;
        } else {
            slot.2 = true;
        }
    }
    let mut listed: Vec<(String, (SystemTime, bool, bool))> = found.into_iter().collect();
    listed.sort_by_key(|(_, (modified, _, _))| std::cmp::Reverse(*modified));
    listed.truncate(LISTED);
    let now = SystemTime::now();
    listed
        .into_iter()
        .map(|(id, (modified, resumable, traceable))| {
            let label = resumable
                .then(|| dino_core::resume::label(&directory.join(format!("{id}.state.json"))))
                .and_then(Result::ok);
            let model = label
                .as_ref()
                .map(|label| label.model_key.clone())
                .or_else(|| {
                    traceable
                        .then(|| logged_model(&directory.join(format!("{id}.jsonl"))))
                        .flatten()
                });
            SavedSession {
                name: label.and_then(|label| label.name),
                model,
                age_seconds: now.duration_since(modified).map_or(0, |age| age.as_secs()),
                id,
                resumable,
                traceable,
            }
        })
        .collect()
}

/// The model a log's opening record names, read from its first line only.
fn logged_model(path: &Path) -> Option<String> {
    use std::io::BufRead as _;
    use std::io::Read as _;
    let mut first = String::new();
    std::io::BufReader::new(std::fs::File::open(path).ok()?.take(64 * 1_024))
        .read_line(&mut first)
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&first).ok()?;
    value["session"]["model_key"].as_str().map(str::to_owned)
}

/// The log of session `id` in `root`, when `id` is a plausible id and the log
/// exists.
#[must_use]
pub fn log_path(root: &Path, id: &str) -> Option<PathBuf> {
    if !plausible(id) {
        return None;
    }
    let path = root
        .join(".dino")
        .join("sessions")
        .join(format!("{id}.jsonl"));
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dino-places-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".dino").join("sessions")).unwrap();
        std::fs::canonicalize(root).unwrap()
    }

    #[test]
    fn a_session_is_listed_once_with_its_name_model_and_what_can_be_done_with_it() {
        let root = scratch("sessions");
        let sessions = root.join(".dino").join("sessions");
        std::fs::write(
            sessions.join("a1-2.state.json"),
            r#"{"provider":"anthropic","model_key":"claude-x","version":1,"payload":{},"name":"wave"}"#,
        )
        .unwrap();
        std::fs::write(
            sessions.join("a1-2.jsonl"),
            r#"{"record":"started","seq":0,"unix_millis":1,"session":{"model_key":"claude-x"}}"#,
        )
        .unwrap();
        std::fs::write(
            sessions.join("b3-4.jsonl"),
            "{\"record\":\"started\",\"seq\":0,\"unix_millis\":1,\"session\":{\"model_key\":\"qwen\"}}\n",
        )
        .unwrap();
        std::fs::write(sessions.join("notes.txt"), "").unwrap();
        std::fs::write(sessions.join("bad id.jsonl"), "").unwrap();

        let mut listed = super::sessions(&root);
        listed.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed[0].id, "a1-2");
        assert_eq!(listed[0].name.as_deref(), Some("wave"));
        assert!(listed[0].resumable && listed[0].traceable);
        assert_eq!(listed[1].model.as_deref(), Some("qwen"));
        assert!(!listed[1].resumable && listed[1].traceable);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_log_is_found_only_by_an_id_that_stays_in_the_directory() {
        let root = scratch("logs");
        std::fs::write(root.join(".dino").join("sessions").join("a1-2.jsonl"), "").unwrap();
        std::fs::write(root.join("secret.jsonl"), "").unwrap();
        assert!(log_path(&root, "a1-2").is_some());
        for refused in [
            "../../secret",
            "a1-2/..",
            "",
            "a1-3",
            "a1-2.jsonl",
            "/etc/passwd",
        ] {
            assert_eq!(log_path(&root, refused), None, "{refused}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspaces_offered_are_the_current_the_configured_and_the_recent_that_exist() {
        let root = scratch("workspaces");
        let (one, two, three) = (root.join("one"), root.join("two"), root.join("three"));
        for path in [&one, &two, &three] {
            std::fs::create_dir_all(path).unwrap();
        }
        let file = root.join("recent");
        note_workspace(&file, &two).unwrap();
        note_workspace(&file, &three).unwrap();
        note_workspace(&file, &root.join("gone")).unwrap();
        note_workspace(&file, &two).unwrap();
        let mut text = String::new();
        std::fs::File::open(&file)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text.lines().last(), Some(two.to_str().unwrap()), "{text}");
        assert_eq!(text.lines().count(), 3, "noted once each: {text}");

        let workspaces = Workspaces {
            configured: vec![one.clone(), two.clone(), root.join("missing")],
            recent: Some(file),
        };
        let offered = workspaces.offered(&root);
        let paths: Vec<&Path> = offered.iter().map(|place| place.path.as_path()).collect();
        assert_eq!(paths, [root.as_path(), &one, &two, &three]);
        assert!(offered[0].current);
        assert!(offered[2].configured && offered[2].recent);
        assert!(!offered[3].configured && offered[3].recent);
        let _ = std::fs::remove_dir_all(&root);
    }
}
