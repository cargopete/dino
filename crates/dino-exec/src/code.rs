//! The `code` tool: definitions, references, hover, implementations and
//! symbols from rust-analyzer.
//!
//! One server per workspace, started on the first call inside the sandbox and
//! killed with the tool. Before each question the file asked about, and every
//! source changed since the last, is sent to it as it now is on disk.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::workspace::WorkspaceRoot;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_sandbox::Sandbox;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::sync::Mutex;

use crate::exec::capped_head;
use crate::exec::json_output;
use crate::lsp;
use crate::rust::Toolchain;

const NAME: &str = "code";
const PROGRAM: &str = "rust-analyzer";
/// Longest line of context a location repeats, in characters.
const MAX_CONTEXT_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeLimits {
    pub max_results: usize,
    pub max_hover_bytes: usize,
    /// For one request once the server is up.
    pub timeout: Duration,
    /// How long a call waits for the server to finish indexing before it asks
    /// anyway.
    pub index_wait: Duration,
}

impl Default for CodeLimits {
    fn default() -> Self {
        Self {
            max_results: 100,
            max_hover_bytes: 8 * 1_024,
            timeout: Duration::from_secs(30),
            index_wait: Duration::from_secs(90),
        }
    }
}

#[derive(Debug, Default)]
struct Session {
    server: Option<lsp::Server>,
    /// Opened documents, with the version and text each was last sent as.
    open: HashMap<PathBuf, (i64, String)>,
    sources: HashMap<PathBuf, Stamp>,
}

type Stamp = (Option<SystemTime>, u64);

/// The workspace's Rust sources and manifests, each with when it last changed.
fn sources(root: &Path) -> HashMap<PathBuf, Stamp> {
    let mut found = HashMap::new();
    let walk = ignore::WalkBuilder::new(root)
        .follow_links(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != "target")
        .build();
    for entry in walk.flatten() {
        let path = entry.path();
        let wanted = path.extension().is_some_and(|extension| extension == "rs")
            || path.file_name().is_some_and(|name| name == "Cargo.toml");
        if let (true, Ok(metadata)) = (wanted, entry.metadata())
            && metadata.is_file()
        {
            found.insert(
                path.to_path_buf(),
                (metadata.modified().ok(), metadata.len()),
            );
        }
    }
    found
}

#[derive(Debug)]
pub struct CodeTool {
    root: WorkspaceRoot,
    command: (String, Vec<String>),
    sandbox: Arc<dyn Sandbox>,
    limits: CodeLimits,
    session: Mutex<Session>,
}

impl CodeTool {
    /// The tool for one workspace, when rust-analyzer can be run there.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the root cannot be canonicalised or is not a
    /// directory.
    pub fn new(
        root: impl AsRef<Path>,
        limits: CodeLimits,
        sandbox: Arc<dyn Sandbox>,
    ) -> Result<Option<Self>, ToolError> {
        let root = WorkspaceRoot::new(root)?;
        Ok(locate(root.path()).map(|command| Self {
            root,
            command,
            sandbox,
            limits,
            session: Mutex::default(),
        }))
    }

    /// Starts the server if need be and waits for it to index, with a note
    /// when it has not finished.
    async fn ready(&self, session: &mut Session) -> Result<Option<String>, ToolError> {
        if session
            .server
            .as_ref()
            .is_some_and(|server| server.status().exited)
        {
            *session = Session::default();
        }
        if session.server.is_none() {
            session.sources = sources(self.root.path());
            let (program, arguments) = &self.command;
            session.server = Some(
                lsp::Server::start(
                    program,
                    arguments,
                    self.root.path(),
                    self.sandbox.as_ref(),
                    self.initialization(),
                    self.limits.timeout,
                )
                .await?,
            );
        }
        let server = session
            .server
            .as_mut()
            .ok_or_else(|| ToolError::new("rust-analyzer did not start"))?;
        if server.settle(self.limits.index_wait).await {
            return Ok(None);
        }
        let status = server.status();
        let note = if status.exited {
            "rust-analyzer exited; the next call starts it again".to_owned()
        } else {
            format!(
                "rust-analyzer had not finished indexing after {} seconds{}, so this answer may \
                 be incomplete; asking again later may find more",
                self.limits.index_wait.as_secs(),
                status
                    .message
                    .map(|message| format!(" ({})", first_line(&message)))
                    .unwrap_or_default()
            )
        };
        Ok(Some(note))
    }

