//! The case format, and copying a fixture somewhere it can be edited.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::EvalError;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    pub prompt: String,
    /// The agent's model-turn budget for this case.
    pub max_turns: u32,
    #[serde(default)]
    pub expect: Expectations,
    /// Workspace-relative paths the model may not write, such as the tests the
    /// case is judged by.
    #[serde(default)]
    pub protected: Vec<PathBuf>,
    /// Where the case was loaded from. Not part of the file.
    #[serde(skip)]
    pub directory: PathBuf,
}

/// What decides pass or fail.
///
/// Everything here is about the outcome and is checkable by a program. What a
/// model did on the way is a metric, and metrics decide nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Expectations {
    /// `clean` requires no errors; `errors` requires at least one.
    #[serde(default)]
    pub cargo_check: Option<CheckExpectation>,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub files_unchanged: Vec<String>,
    #[serde(default)]
    pub contains: Vec<TextExpectation>,
    #[serde(default)]
    pub not_contains: Vec<TextExpectation>,
    /// Substrings the model's final answer must hold.
    ///
    /// Some tasks are answered rather than edited. Without this the harness's
    /// no-change rule would make such a case unexpressible, and a corpus that
    /// can only ask for edits will only ever learn about editing.
    #[serde(default)]
    pub answer_contains: Vec<String>,
    /// After the run, copy the case's `hidden/` tree over the workspace and
    /// require `cargo test` to pass. The model never sees these tests, so a
    /// case can check behaviour rather than only that the code compiles.
    #[serde(default)]
    pub hidden_tests: bool,
    /// The task is meant to be impossible as asked, and the model must say so
    /// with `report_blocker` rather than finish. A run that reports a blocker
    /// the case did not expect fails, so the exit cannot be used to dodge work.
    #[serde(default)]
    pub blocked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckExpectation {
    Clean,
    Errors,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextExpectation {
    pub file: String,
    pub text: String,
}

impl Case {
    /// Loads one case directory.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError`] when `case.toml` is missing or malformed, or when
    /// the case has no `workspace/` to copy.
    pub fn load(directory: impl AsRef<Path>) -> Result<Self, EvalError> {
        let directory = directory.as_ref().to_path_buf();
        let manifest = directory.join("case.toml");
        let text = std::fs::read_to_string(&manifest).map_err(|error| {
            EvalError::Case(format!("cannot read {}: {error}", manifest.display()))
        })?;
        let mut case: Self = toml::from_str(&text).map_err(|error| {
            EvalError::Case(format!("cannot parse {}: {error}", manifest.display()))
        })?;
        if !directory.join("workspace").is_dir() {
            return Err(EvalError::Case(format!(
                "case {} has no workspace/ directory to copy",
                case.name
            )));
        }
        if case.expect.hidden_tests && !directory.join("hidden").is_dir() {
            return Err(EvalError::Case(format!(
                "case {} asks for hidden tests and has no hidden/ directory",
                case.name
            )));
        }
        case.directory = directory;
        Ok(case)
    }

    /// Loads every case directory beneath a corpus root, in name order.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError`] when the corpus cannot be read or a case in it is
    /// malformed. One broken case fails the run rather than being skipped: a
    /// corpus that quietly runs eleven of its twelve cases is reporting a pass
    /// rate for a set nobody chose.
    pub fn load_corpus(root: impl AsRef<Path>) -> Result<Vec<Self>, EvalError> {
        let root = root.as_ref();
        let entries = std::fs::read_dir(root).map_err(|error| {
            EvalError::Case(format!("cannot read corpus {}: {error}", root.display()))
        })?;
        let mut directories: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.join("case.toml").is_file())
            .collect();
        directories.sort();
        directories.into_iter().map(Self::load).collect()
    }

    /// Copies the fixture into a fresh directory the run may edit.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError`] when the copy fails.
    pub fn materialise(&self, into: &Path) -> Result<PathBuf, EvalError> {
        let workspace = into.join(&self.name);
        if workspace.exists() {
            std::fs::remove_dir_all(&workspace).map_err(|error| {
                EvalError::Case(format!(
                    "cannot clear {} before copying: {error}",
                    workspace.display()
                ))
            })?;
        }
        copy_tree(&self.directory.join("workspace"), &workspace)?;
        Ok(workspace)
    }
}

/// Lays the case's hidden tests over a finished workspace.
///
/// # Errors
///
/// Returns [`EvalError`] when the copy fails.
pub fn overlay_hidden(case: &Case, workspace: &Path) -> Result<(), EvalError> {
    copy_tree(&case.directory.join("hidden"), workspace)
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), EvalError> {
    std::fs::create_dir_all(to)
        .map_err(|error| EvalError::Case(format!("cannot create {}: {error}", to.display())))?;
    let entries = std::fs::read_dir(from)
        .map_err(|error| EvalError::Case(format!("cannot read {}: {error}", from.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| EvalError::Case(format!("cannot read {}: {error}", from.display())))?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type().map_err(|error| {
            EvalError::Case(format!(
                "cannot inspect {}: {error}",
                entry.path().display()
            ))
        })?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target).map_err(|error| {
                EvalError::Case(format!("cannot copy into {}: {error}", target.display()))
            })?;
        }
        // Symbolic links in a fixture are skipped rather than followed. A
        // fixture that needs one is describing a case this harness cannot yet
        // run honestly.
    }
    Ok(())
}

/// SHA-256 of every file in a tree, relative to its root.
///
/// The same reasoning as RFC-0011 section 5: an edit and its exact reversal are
/// two edits and no change, and a harness that counted edits would disagree.
///
/// # Errors
///
/// Returns [`EvalError`] when the tree cannot be walked or a file cannot be
/// read.
pub fn fingerprint_tree(root: &Path) -> Result<BTreeMap<String, String>, EvalError> {
    let mut files = BTreeMap::new();
    collect(root, root, &mut files)?;
    Ok(files)
}

fn collect(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, String>,
) -> Result<(), EvalError> {
    let entries = std::fs::read_dir(directory).map_err(|error| {
        EvalError::Case(format!("cannot read {}: {error}", directory.display()))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            EvalError::Case(format!("cannot read {}: {error}", directory.display()))
        })?;
        let path = entry.path();
        // Build output and the agent's own session directory are not the work.
        // Nor is a lockfile Cargo writes on its first run, including the
        // harness's own check at the end of a turn.
        let name = entry.file_name();
        if matches!(name.to_string_lossy().as_ref(), "target" | ".gyr")
            || (directory == root && name == "Cargo.lock")
        {
            continue;
        }
        if path.is_dir() {
            collect(root, &path, files)?;
        } else if path.is_file() {
            let bytes = std::fs::read(&path).map_err(|error| {
                EvalError::Case(format!("cannot read {}: {error}", path.display()))
            })?;
            let name = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            files.insert(name, digest(&bytes));
        }
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    use sha2::Digest as _;

    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut encoded, byte| {
            let _ = write!(&mut encoded, "{byte:02x}");
            encoded
        })
}
