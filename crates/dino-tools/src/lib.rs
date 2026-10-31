//! Bounded filesystem tools rooted in one canonical workspace.

use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::workspace::WorkspaceRoot;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolLimits {
    pub max_read_lines: usize,
    pub max_read_bytes: usize,
    pub max_search_matches: usize,
    pub max_search_bytes: usize,
    pub max_search_files: usize,
    pub max_search_file_bytes: u64,
    pub max_list_entries: usize,
    pub max_list_bytes: usize,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_read_lines: 200,
            max_read_bytes: 32 * 1_024,
            max_search_matches: 200,
            max_search_bytes: 64 * 1_024,
            max_search_files: 20_000,
            max_search_file_bytes: 2 * 1_024 * 1_024,
            max_list_entries: 500,
            max_list_bytes: 32 * 1_024,
        }
    }
}

#[derive(Debug)]
pub struct WorkspaceTools {
    root: WorkspaceRoot,
    limits: ToolLimits,
    /// The fingerprint each file had when the model last saw it, through a read
    /// or through its own write. A file that still has it has not changed
    /// behind the model's back, whatever hash the model copies into a patch.
    seen: Mutex<HashMap<PathBuf, String>>,
    /// Workspace-relative paths no tool may write, such as the tests a task is
    /// judged by. A model under pressure edits those to pass.
    protected: Vec<PathBuf>,
    /// Every fingerprint each file has had in this session, oldest first. An
    /// edit that lands on one of them has undone work, which a model going in
    /// circles does not notice on its own.
    versions: Mutex<HashMap<PathBuf, Vec<String>>>,
    /// Ranges already returned in this session, with the fingerprint they were
    /// read at. The same range of an unchanged file is how a turn gets spent twice.
    reads: Mutex<HashMap<PathBuf, Vec<ReadSpan>>>,
}

impl WorkspaceTools {
    /// Creates a tool runtime confined to an existing directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be canonicalised or is not a
    /// directory.
    pub fn new(root: impl AsRef<Path>, limits: ToolLimits) -> Result<Self, ToolError> {
        Ok(Self {
            root: WorkspaceRoot::new(root)?,
            limits,
            seen: Mutex::new(HashMap::new()),
            protected: Vec::new(),
            versions: Mutex::new(HashMap::new()),
            reads: Mutex::new(HashMap::new()),
        })
    }

    /// Refuses writes beneath these workspace-relative paths.
    #[must_use]
    pub fn protecting(mut self, paths: Vec<PathBuf>) -> Self {
        self.protected = paths;
        self
    }

    /// Resolves a path for writing, refusing one that is protected.
    fn writable(&self, path: &str, existing: bool) -> Result<PathBuf, ToolError> {
        let resolved = if existing {
            self.root.resolve_file(path)?
        } else {
            self.root.resolve_writable(path)?
        };
        let relative = PathBuf::from(self.root.relative(&resolved)?);
        if self
            .protected
            .iter()
            .any(|protected| relative.starts_with(protected))
        {
            return Err(ToolError::new(format!(
                "{path} is protected and cannot be changed. If the task cannot be done \
                 without changing it, call report_blocker and say why."
            )));
        }
        Ok(resolved)
    }

