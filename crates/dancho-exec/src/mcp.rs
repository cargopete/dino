//! A client for local MCP servers over stdio.
//!
//! A server is a process the user's config names. It runs in the session's
//! sandbox unless that config says otherwise, gets only the environment
//! variables the config passes through, and its tools are offered as
//! `mcp__<server>__<tool>`, each call approved like a process.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use dancho_core::ToolError;
use dancho_core::ToolFuture;
use dancho_core::ToolRuntime;
use dancho_protocol::ToolAction;
use dancho_protocol::ToolCall;
use dancho_protocol::ToolDefinition;
use dancho_protocol::ToolOutput;
use dancho_sandbox::Sandbox;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::sync::Mutex;

const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_OUTPUT_BYTES: usize = 64 * 1_024;

/// One server, as the user's config describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub command: Vec<String>,
    /// Environment variables passed through from dancho's own environment.
    pub pass_env: Vec<String>,
    /// Tools that only read, which then need no approval.
    pub read_only_tools: Vec<String>,
    /// Run inside the session sandbox. On by default.
    pub sandboxed: bool,
}

struct Connection {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    // Held so the server lives exactly as long as the connection.
    _child: Child,
}

/// A connected server and the tools it offered.
pub struct McpTools {
    server: McpServer,
    tools: Vec<ToolDefinition>,
    connection: Mutex<Connection>,
    timeout: Duration,
}

impl std::fmt::Debug for McpTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpTools")
            .field("server", &self.server.name)
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

fn prefix(server: &str) -> String {
    format!("mcp__{server}__")
}