    async fn run(&self, request: Request) -> Result<ToolOutput, ToolError> {
        let mut session = self.session.lock().await;
        let note = self.ready(&mut session).await?;
        let Session {
            server: Some(server),
            open,
            sources: before,
        } = &mut *session
        else {
            return Err(ToolError::new("rust-analyzer did not start"));
        };
        let mut files = Files::default();
        let position = match &request.operation {
            Operation::Symbols => None,
            _ => Some(self.position(&request, &mut files)?),
        };
        catch_up(
            server,
            open,
            before,
            sources(self.root.path()),
            position.as_ref().map(|(path, _)| path.as_path()),
        )
        .await?;
        let target = position.as_ref().map(
            |(path, at)| json!({"textDocument": {"uri": lsp::uri(path)}, "position": at.wire}),
        );
        let at = position
            .as_ref()
            .map(|(path, at)| format!("{}:{}:{}", self.shown(path), at.line + 1, at.column + 1));
        let target = target.unwrap_or_default();
        let found = match request.operation {
            Operation::Hover => {
                let result = server.request("textDocument/hover", target).await?;
                let text = hover_text(&result["contents"]);
                return json_output(&HoverOutput {
                    operation: "hover",
                    at,
                    hover: (!text.is_empty())
                        .then(|| capped_head(&text, self.limits.max_hover_bytes)),
                    note,
                });
            }
            Operation::Symbols => {
                let query = request.query.clone().unwrap_or_default();
                let result = server
                    .request("workspace/symbol", json!({"query": query}))
                    .await?;
                symbols(&result)
            }
            Operation::Definition => {
                locations(&server.request("textDocument/definition", target).await?)
            }
            Operation::Implementations => locations(
                &server
                    .request("textDocument/implementation", target)
                    .await?,
            ),
            Operation::References => {
                let mut params = target;
                params["context"] = json!({"includeDeclaration": true});
                locations(&server.request("textDocument/references", params).await?)
            }
        };
        let total = found.len();
        let shown = found
            .into_iter()
            .take(self.limits.max_results)
            .map(|found| self.located(found, &mut files))
            .collect();
        json_output(&LocationsOutput {
            operation: request.operation.name(),
            at,
            query: request.query,
            total,
            locations: shown,
            truncated: total > self.limits.max_results,
            note,
        })
    }

    fn initialization(&self) -> Value {
        json!({
            "processId": std::process::id(),
            "rootUri": lsp::uri(self.root.path()),
            "workspaceFolders": [{"uri": lsp::uri(self.root.path()), "name": "workspace"}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "textDocument": {
                    "synchronization": {},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "definition": {"linkSupport": false},
                    "implementation": {"linkSupport": false},
                    "references": {}
                },
                "workspace": {"symbol": {}, "workspaceFolders": true},
                "experimental": {"serverStatusNotification": true}
            },
            "initializationOptions": {
                // Diagnostics are the cargo tool's; a check on every change would
                // contend with it for the build directory.
                "checkOnSave": false,
                "files": {"watcher": "server"},
                "workspace": {"symbol": {"search": {"kind": "all_symbols", "limit": 1_024}}}
            }
        })
    }

    /// The file and position a request names, found from its line and name.
    fn position(
        &self,
        request: &Request,
        files: &mut Files,
    ) -> Result<(PathBuf, Position), ToolError> {
        let given = request
            .path
            .as_deref()
            .ok_or_else(|| ToolError::new("this operation needs a path"))?;
        let path = self.root.resolve_file(given)?;
        let line = request
            .line
            .ok_or_else(|| ToolError::new("this operation needs a line"))?;
        let text = files
            .line(&path, line - 1)
            .ok_or_else(|| ToolError::new(format!("{given} has no line {line}")))?;
        let column = match (&request.name, request.column) {
            (Some(name), column) => find_name(&text, name, column.map(|column| column - 1))
                .ok_or_else(|| {
                    ToolError::new(format!(
                        "{name:?} is not on line {line} of {given}, which reads: {}",
                        text.trim()
                    ))
                })?,
            (None, Some(column)) => column - 1,
            (None, None) => return Err(ToolError::new("name the identifier on that line")),
        };
        Ok((
            path,
            Position {
                line: line - 1,
                column,
                wire: json!({"line": line - 1, "character": lsp::utf16_offset(&text, column)}),
            },
        ))
    }