    #[must_use]
    fn tool_definitions() -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "read".into(),
                description: "Read a bounded range from a UTF-8 file in the workspace. The same \
                              range of an unchanged file returns a note instead of the bytes; \
                              pass again to read it once more."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1},
                        "start_line": {"type": "integer", "minimum": 1},
                        "end_line": {"type": "integer", "minimum": 1},
                        "again": {
                            "type": "boolean",
                            "description": "Return the bytes even when this range was already read and the file has not changed."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
            ToolDefinition {
                name: "search".into(),
                description: "Search UTF-8 workspace files, respecting ignore files. The query is \
                     literal unless regex is true. path narrows it to a directory or a single \
                     file, and glob to files whose workspace path matches, such as *.rs or \
                     src/**/*.ts."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "minLength": 1},
                        "path": {"type": "string", "minLength": 1},
                        "max_results": {"type": "integer", "minimum": 1},
                        "regex": {"type": "boolean", "description": "Read query as a Rust regular expression."},
                        "ignore_case": {"type": "boolean"},
                        "glob": {"type": "string", "minLength": 1, "description": "Only files whose workspace path matches."}
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
            },
            ToolDefinition {
                name: "list".into(),
                description: "List the files and directories in the workspace. Ignored and \
                              hidden entries are left out unless you ask for them with all, \
                              which is how to see build output, dotfiles and .gitignore."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1},
                        "depth": {"type": "integer", "minimum": 1},
                        "pattern": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Only files whose workspace path matches this glob, at any depth, such as **/*_test.go."
                        },
                        "all": {
                            "type": "boolean",
                            "description": "Include ignored and hidden entries."
                        }
                    },
                    "additionalProperties": false
                }),
            },
            ToolDefinition {
                name: "apply_patch".into(),
                description: "Replace exact UTF-8 text in a previously read workspace file: one \
                              old_text with new_text, or several replacements at once in edits. \
                              Each old_text must occur exactly once in the file as it was before \
                              the call, and edits may not overlap."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1},
                        "expected_sha256": {
                            "type": "string",
                            "minLength": 8,
                            "pattern": "^[0-9a-f]{8,64}$",
                            "description": "The sha256 from reading the file. May be left out \
                                            when the file has not changed since you last read \
                                            or wrote it."
                        },
                        "old_text": {"type": "string", "minLength": 1},
                        "new_text": {"type": "string"},
                        "edits": {
                            "type": "array",
                            "minItems": 1,
                            "description": "Several replacements in this file, in place of \
                                            old_text and new_text.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_text": {"type": "string", "minLength": 1},
                                    "new_text": {"type": "string"}
                                },
                                "required": ["old_text", "new_text"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
            ToolDefinition {
                name: "write_file".into(),
                description: "Write a whole UTF-8 file in the workspace, creating it and any \
                              missing directories. A file that already exists must have been \
                              read first; for a small change to one, apply_patch is cheaper."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1},
                        "content": {"type": "string"},
                        "expected_sha256": {"type": "string", "minLength": 64, "pattern": "^[0-9a-f]{64}$"}
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }),
            },
        ]
    }

    /// Classifies a call using the same path resolution execution will use.
    ///
    /// The subject reported for `apply_patch` is therefore the file that would
    /// actually be written, not the string the model supplied. An approval
    /// granted for one file cannot be spent on another by way of a symbolic
    /// link or an unusual spelling of the same path.
    fn classify_sync(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        match call.name.as_str() {
            "read" => {
                let _: ReadArguments = parse_arguments(&call.arguments, "read")?;
                Ok(ToolAction::read_only())
            }
            "search" => {
                let _: SearchArguments = parse_arguments(&call.arguments, "search")?;
                Ok(ToolAction::read_only())
            }
            "list" => {
                let _: ListArguments = parse_arguments(&call.arguments, "list")?;
                Ok(ToolAction::read_only())
            }
            "apply_patch" => {
                let arguments: PatchArguments = parse_arguments(&call.arguments, "apply_patch")?;
                arguments.edits()?;
                let resolved = self.writable(&arguments.path, true)?;
                Ok(ToolAction::mutating(self.root.relative(&resolved)?))
            }
            "write_file" => {
                let arguments: WriteArguments = parse_arguments(&call.arguments, "write_file")?;
                let resolved = self.writable(&arguments.path, false)?;
                Ok(ToolAction::mutating(self.root.relative(&resolved)?))
            }
            name => Err(ToolError::new(format!("unknown tool {name:?}"))),
        }
    }

    fn execute_sync(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        match call.name.as_str() {
            "read" => self.read(&parse_arguments(&call.arguments, "read")?),
            "search" => self.search(parse_arguments(&call.arguments, "search")?),
            "list" => self.list(&parse_arguments(&call.arguments, "list")?),
            "apply_patch" => self.apply_patch(&parse_arguments(&call.arguments, "apply_patch")?),
            "write_file" => self.write_file(&parse_arguments(&call.arguments, "write_file")?),
            name => Err(ToolError::new(format!("unknown tool {name:?}"))),
        }
    }

    fn read(&self, arguments: &ReadArguments) -> Result<ToolOutput, ToolError> {
        let path = self.root.resolve_file(&arguments.path)?;
        let bytes = fs::read(&path).map_err(|error| file_error("read", &path, &error))?;
        let text = String::from_utf8(bytes.clone())
            .map_err(|_| ToolError::new(format!("file is not valid UTF-8: {}", arguments.path)))?;
        let total_lines = text.lines().count();
        let start_line = arguments.start_line.unwrap_or(1);
        if start_line == 0 {
            return Err(ToolError::new("start_line must be at least 1"));
        }
        let requested_end = arguments.end_line.unwrap_or_else(|| {
            start_line
                .saturating_add(self.limits.max_read_lines)
                .saturating_sub(1)
        });
        if requested_end < start_line {
            return Err(ToolError::new("end_line must not precede start_line"));
        }
        let capped_end = requested_end.min(
            start_line
                .saturating_add(self.limits.max_read_lines)
                .saturating_sub(1),
        );
        let end_line = capped_end.min(total_lines);
        let mut content = String::new();
        if start_line <= total_lines {
            for (index, line) in text
                .lines()
                .enumerate()
                .skip(start_line - 1)
                .take(end_line - start_line + 1)
            {
                use std::fmt::Write as _;
                writeln!(&mut content, "{:>6}\t{line}", index + 1)
                    .expect("writing to a String cannot fail");
            }
        }
        let byte_truncated = truncate_utf8(&mut content, self.limits.max_read_bytes);
        let truncated = byte_truncated || end_line < requested_end.min(total_lines);
        let sha256 = fingerprint(&bytes);
        // Recorded before the stall, so a withheld re-read still counts as
        // having seen the file. `apply_patch` would otherwise demand another read.
        self.remember(&path, &sha256);
        let span = ReadSpan {
            start_line,
            end_line,
            sha256: sha256.clone(),
        };
        if !arguments.again && self.already_read(&path, &span) {
            let content = format!(
                "unchanged since the read of lines {start_line}-{end_line} at SHA-256 \
                 {sha256}. Pass again to read it once more.\n"
            );
            return json_output(&ReadOutput {
                path: self.root.relative(&path)?,
                start_line,
                end_line,
                total_lines,
                truncated,
                sha256,
                content,
                repeated: true,
            });
        }
        self.remember_read(&path, span);
        let output = ReadOutput {
            path: self.root.relative(&path)?,
            start_line,
            end_line,
            total_lines,
            truncated,
            sha256,
            content,
            repeated: false,
        };
        json_output(&output)
    }

    /// Whether this exact resolved range was already returned for these bytes.
    fn already_read(&self, path: &Path, span: &ReadSpan) -> bool {
        self.reads
            .lock()
            .is_ok_and(|reads| reads.get(path).is_some_and(|spans| spans.contains(span)))
    }

    fn remember_read(&self, path: &Path, span: ReadSpan) {
        let Ok(mut reads) = self.reads.lock() else {
            return;
        };
        let spans = reads.entry(path.to_path_buf()).or_default();
        // A new fingerprint retires ranges read at the old one. The file
        // changed, so those ranges are not the bytes on disk.
        spans.retain(|known| known.sha256 == span.sha256);
        if !spans.contains(&span) {
            spans.push(span);
        }
    }

    fn search(&self, arguments: SearchArguments) -> Result<ToolOutput, ToolError> {
        if arguments.query.is_empty() {
            return Err(ToolError::new("search query must not be empty"));
        }
        let relative_root = arguments.path.as_deref().unwrap_or(".");
        // A file is a fair thing to search; refusing one cost a turn in every
        // long eval run.
        let search_root = self.root.resolve(relative_root)?;
        let result_limit = arguments
            .max_results
            .unwrap_or(self.limits.max_search_matches)
            .min(self.limits.max_search_matches);
        let pattern = if arguments.regex {
            arguments.query.clone()
        } else {
            regex::escape(&arguments.query)
        };
        let needle = regex::RegexBuilder::new(&pattern)
            .case_insensitive(arguments.ignore_case)
            .build()
            .map_err(|error| ToolError::new(format!("invalid search pattern: {error}")))?;
        let files = arguments.glob.as_deref().map(glob_matcher).transpose()?;
        let mut builder = WalkBuilder::new(search_root);
        builder
            .follow_links(false)
            .parents(false)
            .require_git(false)
            .max_filesize(Some(self.limits.max_search_file_bytes))
            .sort_by_file_path(std::cmp::Ord::cmp);

        let mut matches = Vec::new();
        let mut total_matches = 0_usize;
        let mut files_scanned = 0_usize;
        let mut output_bytes = 0_usize;
        let mut truncated = false;
        for entry in builder.build() {
            let entry =
                entry.map_err(|error| ToolError::new(format!("search walk failed: {error}")))?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if files_scanned == self.limits.max_search_files {
                truncated = true;
                break;
            }
            let relative = self.root.relative(entry.path())?;
            if files
                .as_ref()
                .is_some_and(|files| !files.is_match(&relative))
            {
                continue;
            }
            files_scanned += 1;
            let Ok(text) = fs::read_to_string(entry.path()) else {
                continue;
            };
            for (line_index, line) in text.lines().enumerate() {
                for column in needle.find_iter(line).map(|found| found.start()) {
                    total_matches += 1;
                    if matches.len() == result_limit {
                        truncated = true;
                        continue;
                    }
                    let found = SearchMatch {
                        path: relative.clone(),
                        line: line_index + 1,
                        column: column + 1,
                        text: line.to_owned(),
                    };
                    let encoded_size = serde_json::to_vec(&found)
                        .map_err(|error| {
                            ToolError::new(format!("cannot encode search match: {error}"))
                        })?
                        .len();
                    if output_bytes.saturating_add(encoded_size) > self.limits.max_search_bytes {
                        truncated = true;
                        continue;
                    }
                    output_bytes += encoded_size;
                    matches.push(found);
                }
            }
        }
        let output = SearchOutput {
            query: arguments.query,
            files_scanned,
            total_matches,
            matches,
            truncated,
        };
        json_output(&output)
    }

    /// Lists what is in the workspace, respecting the same ignore rules search
    /// uses so a listing is not mostly build output.
    ///
    /// Added because the eval corpus caught a model reaching for
    /// `exec find . -name "*.rs"`: `search` finds text and `read` reads a known
    /// path, and neither answered "what files are here". RFC-0005 section 7.
    fn list(&self, arguments: &ListArguments) -> Result<ToolOutput, ToolError> {
        let relative = arguments.path.as_deref().unwrap_or(".");
        let root = self.root.resolve_directory(relative)?;
        let mut builder = WalkBuilder::new(&root);
        builder
            .follow_links(false)
            .parents(false)
            .require_git(false)
            .sort_by_file_path(std::cmp::Ord::cmp);
        if arguments.all {
            builder
                .hidden(false)
                .ignore(false)
                .git_ignore(false)
                .git_exclude(false)
                .git_global(false);
        }
        if let Some(depth) = arguments.depth {
            builder.max_depth(Some(depth));
        }
        let pattern = arguments.pattern.as_deref().map(glob_matcher).transpose()?;

        let mut entries = Vec::new();
        let mut total_entries = 0_usize;
        let mut output_bytes = 0_usize;
        let mut truncated = false;
        for entry in builder.build() {
            let entry =
                entry.map_err(|error| ToolError::new(format!("list walk failed: {error}")))?;
            // The walk yields its own root first, which is not an entry in it.
            if entry.path() == root {
                continue;
            }
            let Some(kind) = entry.file_type() else {
                continue;
            };
            if let Some(pattern) = &pattern
                && !(kind.is_file() && pattern.is_match(self.root.relative(entry.path())?))
            {
                continue;
            }
            total_entries += 1;
            if entries.len() == self.limits.max_list_entries {
                truncated = true;
                continue;
            }
            let listed = ListEntry {
                path: self.root.relative(entry.path())?,
                kind: if kind.is_dir() { "directory" } else { "file" },
                bytes: if kind.is_file() {
                    entry.metadata().ok().map(|metadata| metadata.len())
                } else {
                    None
                },
            };
            let encoded = serde_json::to_vec(&listed)
                .map_err(|error| ToolError::new(format!("cannot encode entry: {error}")))?
                .len();
            if output_bytes.saturating_add(encoded) > self.limits.max_list_bytes {
                truncated = true;
                continue;
            }
            output_bytes += encoded;
            entries.push(listed);
        }

        json_output(&ListOutput {
            path: relative.to_owned(),
            all: arguments.all,
            entries,
            total_entries,
            truncated,
        })
    }

    fn apply_patch(&self, arguments: &PatchArguments) -> Result<ToolOutput, ToolError> {
        let edits = arguments.edits()?;
        if edits.iter().any(|edit| edit.old_text.is_empty()) {
            return Err(ToolError::new("old_text must not be empty"));
        }
        if edits.iter().all(|edit| edit.old_text == edit.new_text) {
            return Err(ToolError::new("patch would not change the file"));
        }
        let path = self.writable(&arguments.path, true)?;
        let bytes = fs::read(&path).map_err(|error| file_error("read", &path, &error))?;
        let before_sha256 = fingerprint(&bytes);
        self.check_current(
            &path,
            &arguments.path,
            arguments.expected_sha256.as_deref(),
            &before_sha256,
        )?;
        let text = String::from_utf8(bytes)
            .map_err(|_| ToolError::new(format!("file is not valid UTF-8: {}", arguments.path)))?;
        let updated = replace_all(&text, &edits)?;
        write_atomic(&path, updated.as_bytes())?;
        let after_sha256 = fingerprint(updated.as_bytes());
        let note = self.notes(&path, &after_sha256, &arguments.path)?;
        self.remember(&path, &after_sha256);
        let output = PatchOutput {
            path: self.root.relative(&path)?,
            before_sha256,
            after_sha256,
            bytes_written: updated.len(),
            note,
        };
        json_output(&output)
    }

    /// What the model should know about a write it has just made.
    fn notes(&self, path: &Path, sha256: &str, shown: &str) -> Result<Option<String>, ToolError> {
        let relative = PathBuf::from(self.root.relative(path)?);
        let later = dino_sandbox::EXECUTABLE_CONFIG
            .iter()
            .any(|config| relative.starts_with(config))
            .then(|| {
                format!(
                    "{shown} changes what other programs do later: where packages come from, or \
                     code run at install, build, commit or cd. The person did not ask for it \
                     unless they said so; name the change and why in your final answer."
                )
            });
        let notes: Vec<String> = [self.revisit_note(path, sha256, shown), later]
            .into_iter()
            .flatten()
            .collect();
        Ok((!notes.is_empty()).then(|| notes.join(" ")))
    }

    fn remember(&self, path: &Path, sha256: &str) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(path.to_path_buf(), sha256.to_owned());
        }
        if let Ok(mut versions) = self.versions.lock() {
            let known = versions.entry(path.to_path_buf()).or_default();
            if !known.iter().any(|version| version == sha256) {
                known.push(sha256.to_owned());
            }
        }
    }

    /// Says so when a write puts a file back to a version it had before.
    fn revisit_note(&self, path: &Path, sha256: &str, shown: &str) -> Option<String> {
        let versions = self.versions.lock().ok()?;
        let known = versions.get(path)?;
        known.iter().any(|version| version == sha256).then(|| {
            format!(
                "this edit returns {shown} to a version it had earlier in this session; \
                 if that was not the intent, the last changes are going round in circles"
            )
        })
    }

    /// Refuses to change a file the model has not seen as it now is.
    ///
    /// Seen means read or written through these tools at its current
    /// fingerprint, or named by a hash, or a prefix of at least eight
    /// characters, that matches. Anything else changed behind the model's back.
    fn check_current(
        &self,
        path: &Path,
        shown: &str,
        expected: Option<&str>,
        current: &str,
    ) -> Result<(), ToolError> {
        let seen = self
            .seen
            .lock()
            .ok()
            .and_then(|seen| seen.get(path).cloned());
        let named = expected.is_some_and(|hash| hash.len() >= 8 && current.starts_with(hash));
        if named || seen.as_deref() == Some(current) {
            return Ok(());
        }
        match (seen, expected) {
            (None, None) => Err(ToolError::new(format!(
                "read {shown} before changing it, or pass its sha256"
            ))),
            (_, Some(expected)) => Err(ToolError::new(format!(
                "stale file: expected SHA-256 {expected}, found {current}; it has changed \
                 since you last saw it, so read it again"
            ))),
            (Some(last), None) => Err(ToolError::new(format!(
                "stale file: last seen at SHA-256 {last}, found {current}; it has changed \
                 since you last saw it, so read it again"
            ))),
        }
    }

    fn write_file(&self, arguments: &WriteArguments) -> Result<ToolOutput, ToolError> {
        let path = self.writable(&arguments.path, false)?;
        let created = match fs::read(&path) {
            Ok(bytes) => {
                let found = fingerprint(&bytes);
                let seen = self.seen.lock().is_ok_and(|seen| seen.contains_key(&path));
                if !seen && arguments.expected_sha256.is_none() {
                    return Err(ToolError::new(format!(
                        "{} already exists: read it before replacing it, or use apply_patch \
                         to change part of it",
                        arguments.path
                    )));
                }
                self.check_current(
                    &path,
                    &arguments.path,
                    arguments.expected_sha256.as_deref(),
                    &found,
                )?;
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if arguments.expected_sha256.is_some() {
                    return Err(ToolError::new(format!(
                        "{} does not exist, so there is no sha256 to match; leave \
                         expected_sha256 out to create it",
                        arguments.path
                    )));
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| file_error("create the directory for", &path, &error))?;
                }
                true
            }
            Err(error) => return Err(file_error("read", &path, &error)),
        };
        write_atomic(&path, arguments.content.as_bytes())?;
        let sha256 = fingerprint(arguments.content.as_bytes());
        let note = self.notes(&path, &sha256, &arguments.path)?;
        self.remember(&path, &sha256);
        json_output(&WriteOutput {
            note,
            path: self.root.relative(&path)?,
            created,
            sha256,
            bytes_written: arguments.content.len(),
        })
    }
}

impl ToolRuntime for WorkspaceTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Self::tool_definitions()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        self.classify_sync(call)
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let result = self.execute_sync(call);
        Box::pin(async move { result })
    }

    /// Repeat reads are withheld only while the model still holds the first
    /// one. `seen` stays: it is about the bytes on disk, not the conversation.
    fn history_lost(&self) {
        if let Ok(mut reads) = self.reads.lock() {
            reads.clear();
        }
    }
}

/// A glob over workspace paths. `*` stops at `/` and `**` crosses it, so
/// `*.rs` means a file at the top and `**/*.rs` means one anywhere; a pattern
/// with no `/` matches a file name at any depth, as people expect of `*.rs`.
fn glob_matcher(pattern: &str) -> Result<globset::GlobMatcher, ToolError> {
    let pattern = if pattern.contains('/') {
        pattern.to_owned()
    } else {
        format!("**/{pattern}")
    };
    globset::GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| ToolError::new(format!("invalid glob {pattern:?}: {error}")))
}

fn parse_arguments<T>(arguments: &Value, tool: &str) -> Result<T, ToolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::new(format!("invalid {tool} arguments: {error}")))
}

fn json_output<T: Serialize>(value: &T) -> Result<ToolOutput, ToolError> {
    serde_json::to_string(value)
        .map(ToolOutput::success)
        .map_err(|error| ToolError::new(format!("cannot encode tool output: {error}")))
}

fn fingerprint(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes);
    digest.iter().fold(
        String::with_capacity(digest.len() * 2),
        |mut encoded, byte| {
            write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
            encoded
        },
    )
}

fn truncate_utf8(value: &mut String, max_bytes: usize) -> bool {
    if value.len() <= max_bytes {
        return false;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    true
}

fn file_error(action: &str, path: &Path, error: &std::io::Error) -> ToolError {
    ToolError::new(format!("cannot {action} {}: {error}", path.display()))
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), ToolError> {
    let parent = path.parent().ok_or_else(|| {
        ToolError::new(format!("file has no parent directory: {}", path.display()))
    })?;
    // A new file keeps the permissions its creation gave it.
    let permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(file_error("inspect", path, &error)),
    };
    let mut last_error = None;
    for _ in 0..16 {
        let serial = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(".dino-{}-{serial}.tmp", std::process::id()));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
        {
            Ok(mut file) => {
                let result = (|| {
                    if let Some(permissions) = &permissions {
                        file.set_permissions(permissions.clone())?;
                    }
                    file.write_all(contents)?;
                    file.sync_all()?;
                    drop(file);
                    fs::rename(&temp_path, path)
                })();
                if let Err(error) = result {
                    let _ = fs::remove_file(&temp_path);
                    return Err(file_error("replace", path, &error));
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(file_error("create temporary file for", path, &error)),
        }
    }
    Err(file_error(
        "create temporary file for",
        path,
        &last_error.expect("at least one temporary file collision"),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    /// Return the bytes even when this range was already read unchanged.
    #[serde(default)]
    again: bool,
}

/// One range already handed back, at the bytes it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadSpan {
    start_line: usize,
    end_line: usize,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReadOutput {
    path: String,
    start_line: usize,
    end_line: usize,
    total_lines: usize,
    truncated: bool,
    sha256: String,
    content: String,
    /// The bytes were withheld because this range was already returned.
    #[serde(default, skip_serializing_if = "is_false")]
    repeated: bool,
}

// `skip_serializing_if` is given a reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
    path: Option<String>,
    max_results: Option<usize>,
    /// Read the query as a regular expression rather than literally.
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    ignore_case: bool,
    /// Only files whose workspace path matches this glob, such as `*.rs`.
    glob: Option<String>,
}

#[derive(Debug, Serialize)]
struct SearchOutput {
    query: String,
    files_scanned: usize,
    total_matches: usize,
    matches: Vec<SearchMatch>,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct SearchMatch {
    path: String,
    line: usize,
    column: usize,
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArguments {
    path: Option<String>,
    depth: Option<usize>,
    /// Only files whose workspace path matches this glob, at any depth.
    pattern: Option<String>,
    /// Include ignored and hidden entries.
    ///
    /// Added because an eval run caught the model reaching for
    /// `exec find . -name .dino`: the ignore-aware walk that keeps a listing from
    /// being mostly build output also makes it blind, and there was no way to
    /// ask. RFC-0005 section 6.1.
    #[serde(default)]
    all: bool,
}

#[derive(Debug, Serialize)]
struct ListEntry {
    path: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ListOutput {
    path: String,
    /// Whether ignored and hidden entries were included, so an absence in this
    /// listing can be read correctly.
    all: bool,
    entries: Vec<ListEntry>,
    /// Everything the walk saw, so a capped list never reads as a whole one.
    total_entries: usize,
    truncated: bool,
}

/// Applies every edit to `text`, each matched against the text as it was.
fn replace_all(text: &str, edits: &[Edit]) -> Result<String, ToolError> {
    let mut spans = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let which = if edits.len() == 1 {
            String::new()
        } else {
            format!("edit {}: ", index + 1)
        };
        let found: Vec<usize> = text
            .match_indices(&edit.old_text)
            .map(|(at, _)| at)
            .collect();
        match found.as_slice() {
            [at] => spans.push((*at, *at + edit.old_text.len(), edit)),
            [] => {
                return Err(ToolError::new(format!(
                    "{which}old_text must match exactly once, found 0 matches{}",
                    nearest_lines(text, &edit.old_text)
                )));
            }
            _ => {
                return Err(ToolError::new(format!(
                    "{which}old_text must match exactly once, found {} matches; include more \
                     surrounding lines to make it unique",
                    found.len()
                )));
            }
        }
    }
    spans.sort_by_key(|(start, _, _)| *start);
    if spans.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(ToolError::new(
            "two edits overlap; merge them into one or make them disjoint",
        ));
    }
    let mut updated = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, end, edit) in spans {
        updated.push_str(&text[copied..start]);
        updated.push_str(&edit.new_text);
        copied = end;
    }
    updated.push_str(&text[copied..]);
    Ok(updated)
}

/// How many file lines a miss shows, at most.
const NEAREST_LINES: usize = 12;

/// The file's lines where a missed `old_text` most likely meant, so the next
/// attempt can copy them. Matching ignores leading and trailing whitespace on
/// each line; nothing is applied on the strength of it.
fn nearest_lines(text: &str, old_text: &str) -> String {
    let file: Vec<&str> = text.lines().collect();
    let wanted: Vec<&str> = old_text.lines().map(str::trim).collect();
    let Some(first) = wanted.iter().find(|line| !line.is_empty()) else {
        return String::new();
    };
    let whole = (0..file.len().saturating_sub(wanted.len() - 1)).find(|&start| {
        wanted
            .iter()
            .enumerate()
            .all(|(offset, line)| file[start + offset].trim() == *line)
    });
    let (start, why) = match whole {
        Some(start) => (start, "it matches if whitespace at line ends is ignored"),
        None => match file.iter().position(|line| line.trim() == *first) {
            Some(start) => (start, "its first line is here"),
            None => return String::new(),
        },
    };
    let end = (start + wanted.len().max(1))
        .min(file.len())
        .min(start + NEAREST_LINES);
    let shown: Vec<String> = (start..end)
        .map(|index| format!("{:>6}\t{}", index + 1, file[index]))
        .collect();
    format!(
        ". Did you mean lines {}-{}? {why}:\n{}",
        start + 1,
        end,
        shown.join("\n")
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArguments {
    path: String,
    expected_sha256: Option<String>,
    old_text: Option<String>,
    new_text: Option<String>,
    #[serde(default)]
    edits: Vec<Edit>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    old_text: String,
    new_text: String,
}

impl PatchArguments {
    /// The replacements, from either `old_text` and `new_text` or `edits`.
    fn edits(&self) -> Result<Vec<Edit>, ToolError> {
        match (&self.old_text, &self.new_text, self.edits.is_empty()) {
            (Some(old_text), Some(new_text), true) => Ok(vec![Edit {
                old_text: old_text.clone(),
                new_text: new_text.clone(),
            }]),
            (None, None, false) => Ok(self.edits.clone()),
            (None, None, true) => Err(ToolError::new("give old_text and new_text, or edits")),
            _ => Err(ToolError::new(
                "give either old_text and new_text, or edits, not both or half of one",
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArguments {
    path: String,
    content: String,
    expected_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
struct WriteOutput {
    path: String,
    created: bool,
    sha256: String,
    bytes_written: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Debug, Serialize)]
struct PatchOutput {
    path: String,
    before_sha256: String,
    after_sha256: String,
    bytes_written: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use std::path::PathBuf;

    use dino_protocol::ToolCall;
    use serde_json::json;

    use super::*;

    static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestWorkspace {
        path: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let serial = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("dino-tools-test-{}-{serial}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn write(&self, path: &str, contents: &str) {
            let path = self.path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }

        fn tools(&self) -> WorkspaceTools {
            WorkspaceTools::new(&self.path, ToolLimits::default()).unwrap()
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).unwrap();
        }
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: name.into(),
            arguments,
        }
    }

    fn output_json(output: &ToolOutput) -> Value {
        assert!(!output.is_error);
        serde_json::from_str(&output.content).unwrap()
    }

    #[test]
    fn list_reports_files_directories_and_sizes_respecting_ignore_files() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\ntwo\n");
        workspace.write("src/parser.rs", "three\n");
        workspace.write("target/debug/artifact", "build output\n");
        workspace.write(".gitignore", "target\n");
        let tools = workspace.tools();

        let output = tools.execute_sync(&call("list", json!({}))).unwrap();
        let output = output_json(&output);

        let paths: Vec<&str> = output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["src", "src/lib.rs", "src/parser.rs"]);
        assert_eq!(output["entries"][0]["kind"], "directory");
        assert_eq!(output["entries"][1]["kind"], "file");
        assert_eq!(output["entries"][1]["bytes"], 8);
        assert_eq!(output["total_entries"], 3);
        assert_eq!(output["truncated"], false);
        // A hidden file and ignored build output are both absent, which is the
        // whole reason this is not `exec ls`.
        assert!(!paths.contains(&"target"), "{paths:?}");
        assert!(!paths.contains(&".gitignore"), "{paths:?}");
    }

    #[test]
    fn list_shows_ignored_and_hidden_entries_only_when_asked() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\n");
        workspace.write("target/debug/artifact", "build output\n");
        workspace.write(".gitignore", "target\n");
        let tools = workspace.tools();

        let ordinary = output_json(&tools.execute_sync(&call("list", json!({}))).unwrap());
        let everything = output_json(
            &tools
                .execute_sync(&call("list", json!({"all": true})))
                .unwrap(),
        );

        let names = |output: &Value| {
            output["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["path"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert!(
            !names(&ordinary)
                .iter()
                .any(|name| name.starts_with("target"))
        );
        assert!(
            names(&everything)
                .iter()
                .any(|name| name.starts_with("target"))
        );
        assert!(names(&everything).contains(&".gitignore".to_owned()));
        // The flag is reported, so an absence in an ordinary listing can be read
        // as "not shown" rather than "not there".
        assert_eq!(ordinary["all"], false);
        assert_eq!(everything["all"], true);
    }

    #[test]
    fn list_can_be_narrowed_by_path_and_depth() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\n");
        workspace.write("src/deep/nested.rs", "two\n");
        let tools = workspace.tools();

        let shallow = output_json(
            &tools
                .execute_sync(&call("list", json!({"path": "src", "depth": 1})))
                .unwrap(),
        );
        let deep = output_json(
            &tools
                .execute_sync(&call("list", json!({"path": "src"})))
                .unwrap(),
        );

        assert_eq!(shallow["total_entries"], 2);
        assert_eq!(deep["total_entries"], 3);
    }

    #[test]
    fn list_says_how_many_entries_it_did_not_return() {
        let workspace = TestWorkspace::new();
        for index in 0..8 {
            workspace.write(&format!("src/file{index}.rs"), "x\n");
        }
        let limits = ToolLimits {
            max_list_entries: 3,
            ..ToolLimits::default()
        };
        let tools = WorkspaceTools::new(&workspace.path, limits).unwrap();

        let output = output_json(&tools.execute_sync(&call("list", json!({}))).unwrap());

        assert_eq!(output["entries"].as_array().unwrap().len(), 3);
        assert_eq!(output["total_entries"], 9);
        assert_eq!(output["truncated"], true);
    }

    #[test]
    fn list_cannot_escape_the_workspace() {
        let workspace = TestWorkspace::new();
        let tools = workspace.tools();

        let error = tools
            .execute_sync(&call("list", json!({"path": ".."})))
            .unwrap_err();

        assert!(error.to_string().contains("remain inside the workspace"));
    }

    #[tokio::test]
    async fn an_empty_list_path_through_a_tool_set_lists_the_workspace_root() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\n");
        let tools = dino_core::ToolSet::new(vec![Box::new(workspace.tools())]).unwrap();

        let output = tools
            .execute(&call("list", json!({"path": "", "pattern": ""})))
            .await
            .unwrap();

        assert_eq!(output_json(&output)["path"], ".");
    }

    #[test]
    fn classification_names_read_and_search_as_read_only() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\n");
        let tools = workspace.tools();

        let read = tools
            .classify(&call("read", json!({"path": "src/lib.rs"})))
            .unwrap();
        let search = tools
            .classify(&call("search", json!({"query": "one"})))
            .unwrap();

        assert_eq!(read, ToolAction::read_only());
        assert_eq!(search, ToolAction::read_only());
    }

    #[cfg(unix)]
    #[test]
    fn the_classified_subject_is_the_path_that_would_be_written() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\n");
        symlink(
            workspace.path.join("src/lib.rs"),
            workspace.path.join("alias.rs"),
        )
        .unwrap();
        let tools = workspace.tools();
        let patch = |path: &str| {
            call(
                "apply_patch",
                json!({
                    "path": path,
                    "expected_sha256": "0".repeat(64),
                    "old_text": "one",
                    "new_text": "two",
                }),
            )
        };

        let direct = tools.classify(&patch("./src/lib.rs")).unwrap();
        let through_link = tools.classify(&patch("alias.rs")).unwrap();

        // Both spellings reach one file, so both are approved as that file. An
        // approval granted for src/lib.rs cannot be dodged by asking again
        // under a different name for the same bytes.
        assert_eq!(direct, ToolAction::mutating("src/lib.rs"));
        assert_eq!(through_link, ToolAction::mutating("src/lib.rs"));
    }

    #[test]
    fn classification_rejects_parent_traversal_before_any_decision() {
        let workspace = TestWorkspace::new();
        let tools = workspace.tools();
        let arguments = json!({
            "path": "../outside",
            "expected_sha256": "0".repeat(64),
            "old_text": "one",
            "new_text": "two",
        });

        let error = tools.classify(&call("apply_patch", arguments)).unwrap_err();

        assert!(error.to_string().contains("remain inside the workspace"));
    }

    #[cfg(unix)]
    #[test]
    fn classification_rejects_a_symlink_escape_before_any_decision() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        let outside =
            std::env::temp_dir().join(format!("dino-classify-outside-{}", std::process::id()));
        fs::write(&outside, "secret").unwrap();
        symlink(&outside, workspace.path.join("escape")).unwrap();
        let tools = workspace.tools();
        let arguments = json!({
            "path": "escape",
            "expected_sha256": "0".repeat(64),
            "old_text": "secret",
            "new_text": "published",
        });

        let error = tools.classify(&call("apply_patch", arguments)).unwrap_err();

        fs::remove_file(&outside).unwrap();
        assert!(error.to_string().contains("escapes the workspace"));
    }

    #[test]
    fn an_unknown_tool_cannot_be_classified() {
        let workspace = TestWorkspace::new();
        let tools = workspace.tools();

        let error = tools
            .classify(&call("exec", json!({"command": "rm -rf /"})))
            .unwrap_err();

        assert!(error.to_string().contains("unknown tool"));
    }

    #[test]
    fn read_is_numbered_bounded_and_fingerprinted() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\ntwo\nthree\n");
        let tools = workspace.tools();

        let output = tools
            .execute_sync(&call(
                "read",
                json!({"path": "src/lib.rs", "start_line": 2, "end_line": 3}),
            ))
            .unwrap();
        let output = output_json(&output);

        assert_eq!(output["content"], "     2\ttwo\n     3\tthree\n");
        assert_eq!(output["total_lines"], 3);
        assert_eq!(output["truncated"], false);
        assert_eq!(output["sha256"].as_str().unwrap().len(), 64);
        assert!(output.get("repeated").is_none());
    }

    #[test]
    fn a_repeated_read_of_an_unchanged_range_returns_a_note() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\ntwo\n");
        workspace.write("src/other.rs", "one\ntwo\n");
        let tools = workspace.tools();
        let read =
            |arguments: Value| output_json(&tools.execute_sync(&call("read", arguments)).unwrap());

        let first = read(json!({"path": "src/lib.rs"}));
        // Same resolved range, different spelling of the path.
        let second = read(json!({"path": "./src/lib.rs", "end_line": 2}));
        let forced = read(json!({"path": "src/lib.rs", "again": true}));
        let other_range = read(json!({"path": "src/lib.rs", "start_line": 2, "end_line": 2}));
        let other_file = read(json!({"path": "src/other.rs"}));

        assert_eq!(second["repeated"], true);
        assert_eq!(second["sha256"], first["sha256"]);
        assert!(
            second["content"]
                .as_str()
                .unwrap()
                .contains("Pass again to read it once more"),
            "{}",
            second["content"]
        );
        assert!(!second["content"].as_str().unwrap().contains("one"));
        assert_eq!(forced["content"], first["content"]);
        assert!(forced.get("repeated").is_none());
        assert_eq!(other_range["content"], "     2\ttwo\n");
        assert!(other_range.get("repeated").is_none());
        assert!(other_file["content"].as_str().unwrap().contains("one"));
        assert!(other_file.get("repeated").is_none());

        workspace.write("src/lib.rs", "one\ntwo\nthree\n");
        let changed = read(json!({"path": "src/lib.rs"}));
        assert!(changed.get("repeated").is_none(), "{changed}");
        assert!(changed["content"].as_str().unwrap().contains("three"));
    }

    #[test]
    fn a_lost_history_returns_the_bytes_again() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\ntwo\n");
        let tools = workspace.tools();
        let read = || {
            output_json(
                &tools
                    .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
                    .unwrap(),
            )
        };
        read();
        assert_eq!(read()["repeated"], true);

        // A new conversation, or elided results: the model no longer has them.
        tools.history_lost();

        let after = read();
        assert!(after.get("repeated").is_none(), "{after}");
        assert!(after["content"].as_str().unwrap().contains("two"));
    }

    #[test]
    fn a_stall_is_the_resolved_range_not_the_arguments() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\ntwo\nthree\nfour\n");
        let tools = WorkspaceTools::new(
            &workspace.path,
            ToolLimits {
                max_read_lines: 2,
                ..ToolLimits::default()
            },
        )
        .unwrap();
        let read =
            |arguments: Value| output_json(&tools.execute_sync(&call("read", arguments)).unwrap());

        let first = read(json!({"path": "src/lib.rs"}));
        let same_window = read(json!({"path": "src/lib.rs", "end_line": 2}));
        let next_window = read(json!({"path": "src/lib.rs", "start_line": 3}));

        assert_eq!(first["end_line"], 2);
        assert_eq!(same_window["repeated"], true);
        assert!(next_window.get("repeated").is_none(), "{next_window}");
        assert!(
            next_window["content"].as_str().unwrap().contains("three"),
            "{next_window}"
        );
    }

    #[test]
    fn a_stalled_read_still_counts_as_having_seen_the_file() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "fn a() {}\n");
        let tools = workspace.tools();
        tools
            .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
            .unwrap();
        let second = output_json(
            &tools
                .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
                .unwrap(),
        );
        assert_eq!(second["repeated"], true);

        tools
            .execute_sync(&call(
                "apply_patch",
                json!({"path": "src/lib.rs", "old_text": "a()", "new_text": "b()"}),
            ))
            .unwrap();

        assert_eq!(
            fs::read_to_string(workspace.path.join("src/lib.rs")).unwrap(),
            "fn b() {}\n"
        );
    }

    #[test]
    fn several_edits_apply_in_one_call() {
        let workspace = TestWorkspace::new();
        workspace.write(
            "src/lib.rs",
            "fn a() -> u32 {\n    1\n}\nfn b() -> u32 {\n    2\n}\n",
        );
        let tools = workspace.tools();
        tools
            .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
            .unwrap();

        tools
            .execute_sync(&call(
                "apply_patch",
                json!({
                    "path": "src/lib.rs",
                    "edits": [
                        {"old_text": "    2\n", "new_text": "    20\n"},
                        {"old_text": "    1\n", "new_text": "    10\n"}
                    ]
                }),
            ))
            .unwrap();

        assert_eq!(
            fs::read_to_string(workspace.path.join("src/lib.rs")).unwrap(),
            "fn a() -> u32 {\n    10\n}\nfn b() -> u32 {\n    20\n}\n"
        );
    }

    #[test]
    fn one_missing_edit_refuses_them_all_and_names_it() {
        let workspace = TestWorkspace::new();
        let original = "fn a() -> u32 {\n    1\n}\n";
        workspace.write("src/lib.rs", original);
        let tools = workspace.tools();
        tools
            .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
            .unwrap();

        let error = tools
            .execute_sync(&call(
                "apply_patch",
                json!({
                    "path": "src/lib.rs",
                    "edits": [
                        {"old_text": "    1\n", "new_text": "    10\n"},
                        {"old_text": "fn a() -> u32 {\n  1", "new_text": "x"}
                    ]
                }),
            ))
            .unwrap_err()
            .to_string();

        assert!(error.starts_with("edit 2: "), "{error}");
        assert!(error.contains("Did you mean lines 1-2?"), "{error}");
        assert!(error.contains("     2\t    1"), "{error}");
        assert_eq!(
            fs::read_to_string(workspace.path.join("src/lib.rs")).unwrap(),
            original
        );
    }

    #[test]
    fn overlapping_edits_and_mixed_forms_are_refused() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "abcdef\n");
        let tools = workspace.tools();
        tools
            .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
            .unwrap();

        let overlap = tools
            .execute_sync(&call(
                "apply_patch",
                json!({
                    "path": "src/lib.rs",
                    "edits": [
                        {"old_text": "abcd", "new_text": "x"},
                        {"old_text": "cdef", "new_text": "y"}
                    ]
                }),
            ))
            .unwrap_err();
        assert!(overlap.to_string().contains("overlap"), "{overlap}");

        let mixed = tools.classify(&call(
            "apply_patch",
            json!({
                "path": "src/lib.rs",
                "old_text": "a",
                "new_text": "b",
                "edits": [{"old_text": "c", "new_text": "d"}]
            }),
        ));
        assert!(mixed.is_err());
    }

    #[test]
    fn a_patch_that_leaves_the_file_unchanged_is_an_error() {
        let workspace = TestWorkspace::new();
        let original = "pub fn fare_cents() -> u32 {\n    100\n}\n";
        workspace.write("src/lib.rs", original);
        let tools = workspace.tools();
        tools
            .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
            .unwrap();

        let error = tools
            .execute_sync(&call(
                "apply_patch",
                json!({
                    "path": "src/lib.rs",
                    "old_text": "100",
                    "new_text": "100"
                }),
            ))
            .unwrap_err();

        assert!(
            error.to_string().contains("would not change the file"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(workspace.path.join("src/lib.rs")).unwrap(),
            original
        );
    }

    #[test]
    fn parent_traversal_is_rejected() {
        let workspace = TestWorkspace::new();
        let tools = workspace.tools();

        let error = tools
            .execute_sync(&call("read", json!({"path": "../outside"})))
            .unwrap_err();

        assert!(error.to_string().contains("remain inside the workspace"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new();
        let outside =
            std::env::temp_dir().join(format!("dino-tools-outside-{}", std::process::id()));
        fs::write(&outside, "secret").unwrap();
        symlink(&outside, workspace.path.join("escape")).unwrap();
        let tools = workspace.tools();

        let error = tools
            .execute_sync(&call("read", json!({"path": "escape"})))
            .unwrap_err();

        assert!(error.to_string().contains("escapes the workspace"));
        fs::remove_file(outside).unwrap();
    }

    #[test]
    fn search_is_literal_bounded_and_gitignore_aware() {
        let workspace = TestWorkspace::new();
        workspace.write(".gitignore", "ignored.txt\n");
        workspace.write("src/lib.rs", "needle here\nneedle again\n");
        workspace.write("ignored.txt", "needle hidden\n");
        let tools = workspace.tools();

        let output = tools
            .execute_sync(&call(
                "search",
                json!({"query": "needle", "max_results": 1}),
            ))
            .unwrap();
        let output = output_json(&output);

        assert_eq!(output["total_matches"], 2);
        assert_eq!(output["matches"].as_array().unwrap().len(), 1);
        assert_eq!(output["matches"][0]["path"], "src/lib.rs");
        assert_eq!(output["truncated"], true);
    }

    #[test]
    fn search_takes_a_regex_a_case_and_a_glob_when_asked() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "fn parse_one() {}\nfn Parse_Two() {}\n");
        workspace.write("src/deep/mod.rs", "fn parse_three() {}\n");
        workspace.write("README.md", "fn parse_in_prose\n");
        let tools = workspace.tools();
        let search = |arguments: Value| {
            output_json(&tools.execute_sync(&call("search", arguments)).unwrap())
        };

        // Literal by default: the dot and brackets are just characters.
        assert_eq!(search(json!({"query": "parse_.*"}))["total_matches"], 0);
        assert_eq!(
            search(json!({"query": r"fn parse_\w+", "regex": true}))["total_matches"],
            3
        );
        assert_eq!(
            search(json!({"query": "parse_", "ignore_case": true, "glob": "*.rs"}))["total_matches"],
            3
        );
        let top_only = search(json!({"query": "parse", "glob": "src/*.rs", "ignore_case": true}));
        assert_eq!(top_only["total_matches"], 2, "{top_only}");
        assert!(
            tools
                .execute_sync(&call("search", json!({"query": "(", "regex": true})))
                .unwrap_err()
                .to_string()
                .contains("invalid search pattern")
        );
    }

    #[test]
    fn list_finds_files_by_pattern_at_any_depth() {
        let workspace = TestWorkspace::new();
        workspace.write("a_test.go", "");
        workspace.write("pkg/b_test.go", "");
        workspace.write("pkg/b.go", "");
        let tools = workspace.tools();

        let output = output_json(
            &tools
                .execute_sync(&call("list", json!({"pattern": "*_test.go"})))
                .unwrap(),
        );
        let paths: Vec<&str> = output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["path"].as_str())
            .collect();
        assert_eq!(paths, ["a_test.go", "pkg/b_test.go"]);
    }

    #[test]
    fn search_may_be_narrowed_to_one_file() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "needle here\n");
        workspace.write("src/main.rs", "needle there\n");
        let tools = workspace.tools();

        let output = tools
            .execute_sync(&call(
                "search",
                json!({"query": "needle", "path": "src/lib.rs"}),
            ))
            .unwrap();
        let output = output_json(&output);

        assert_eq!(output["total_matches"], 1);
        assert_eq!(output["matches"][0]["path"], "src/lib.rs");
        assert!(
            tools
                .execute_sync(&call(
                    "search",
                    json!({"query": "needle", "path": "src/gone.rs"})
                ))
                .is_err()
        );
    }

    #[test]
    fn exact_patch_requires_current_fingerprint_and_one_match() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "fn old() {}\n");
        let tools = workspace.tools();
        let read_output = tools
            .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
            .unwrap();
        let read = output_json(&read_output);
        let fingerprint = read["sha256"].as_str().unwrap();

        let patched = tools
            .execute_sync(&call(
                "apply_patch",
                json!({
                    "path": "src/lib.rs",
                    "expected_sha256": fingerprint,
                    "old_text": "old",
                    "new_text": "new"
                }),
            ))
            .unwrap();
        let patched = output_json(&patched);

        assert_eq!(
            fs::read_to_string(workspace.path.join("src/lib.rs")).unwrap(),
            "fn new() {}\n"
        );
        assert_ne!(patched["before_sha256"], patched["after_sha256"]);

        // Built on the hash from before its own last edit, as a model issuing
        // two patches in one turn does. The only change since was its own.
        let chained = tools
            .execute_sync(&call(
                "apply_patch",
                json!({
                    "path": "src/lib.rs",
                    "expected_sha256": fingerprint,
                    "old_text": "new",
                    "new_text": "newer"
                }),
            ))
            .unwrap();
        assert!(!chained.is_error);
        assert_eq!(
            fs::read_to_string(workspace.path.join("src/lib.rs")).unwrap(),
            "fn newer() {}\n"
        );
    }

    #[test]
    fn a_change_the_model_did_not_make_makes_the_file_stale() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "fn old() {}\n");
        let tools = workspace.tools();
        let read = output_json(
            &tools
                .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
                .unwrap(),
        );
        let fingerprint = read["sha256"].as_str().unwrap().to_owned();
        // A formatter, a build script or a person, between the read and the patch.
        workspace.write("src/lib.rs", "fn old() {}\nfn added() {}\n");

        for hash in [Some(fingerprint.as_str()), None] {
            let mut arguments = json!({"path": "src/lib.rs", "old_text": "old", "new_text": "new"});
            if let Some(hash) = hash {
                arguments["expected_sha256"] = json!(hash);
            }
            let stale = tools
                .execute_sync(&call("apply_patch", arguments))
                .unwrap_err();
            assert!(stale.to_string().contains("stale file"), "{stale}");
        }
        assert_eq!(
            fs::read_to_string(workspace.path.join("src/lib.rs")).unwrap(),
            "fn old() {}\nfn added() {}\n"
        );
    }

    #[test]
    fn a_mis_copied_or_missing_hash_is_forgiven_for_a_file_seen_as_it_is() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "fn a() {}\nfn b() {}\n");
        let tools = workspace.tools();
        tools
            .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
            .unwrap();

        tools
            .execute_sync(&call(
                "apply_patch",
                json!({"path": "src/lib.rs", "expected_sha256": "b4ca2f645931f9a2e94473e5e54a8f21a11b8f2a3c5d6e7f8a9b0c1d2e3f4a5b", "old_text": "a()", "new_text": "first()"}),
            ))
            .unwrap();
        tools
            .execute_sync(&call(
                "apply_patch",
                json!({"path": "src/lib.rs", "old_text": "b()", "new_text": "second()"}),
            ))
            .unwrap();

        assert_eq!(
            fs::read_to_string(workspace.path.join("src/lib.rs")).unwrap(),
            "fn first() {}\nfn second() {}\n"
        );
    }

    #[test]
    fn a_file_never_read_needs_its_hash() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "fn a() {}\n");
        let tools = workspace.tools();

        let unread = tools
            .execute_sync(&call(
                "apply_patch",
                json!({"path": "src/lib.rs", "old_text": "a()", "new_text": "b()"}),
            ))
            .unwrap_err();
        assert!(unread.to_string().contains("read"), "{unread}");

        let prefix = &fingerprint(b"fn a() {}\n")[..12];
        tools
            .execute_sync(&call(
                "apply_patch",
                json!({"path": "src/lib.rs", "expected_sha256": prefix, "old_text": "a()", "new_text": "b()"}),
            ))
            .unwrap();
    }

    #[test]
    fn exact_patch_rejects_ambiguous_matches_without_writing() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "same\nsame\n");
        let tools = workspace.tools();
        let original = fs::read(workspace.path.join("src/lib.rs")).unwrap();
        let expected_sha256 = fingerprint(&original);

        let error = tools
            .execute_sync(&call(
                "apply_patch",
                json!({
                    "path": "src/lib.rs",
                    "expected_sha256": expected_sha256,
                    "old_text": "same",
                    "new_text": "different"
                }),
            ))
            .unwrap_err();

        assert!(error.to_string().contains("found 2 matches"));
        assert_eq!(
            fs::read(workspace.path.join("src/lib.rs")).unwrap(),
            original
        );
    }

    #[test]
    fn write_file_creates_a_file_and_the_directories_above_it() {
        let workspace = TestWorkspace::new();
        let tools = workspace.tools();

        let output = tools
            .execute_sync(&call(
                "write_file",
                json!({"path": "tests/properties.rs", "content": "#[test]\nfn holds() {}\n"}),
            ))
            .unwrap();
        let output = output_json(&output);

        assert_eq!(output["created"], true);
        assert_eq!(
            output["sha256"],
            fingerprint(b"#[test]\nfn holds() {}\n").as_str()
        );
        assert_eq!(
            fs::read_to_string(workspace.path.join("tests/properties.rs")).unwrap(),
            "#[test]\nfn holds() {}\n"
        );
    }

    #[test]
    fn write_file_overwrites_only_with_the_current_fingerprint() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "old\n");
        let tools = workspace.tools();

        let unasked = tools
            .execute_sync(&call(
                "write_file",
                json!({"path": "src/lib.rs", "content": "new\n"}),
            ))
            .unwrap_err();
        assert!(unasked.to_string().contains("already exists"), "{unasked}");

        let stale = tools
            .execute_sync(&call(
                "write_file",
                json!({"path": "src/lib.rs", "content": "new\n", "expected_sha256": fingerprint(b"other")}),
            ))
            .unwrap_err();
        assert!(stale.to_string().contains("stale file"), "{stale}");
        assert_eq!(
            fs::read_to_string(workspace.path.join("src/lib.rs")).unwrap(),
            "old\n"
        );

        let output = tools
            .execute_sync(&call(
                "write_file",
                json!({"path": "src/lib.rs", "content": "new\n", "expected_sha256": fingerprint(b"old\n")}),
            ))
            .unwrap();
        assert_eq!(output_json(&output)["created"], false);
        assert_eq!(
            fs::read_to_string(workspace.path.join("src/lib.rs")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn write_file_cannot_leave_the_workspace() {
        let workspace = TestWorkspace::new();
        let outside = TestWorkspace::new();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside.path, workspace.path.join("escape")).unwrap();
        let tools = workspace.tools();

        for path in ["../sibling.rs", "/tmp/absolute.rs", "escape/planted.rs"] {
            let error = tools
                .classify_sync(&call("write_file", json!({"path": path, "content": "x"})))
                .unwrap_err();
            assert!(
                error.to_string().contains("workspace"),
                "{path} said: {error}"
            );
        }
        assert!(!outside.path.join("planted.rs").exists());
    }

    #[test]
    fn an_absolute_path_inside_the_workspace_is_read_as_relative() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "one\n");
        let tools = workspace.tools();
        let canonical = fs::canonicalize(&workspace.path).unwrap();

        for root in [&workspace.path, &canonical] {
            let file = root.join("src/lib.rs").display().to_string();
            let read = output_json(
                &tools
                    .execute_sync(&call("read", json!({"path": file})))
                    .unwrap(),
            );
            assert_eq!(read["path"], "src/lib.rs", "{file}");

            let listed = tools
                .execute_sync(&call("list", json!({"path": root.display().to_string()})))
                .unwrap();
            assert!(listed.content.contains("lib.rs"), "{}", listed.content);
        }
    }

    #[test]
    fn an_absolute_path_that_only_shares_the_root_prefix_is_refused() {
        let workspace = TestWorkspace::new();
        let tools = workspace.tools();
        let sibling = format!("{}-sibling/x.rs", workspace.path.display());
        let climbing = workspace.path.join("../x.rs").display().to_string();

        for path in [sibling, climbing] {
            let error = tools
                .classify_sync(&call("write_file", json!({"path": path, "content": "x"})))
                .unwrap_err();
            assert!(
                error.to_string().contains("workspace"),
                "{path} said: {error}"
            );
        }
    }

    #[test]
    fn write_file_is_classified_as_mutating_its_resolved_path() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "");
        let tools = workspace.tools();

        let action = tools
            .classify_sync(&call(
                "write_file",
                json!({"path": "./src/new.rs", "content": ""}),
            ))
            .unwrap();

        assert_eq!(action, ToolAction::mutating("src/new.rs"));
    }

    #[test]
    fn writing_package_manager_config_is_called_out_for_the_final_answer() {
        let workspace = TestWorkspace::new();
        let tools = workspace.tools();

        let written = tools
            .execute_sync(&call(
                "write_file",
                json!({"path": ".npmrc", "content": "registry=https://registry.npmmirror.com\n"}),
            ))
            .unwrap();
        let written: Value = serde_json::from_str(&written.content).unwrap();
        let note = written["note"].as_str().expect("a note on .npmrc");
        assert!(note.contains("where packages come from"), "{note}");

        let ordinary = tools
            .execute_sync(&call(
                "write_file",
                json!({"path": "src/lib.rs", "content": "\n"}),
            ))
            .unwrap();
        let ordinary: Value = serde_json::from_str(&ordinary.content).unwrap();
        assert!(ordinary.get("note").is_none());
    }

    #[test]
    fn a_protected_path_cannot_be_patched_or_written() {
        let workspace = TestWorkspace::new();
        workspace.write("tests/rounding.rs", "#[test]\nfn t() {}\n");
        workspace.write("src/lib.rs", "fn a() {}\n");
        let tools = workspace.tools().protecting(vec!["tests".into()]);
        let sha = fingerprint(b"#[test]\nfn t() {}\n");

        for call_ in [
            call(
                "apply_patch",
                json!({"path": "tests/rounding.rs", "expected_sha256": sha, "old_text": "t()", "new_text": "u()"}),
            ),
            call(
                "write_file",
                json!({"path": "tests/new.rs", "content": "", }),
            ),
            call(
                "write_file",
                json!({"path": "./tests/../tests/rounding.rs", "content": ""}),
            ),
        ] {
            let error = tools.classify_sync(&call_).unwrap_err();
            assert!(
                error.to_string().contains("protected") || error.to_string().contains("workspace"),
                "{error}"
            );
            assert!(tools.execute_sync(&call_).is_err());
        }
        assert_eq!(
            fs::read_to_string(workspace.path.join("tests/rounding.rs")).unwrap(),
            "#[test]\nfn t() {}\n"
        );
        assert!(!workspace.path.join("tests/new.rs").exists());

        // Everything else is as writable as before, including a name that only
        // begins like a protected one.
        tools
            .execute_sync(&call(
                "write_file",
                json!({"path": "tests_scratch.rs", "content": ""}),
            ))
            .unwrap();
    }

    #[test]
    fn an_edit_that_returns_a_file_to_an_earlier_version_says_so() {
        let workspace = TestWorkspace::new();
        workspace.write("src/lib.rs", "fn a() {}\n");
        let tools = workspace.tools();
        tools
            .execute_sync(&call("read", json!({"path": "src/lib.rs"})))
            .unwrap();
        let patch = |old: &str, new: &str| {
            output_json(
                &tools
                    .execute_sync(&call(
                        "apply_patch",
                        json!({"path": "src/lib.rs", "old_text": old, "new_text": new}),
                    ))
                    .unwrap(),
            )
        };

        let forward = patch("a()", "b()");
        assert!(forward.get("note").is_none(), "{forward}");
        let back = patch("b()", "a()");
        assert!(back["note"].as_str().unwrap().contains("earlier"), "{back}");
    }
}
