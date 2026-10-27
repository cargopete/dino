//! Running a case through another agent's own harness, so its result is scored
//! by the same hidden tests and scorer as a dancho run.
//!
//! The harness runs as installed and logged in, without the person's own
//! settings, and with its own permissions and sandbox, not dancho's. What is comparable is the outcome and the
//! claims; turns and tool errors are read from each harness's event stream and
//! are close to, not identical with, dancho's.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use dancho_sandbox::Sandbox;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::Case;
use crate::EvalError;
use crate::Metrics;
use crate::Outcome;
use crate::Variant;
use crate::case::fingerprint_tree;
use crate::runner::Finished;
use crate::runner::conclude;
use crate::runner::prompt_for;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Codex,
}

impl Harness {
    fn command(self, model: &str, prompt: &str, workspace: &Path) -> (String, Vec<String>) {
        let owned = |parts: &[&str]| parts.iter().map(|part| (*part).to_owned()).collect();
        match self {
            Self::ClaudeCode => (
                "claude".to_owned(),
                owned(&[
                    "-p",
                    prompt,
                    "--model",
                    model,
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--dangerously-skip-permissions",
                    "--no-session-persistence",
                    // Stock Claude Code, as Codex's --ignore-user-config gives
                    // stock Codex: no personal CLAUDE.md, hooks, MCP or skills.
                    "--setting-sources",
                    "project",
                    "--strict-mcp-config",
                    "--disable-slash-commands",
                    "--no-chrome",
                ]),
            ),
            Self::Codex => (
                "codex".to_owned(),
                [
                    "exec",
                    "--json",
                    "-m",
                    model,
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "--ignore-user-config",
                    "-s",
                    "workspace-write",
                    // Dependencies have to be fetchable, as dancho's cargo fetch allows.
                    "-c",
                    "sandbox_workspace_write.network_access=true",
                    "-C",
                ]
                .iter()
                .map(|part| (*part).to_owned())
                .chain([workspace.display().to_string(), prompt.to_owned()])
                .collect(),
            ),
        }
    }
}

/// What a harness's event stream says happened.
#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub answer: String,
    pub metrics: Metrics,
    pub completed: bool,
}

/// Reads Claude Code's `stream-json` output. A call counts as an error only
/// when it could not run; a command that ran and exited non-zero is a result,
/// as it is in dancho.
#[must_use]
pub fn parse_claude_code(stream: &str) -> Parsed {
    let mut parsed = Parsed::default();
    for line in stream.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("assistant") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" {
                        let name = block["name"].as_str().unwrap_or("?").to_owned();
                        *parsed.metrics.tool_calls.entry(name).or_insert(0) += 1;
                    }
                }
            }
            Some("user") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    let content = block["content"].to_string();
                    if block["is_error"] == true && !content.contains("Exit code") {
                        parsed.metrics.tool_errors += 1;
                    }
                }
            }
            Some("result") => {
                parsed.completed = event["terminal_reason"] == "completed";
                event["result"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut parsed.answer);
                parsed.metrics.model_turns =
                    u32::try_from(event["num_turns"].as_u64().unwrap_or(0)).unwrap_or(u32::MAX);
                let usage = &event["usage"];
                let count = |field: &str| usage[field].as_u64().unwrap_or(0);
                parsed.metrics.tokens.cached_input_tokens = count("cache_read_input_tokens");
                parsed.metrics.tokens.input_tokens = count("input_tokens")
                    + count("cache_creation_input_tokens")
                    + count("cache_read_input_tokens");
                parsed.metrics.tokens.output_tokens = count("output_tokens");
            }
            _ => {}
        }
    }
    parsed
}

/// Reads `codex exec --json` output. Codex reports no model-turn count, so each
/// tool step counts as one, plus one for the closing answer.
#[must_use]
pub fn parse_codex(stream: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut steps = 0_u32;
    for line in stream.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("item.completed") => {
                let item = &event["item"];
                match item["type"].as_str() {
                    Some("agent_message") => {
                        item["text"]
                            .as_str()
                            .unwrap_or_default()
                            .clone_into(&mut parsed.answer);
                    }
                    Some("reasoning") | None => {}
                    Some(kind) => {
                        steps += 1;
                        *parsed
                            .metrics
                            .tool_calls
                            .entry(kind.to_owned())
                            .or_insert(0) += 1;
                        let unrunnable = matches!(item["exit_code"].as_i64(), Some(126 | 127))
                            || item["status"] == "declined";
                        if unrunnable {
                            parsed.metrics.tool_errors += 1;
                        }
                    }
                }
            }
            Some("turn.completed") => {
                parsed.completed = true;
                let usage = &event["usage"];
                let count = |field: &str| usage[field].as_u64().unwrap_or(0);
                parsed.metrics.tokens.input_tokens += count("input_tokens");
                parsed.metrics.tokens.cached_input_tokens += count("cached_input_tokens");
                parsed.metrics.tokens.output_tokens += count("output_tokens");
                parsed.metrics.tokens.reasoning_tokens += count("reasoning_output_tokens");
            }
            _ => {}
        }
    }
    parsed.metrics.model_turns = steps + 1;
    parsed
}