    fn located(&self, found: Found, files: &mut Files) -> Located {
        let text = files.line(&found.path, found.line).unwrap_or_default();
        let column = lsp::char_column(&text, found.character);
        let context: String = text.trim().chars().take(MAX_CONTEXT_CHARS).collect();
        Located {
            path: self.shown(&found.path),
            line: found.line + 1,
            column: column + 1,
            text: context,
            name: found.name,
            kind: found.kind,
            container: found.container,
        }
    }

    /// Workspace paths relative to the root, others in full.
    fn shown(&self, path: &Path) -> String {
        self.root
            .relative(path)
            .unwrap_or_else(|_| path.display().to_string())
    }
}

/// How rust-analyzer is run for `root`: through rustup under the toolchain a
/// file pins, or the first on PATH that answers. The rustup proxy is on PATH
/// whether or not the component is installed, so being there is not enough.
fn locate(root: &Path) -> Option<(String, Vec<String>)> {
    let pinned = Toolchain::find(root)
        .filter(|toolchain| toolchain.rustup.is_some())
        .map(|toolchain| toolchain.wrap(PROGRAM, Vec::new()));
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(PROGRAM))
        .filter(|candidate| candidate.is_file())
        .map(|candidate| (candidate.display().to_string(), Vec::new()));
    pinned
        .into_iter()
        .chain(on_path)
        .find(|(program, arguments)| answers(program, arguments, root))
}

fn answers(program: &str, arguments: &[String], root: &Path) -> bool {
    let child = std::process::Command::new(program)
        .args(arguments)
        .arg("--version")
        .current_dir(root)
        // A missing pinned toolchain is not to be downloaded just to ask.
        .env("RUSTUP_AUTO_INSTALL", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Tells the server about every source that changed since the last call, and
/// sends `asked` and every open document as they now are. Its own watcher
/// reports an edit some time after it lands, which is after the question that
/// follows it.
async fn catch_up(
    server: &lsp::Server,
    open: &mut HashMap<PathBuf, (i64, String)>,
    before: &mut HashMap<PathBuf, Stamp>,
    now: HashMap<PathBuf, Stamp>,
    asked: Option<&Path>,
) -> Result<(), ToolError> {
    let (manifests, changed): (Vec<&PathBuf>, Vec<&PathBuf>) = now
        .iter()
        .filter(|(path, stamp)| before.get(*path) != Some(stamp))
        .map(|(path, _)| path)
        .partition(|path| path.extension().is_none_or(|extension| extension != "rs"));
    let mut paths: Vec<PathBuf> = open.keys().cloned().collect();
    paths.extend(changed.into_iter().cloned());
    paths.extend(asked.map(Path::to_path_buf));
    paths.sort();
    paths.dedup();
    let watched: Vec<Value> = manifests
        .into_iter()
        .map(|path| json!({"uri": lsp::uri(path), "type": 2}))
        .chain(
            before
                .keys()
                .filter(|path| !now.contains_key(*path))
                .map(|path| json!({"uri": lsp::uri(path), "type": 3})),
        )
        .collect();
    if !watched.is_empty() {
        server
            .notify(
                "workspace/didChangeWatchedFiles",
                json!({"changes": watched}),
            )
            .await?;
    }
    for path in paths {
        sync(server, open, &path).await?;
    }
    *before = now;
    Ok(())
}

/// Sends `path` as it is on disk, if the server has not got it that way.
async fn sync(
    server: &lsp::Server,
    open: &mut HashMap<PathBuf, (i64, String)>,
    path: &Path,
) -> Result<(), ToolError> {
    let uri = lsp::uri(path);
    let Ok(text) = std::fs::read_to_string(path) else {
        if open.remove(path).is_some() {
            server
                .notify(
                    "textDocument/didClose",
                    json!({"textDocument": {"uri": uri}}),
                )
                .await?;
        }
        return Ok(());
    };
    match open.get_mut(path) {
        Some((_, sent)) if *sent == text => {}
        Some((version, sent)) => {
            *version += 1;
            server
                .notify(
                    "textDocument/didChange",
                    json!({"textDocument": {"uri": uri, "version": *version},
                           "contentChanges": [{"text": text}]}),
                )
                .await?;
            *sent = text;
        }
        None => {
            server
                .notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": "rust",
                                            "version": 1, "text": text}}),
                )
                .await?;
            open.insert(path.to_path_buf(), (1, text));
        }
    }
    Ok(())
}