impl McpTools {
    /// Starts the server, completes the handshake and lists its tools.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the server cannot be started, does not
    /// answer the handshake, or answers it with an error.
    pub async fn connect(
        server: McpServer,
        workspace: &Path,
        sandbox: &Arc<dyn Sandbox>,
    ) -> Result<Self, ToolError> {
        let Some((program, arguments)) = server.command.split_first() else {
            return Err(ToolError::new(format!(
                "MCP server {} has an empty command",
                server.name
            )));
        };
        let (program, arguments) = if server.sandboxed {
            let wrapped = sandbox
                .wrap(program, arguments)
                .map_err(|error| ToolError::new(error.to_string()))?;
            (wrapped.program, wrapped.arguments)
        } else {
            (program.clone(), arguments.to_vec())
        };
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&arguments)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .env_clear();
        for (name, value) in environment(&server.pass_env) {
            command.env(name, value);
        }
        let mut child = command.spawn().map_err(|error| {
            ToolError::new(format!("cannot start MCP server {}: {error}", server.name))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::new("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::new("no stdout"))?;
        let tools = Self {
            connection: Mutex::new(Connection {
                stdin,
                stdout: BufReader::new(stdout),
                next_id: 1,
                _child: child,
            }),
            server,
            tools: Vec::new(),
            timeout: Duration::from_secs(120),
        };
        tools.handshake().await
    }

    async fn handshake(mut self) -> Result<Self, ToolError> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "dancho", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
        self.notify("notifications/initialized").await?;

        let mut cursor: Option<String> = None;
        loop {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
            let page = self.request("tools/list", params).await?;
            for tool in page["tools"].as_array().into_iter().flatten() {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                self.tools.push(ToolDefinition {
                    name: format!("{}{name}", prefix(&self.server.name)),
                    description: tool["description"].as_str().unwrap_or_default().to_owned(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                });
            }
            cursor = page["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(self)
    }

    /// The server's name and how many tools it offered, for the banner.
    #[must_use]
    pub fn summary(&self) -> String {
        let containment = if self.server.sandboxed {
            "sandboxed"
        } else {
            "unconfined"
        };
        format!(
            "{} ({} tools, {containment})",
            self.server.name,
            self.tools.len()
        )
    }

    async fn notify(&self, method: &str) -> Result<(), ToolError> {
        let mut connection = self.connection.lock().await;
        let line = json!({"jsonrpc": "2.0", "method": method}).to_string();
        write_line(&mut connection.stdin, &line).await
    }

    /// Sends one request and waits for its response, answering pings and
    /// skipping notifications the server sends in between.
    async fn request(&self, method: &str, params: Value) -> Result<Value, ToolError> {
        let mut connection = self.connection.lock().await;
        let id = connection.next_id;
        connection.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        write_line(&mut connection.stdin, &line.to_string()).await?;

        let exchange = async {
            loop {
                let mut line = String::new();
                let read = connection
                    .stdout
                    .read_line(&mut line)
                    .await
                    .map_err(|error| ToolError::new(format!("MCP read failed: {error}")))?;
                if read == 0 {
                    return Err(ToolError::new(format!(
                        "MCP server {} closed its output",
                        self.server.name
                    )));
                }
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message.get("method").is_some() {
                    // A request from the server. Only ping is answered; the
                    // client offers no capabilities beyond it.
                    if let Some(request_id) = message.get("id") {
                        let reply = if message["method"] == "ping" {
                            json!({"jsonrpc": "2.0", "id": request_id, "result": {}})
                        } else {
                            json!({"jsonrpc": "2.0", "id": request_id,
                                   "error": {"code": -32601, "message": "not supported"}})
                        };
                        write_line(&mut connection.stdin, &reply.to_string()).await?;
                    }
                    continue;
                }
                if message.get("id") != Some(&json!(id)) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    return Err(ToolError::new(format!(
                        "MCP server {} refused {method}: {}",
                        self.server.name,
                        error["message"].as_str().unwrap_or("unknown error")
                    )));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        tokio::time::timeout(self.timeout, exchange)
            .await
            .map_err(|_| {
                ToolError::new(format!(
                    "MCP server {} did not answer {method} within {} seconds",
                    self.server.name,
                    self.timeout.as_secs()
                ))
            })?
    }

    fn tool_name<'a>(&self, call: &'a ToolCall) -> Result<&'a str, ToolError> {
        call.name
            .strip_prefix(&prefix(&self.server.name))
            .filter(|name| self.tools.iter().any(|tool| tool.name == call.name) && !name.is_empty())
            .ok_or_else(|| ToolError::new(format!("unknown tool {:?}", call.name)))
    }
}

async fn write_line(stdin: &mut ChildStdin, line: &str) -> Result<(), ToolError> {
    let mut bytes = line.as_bytes().to_vec();
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .await
        .map_err(|error| ToolError::new(format!("MCP write failed: {error}")))?;
    stdin
        .flush()
        .await
        .map_err(|error| ToolError::new(format!("MCP write failed: {error}")))
}

/// The variables a server gets: enough to run, plus the ones the user named.
fn environment(pass: &[String]) -> BTreeMap<String, String> {
    ["PATH", "HOME", "USER", "LANG", "TMPDIR"]
        .iter()
        .map(|name| (*name).to_owned())
        .chain(pass.iter().cloned())
        .filter_map(|name| std::env::var(&name).ok().map(|value| (name, value)))
        .collect()
}

/// The text of a `tools/call` result. Non-text content is named, not dropped
/// silently.
fn render(result: &Value) -> String {
    let mut text = String::new();
    for item in result["content"].as_array().into_iter().flatten() {
        if !text.is_empty() {
            text.push('\n');
        }
        match item["type"].as_str() {
            Some("text") => text.push_str(item["text"].as_str().unwrap_or_default()),
            Some(other) => {
                let _ = std::fmt::Write::write_fmt(&mut text, format_args!("[{other} content]"));
            }
            None => {}
        }
    }
    if text.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        text = structured.to_string();
    }
    crate::exec::capped(&text, MAX_OUTPUT_BYTES)
}

impl ToolRuntime for McpTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.clone()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        let name = self.tool_name(call)?;
        if self.server.read_only_tools.iter().any(|tool| tool == name) {
            return Ok(ToolAction::read_only());
        }
        Ok(ToolAction::process(format!(
            "MCP {} {name}",
            self.server.name
        )))
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        let call = call.clone();
        Box::pin(async move {
            let name = self.tool_name(&call)?.to_owned();
            let result = self
                .request(
                    "tools/call",
                    json!({"name": name, "arguments": call.arguments}),
                )
                .await?;
            let text = render(&result);
            Ok(if result["isError"] == true {
                ToolOutput::error(text)
            } else {
                ToolOutput::success(text)
            })
        })
    }
}