/// Runs one case through `harness` with `model` and scores the result.
///
/// # Errors
///
/// Returns [`EvalError`] when the case cannot be set up or the harness cannot
/// be started. A harness that runs and fails is a result, not an error.
pub async fn run_external(
    case: &Case,
    scratch: &Path,
    sandbox: Arc<dyn Sandbox>,
    harness: Harness,
    model: &str,
) -> Result<Outcome, EvalError> {
    let workspace = case.materialise(scratch)?;
    let before = fingerprint_tree(&workspace)?;
    let prompt = prompt_for(case, true);
    let (program, arguments) = harness.command(model, &prompt, &workspace);
    let log_path = scratch.join(format!("{}.{}.jsonl", case.name, model));

    let started = Instant::now();
    let mut child = tokio::process::Command::new(&program)
        .args(&arguments)
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| EvalError::Setup(format!("cannot start {program}: {error}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| EvalError::Setup("the harness had no stdout".into()))?;
    // Generous, because a large case can take an hour; the case's turn budget
    // is not enforceable on another harness.
    let limit = Duration::from_secs(u64::from(case.max_turns.max(10)) * 60);
    let mut stream = String::new();
    let timed_out = tokio::time::timeout(limit, async {
        let _ = stdout.read_to_string(&mut stream).await;
        let _ = child.wait().await;
    })
    .await
    .is_err();
    let duration_ms = started.elapsed().as_millis();
    let _ = std::fs::write(&log_path, &stream);

    let parsed = match harness {
        Harness::ClaudeCode => parse_claude_code(&stream),
        Harness::Codex => parse_codex(&stream),
    };
    let mut failures = Vec::new();
    if timed_out {
        failures.push(format!(
            "the harness was stopped after {} minutes",
            limit.as_secs() / 60
        ));
    } else if !parsed.completed {
        failures.push("the harness did not complete the run".to_owned());
    }
    let variant = Variant {
        score: true,
        ..Variant::default()
    };
    conclude(
        case,
        scratch,
        sandbox,
        &variant,
        Finished {
            workspace,
            before,
            answer: parsed.answer,
            failures,
            metrics: parsed.metrics,
            duration_ms,
            log_path,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_streams_give_turns_tokens_errors_and_the_answer() {
        let stream = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}
{"type":"user","message":{"content":[{"type":"tool_result","is_error":null,"content":"ok"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}
{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"Exit code 1\ncat: x: No such file"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit"}]}}
{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"File has not been read yet"}]}}
{"type":"result","terminal_reason":"completed","num_turns":4,"result":"Done.","usage":{"input_tokens":4,"cache_creation_input_tokens":100,"cache_read_input_tokens":900,"output_tokens":50}}"#;

        let parsed = parse_claude_code(stream);

        assert!(parsed.completed);
        assert_eq!(parsed.answer, "Done.");
        assert_eq!(parsed.metrics.model_turns, 4);
        assert_eq!(parsed.metrics.tool_calls["Bash"], 1);
        assert_eq!(
            parsed.metrics.tool_errors, 1,
            "a non-zero exit is a result, not an error"
        );
        assert_eq!(parsed.metrics.tokens.input_tokens, 1_004);
        assert_eq!(parsed.metrics.tokens.cached_input_tokens, 900);
    }

    #[test]
    fn codex_streams_count_steps_and_only_unrunnable_commands_as_errors() {
        let stream = r#"{"type":"thread.started"}
{"type":"item.completed","item":{"type":"agent_message","text":"Reading first."}}
{"type":"item.completed","item":{"type":"command_execution","exit_code":0,"status":"completed"}}
{"type":"item.completed","item":{"type":"command_execution","exit_code":1,"status":"failed"}}
{"type":"item.completed","item":{"type":"command_execution","exit_code":127,"status":"failed"}}
{"type":"item.completed","item":{"type":"file_change","status":"completed"}}
{"type":"item.completed","item":{"type":"agent_message","text":"All done."}}
{"type":"turn.completed","usage":{"input_tokens":5000,"cached_input_tokens":4000,"output_tokens":300,"reasoning_output_tokens":40}}"#;

        let parsed = parse_codex(stream);

        assert!(parsed.completed);
        assert_eq!(parsed.answer, "All done.");
        assert_eq!(parsed.metrics.model_turns, 5);
        assert_eq!(parsed.metrics.tool_errors, 1);
        assert_eq!(parsed.metrics.tokens.cached_input_tokens, 4_000);
    }
}