/// Files read once per call, for the lines locations point at.
#[derive(Default)]
struct Files(HashMap<PathBuf, Option<Vec<String>>>);

impl Files {
    fn line(&mut self, path: &Path, index: usize) -> Option<String> {
        self.0
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                std::fs::read_to_string(path)
                    .ok()
                    .map(|text| text.lines().map(str::to_owned).collect())
            })
            .as_ref()?
            .get(index)
            .cloned()
    }
}

#[derive(Debug)]
struct Position {
    line: usize,
    column: usize,
    wire: Value,
}

/// A location as the server gave it, lines and UTF-16 offsets from zero.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Found {
    path: PathBuf,
    line: usize,
    character: usize,
    name: Option<String>,
    kind: Option<&'static str>,
    container: Option<String>,
}

/// The locations in a definition, references or implementation result:
/// one `Location`, an array of them, or of `LocationLink`s.
fn locations(result: &Value) -> Vec<Found> {
    let items = match result {
        Value::Array(items) => items.clone(),
        Value::Null => Vec::new(),
        single => vec![single.clone()],
    };
    let mut found: Vec<Found> = items
        .iter()
        .filter_map(|item| {
            let uri = item
                .get("uri")
                .or_else(|| item.get("targetUri"))?
                .as_str()?;
            let range = item
                .get("range")
                .or_else(|| item.get("targetSelectionRange"))?;
            location(uri, range)
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

fn location(uri: &str, range: &Value) -> Option<Found> {
    let number = |key: &str| {
        range["start"][key]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
    };
    Some(Found {
        path: lsp::path_of(uri)?,
        line: number("line")?,
        character: number("character")?,
        name: None,
        kind: None,
        container: None,
    })
}

/// A `workspace/symbol` result, in the server's order of relevance.
fn symbols(result: &Value) -> Vec<Found> {
    result
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|symbol| {
            let place = &symbol["location"];
            let uri = place["uri"].as_str()?;
            let mut found = location(uri, &place["range"]).unwrap_or(Found {
                path: lsp::path_of(uri)?,
                line: 0,
                character: 0,
                name: None,
                kind: None,
                container: None,
            });
            found.name = symbol["name"].as_str().map(str::to_owned);
            found.kind = symbol["kind"].as_u64().map(kind_name);
            found.container = symbol["containerName"]
                .as_str()
                .filter(|name| !name.is_empty())
                .map(str::to_owned);
            Some(found)
        })
        .collect()
}

fn kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type parameter",
        _ => "symbol",
    }
}

/// A hover's text from any of the shapes the protocol allows for it.
fn hover_text(contents: &Value) -> String {
    match contents {
        Value::String(text) => text.trim().to_owned(),
        Value::Array(items) => items
            .iter()
            .map(hover_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(object) => match (object.get("language"), object.get("value")) {
            (Some(language), Some(value)) => format!(
                "```{}\n{}\n```",
                language.as_str().unwrap_or_default(),
                value.as_str().unwrap_or_default()
            ),
            (None, Some(value)) => value.as_str().unwrap_or_default().trim().to_owned(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// The character column of `name` on `line` as a whole identifier: the
/// occurrence covering `near` when given, else the first.
fn find_name(line: &str, name: &str, near: Option<usize>) -> Option<usize> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let starts: Vec<usize> = line
        .match_indices(name)
        .filter(|(at, _)| {
            let before = line[..*at].chars().next_back();
            let after = line[at + name.len()..].chars().next();
            !before.is_some_and(word) && !after.is_some_and(word)
        })
        .map(|(at, _)| line[..at].chars().count())
        .collect();
    let width = name.chars().count();
    near.and_then(|near| {
        starts
            .iter()
            .copied()
            .find(|start| (*start..start + width).contains(&near))
    })
    .or_else(|| starts.first().copied())
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Definition,
    References,
    Hover,
    Implementations,
    Symbols,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::References => "references",
            Self::Hover => "hover",
            Self::Implementations => "implementations",
            Self::Symbols => "symbols",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    operation: Operation,
    path: Option<String>,
    line: Option<usize>,
    name: Option<String>,
    column: Option<usize>,
    query: Option<String>,
}

impl Request {
    fn parse(arguments: &Value) -> Result<Self, ToolError> {
        let request: Self = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::new(format!("invalid {NAME} arguments: {error}")))?;
        let symbols = request.operation == Operation::Symbols;
        if request.line == Some(0) || request.column == Some(0) {
            return Err(ToolError::new("lines and columns count from 1"));
        }
        if request.name.as_deref() == Some("") {
            return Err(ToolError::new("name must not be empty"));
        }
        match (symbols, request.query.as_deref()) {
            (true, None | Some("")) => Err(ToolError::new("symbols needs a query")),
            (false, Some(_)) => Err(ToolError::new("query is for symbols")),
            (true, Some(_)) if request.path.is_some() || request.line.is_some() => Err(
                ToolError::new("symbols searches the workspace by query, not a position"),
            ),
            (false, None) if request.path.is_none() || request.line.is_none() => {
                Err(ToolError::new(format!(
                    "{} needs a path and a line",
                    request.operation.name()
                )))
            }
            (false, None) if request.name.is_none() && request.column.is_none() => {
                Err(ToolError::new("name the identifier on that line"))
            }
            _ => Ok(request),
        }
    }
}

#[derive(Debug, Serialize)]
struct LocationsOutput {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    total: usize,
    locations: Vec<Located>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Debug, Serialize)]
struct HoverOutput {
    operation: &'static str,
    at: Option<String>,
    hover: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Debug, Serialize)]
struct Located {
    path: String,
    line: usize,
    column: usize,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<String>,
}

impl ToolRuntime for CodeTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: NAME.into(),
            description: "Ask rust-analyzer about Rust code. definition: where the item named \
                          at a position is defined. references: every use of it, the \
                          declaration included. hover: its type, signature and docs. \
                          implementations: the impls of a trait or type, or of a trait method. \
                          symbols: items whose name matches query, across the workspace. A \
                          position is a path, a line and the identifier on that line as name. \
                          Unlike search it follows re-exports, macros and trait methods, and \
                          tells two items of one name apart. The first call starts \
                          rust-analyzer and waits for it to index the workspace, which can take \
                          a minute."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": ["definition", "references", "hover", "implementations", "symbols"]
                    },
                    "path": {"type": "string", "minLength": 1, "description": "Workspace path of the file, for all but symbols."},
                    "line": {"type": "integer", "minimum": 1, "description": "Line of the identifier, counting from 1."},
                    "name": {"type": "string", "minLength": 1, "description": "The identifier on that line, such as parse or Config."},
                    "column": {"type": "integer", "minimum": 1, "description": "Optional: the column, counting from 1, to pick one of several occurrences of name, or in place of it."},
                    "query": {"type": "string", "minLength": 1, "description": "For symbols: part of the name, matched loosely."}
                },
                "required": ["operation"],
                "additionalProperties": false
            }),
        }]
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if call.name != NAME {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        Request::parse(&call.arguments)?;
        Ok(ToolAction::read_only())
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let request = if call.name == NAME {
            Request::parse(&call.arguments)
        } else {
            Err(ToolError::new(format!("unknown tool {:?}", call.name)))
        };
        Box::pin(async move { self.run(request?).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;

    fn kinds(found: &[Found]) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for item in found {
            *counts.entry(item.kind.unwrap_or("?")).or_default() += 1;
        }
        counts
    }

    fn found(path: &str, line: usize, character: usize) -> Found {
        Found {
            path: PathBuf::from(path),
            line,
            character,
            name: None,
            kind: None,
            container: None,
        }
    }

    #[test]
    fn every_location_shape_reads_the_same_sorted_and_deduplicated() {
        let range = |line: u64, character: u64| {
            json!({"start": {"line": line, "character": character},
                   "end": {"line": line, "character": character + 1}})
        };
        let single = json!({"uri": "file:///w/src/a.rs", "range": range(4, 2)});
        assert_eq!(locations(&single), vec![found("/w/src/a.rs", 4, 2)]);
        let many = json!([
            {"uri": "file:///w/src/b.rs", "range": range(1, 0)},
            {"uri": "file:///w/src/a.rs", "range": range(9, 3)},
            {"uri": "file:///w/src/b.rs", "range": range(1, 0)}
        ]);
        assert_eq!(
            locations(&many),
            vec![found("/w/src/a.rs", 9, 3), found("/w/src/b.rs", 1, 0)]
        );
        let links = json!([{"targetUri": "file:///w/src/c.rs", "targetRange": range(0, 0),
                            "targetSelectionRange": range(2, 7)}]);
        assert_eq!(locations(&links), vec![found("/w/src/c.rs", 2, 7)]);
        assert_eq!(locations(&Value::Null), Vec::new());
    }

    #[test]
    fn symbols_keep_their_order_and_name_their_kind() {
        let result = json!([
            {"name": "Parser", "kind": 23, "containerName": "",
             "location": {"uri": "file:///w/src/p.rs", "range": {"start": {"line": 3, "character": 11}}}},
            {"name": "parse", "kind": 12, "containerName": "Parser",
             "location": {"uri": "file:///w/src/p.rs", "range": {"start": {"line": 9, "character": 8}}}},
            {"name": "parse_all", "kind": 12, "location": {"uri": "file:///w/src/q.rs"}}
        ]);
        let found = symbols(&result);
        assert_eq!(
            found.iter().map(|f| f.name.as_deref()).collect::<Vec<_>>(),
            [Some("Parser"), Some("parse"), Some("parse_all")]
        );
        assert_eq!(found[1].container.as_deref(), Some("Parser"));
        assert_eq!(found[0].container, None);
        assert_eq!((found[2].line, found[2].character), (0, 0));
        assert_eq!(
            kinds(&found),
            BTreeMap::from([("function", 2), ("struct", 1)])
        );
    }

    #[test]
    fn hover_text_comes_from_any_shape() {
        assert_eq!(
            hover_text(&json!({"kind": "markdown", "value": "```rust\nfn f()\n```\n"})),
            "```rust\nfn f()\n```"
        );
        assert_eq!(
            hover_text(&json!([{"language": "rust", "value": "fn f()"}, "docs", ""])),
            "```rust\nfn f()\n```\n\ndocs"
        );
        assert_eq!(hover_text(&Value::Null), "");
    }

    #[test]
    fn a_name_is_found_as_a_whole_identifier() {
        let line = "    let parser = parse(parse_all(x), parse);";
        assert_eq!(find_name(line, "parse", None), Some(17));
        assert_eq!(find_name(line, "parse", Some(38)), Some(37));
        assert_eq!(find_name(line, "parse", Some(0)), Some(17));
        assert_eq!(find_name(line, "pars", None), None);
        assert_eq!(find_name("é.len()", "len", None), Some(2));
    }

    #[test]
    fn requests_are_refused_before_anything_starts() {
        let refused = |arguments: Value| Request::parse(&arguments).err().map(|e| e.to_string());
        assert!(refused(json!({"operation": "symbols"})).is_some());
        assert!(refused(json!({"operation": "symbols", "query": "x", "line": 2})).is_some());
        assert!(refused(json!({"operation": "hover", "path": "a.rs"})).is_some());
        assert!(refused(json!({"operation": "hover", "path": "a.rs", "line": 2})).is_some());
        assert!(
            refused(json!({"operation": "hover", "path": "a.rs", "line": 0, "name": "x"}))
                .is_some()
        );
        assert!(
            refused(json!({"operation": "definition", "path": "a.rs", "line": 1, "query": "x"}))
                .is_some()
        );
        assert!(
            refused(json!({"operation": "references", "path": "a.rs", "line": 3, "name": "f"}))
                .is_none()
        );
        assert!(
            refused(json!({"operation": "definition", "path": "a.rs", "line": 3, "column": 5}))
                .is_none()
        );
        assert!(refused(json!({"operation": "symbols", "query": "Parser"})).is_none());
    }
}
