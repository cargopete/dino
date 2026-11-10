//! Hooks before and after any call, and on each message sent.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::approval::AllowAll;
use dino_core::approval::ApprovalPolicy;
use dino_core::approval::ReadOnly;
use dino_exec::AfterCall;
use dino_exec::BeforeCall;
use dino_exec::Hook;
use dino_exec::PromptHooks;
use dino_protocol::ApprovalDecision;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use dino_protocol::ToolOutput;
use dino_sandbox::Unconfined;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn fixture(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("dino-call-hooks-{}-{name}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::canonicalize(root).unwrap()
}

fn hook(parts: &[&str]) -> Hook {
    Hook {
        command: parts.iter().map(|part| (*part).to_owned()).collect(),
        extension: None,
        tool: None,
        program: None,
    }
}

fn exec(program: &str) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "exec".into(),
        arguments: json!({"program": program, "args": ["-rf", "target"]}),
    }
}

fn gate<P: ApprovalPolicy>(root: &PathBuf, inner: P, hooks: Vec<Hook>) -> BeforeCall<P> {
    BeforeCall::new(inner, hooks, root, Arc::new(Unconfined))
}

#[tokio::test]
async fn a_before_call_hook_refuses_with_what_it_printed() {
    let root = fixture("refuse");
    let policy = gate(
        &root,
        AllowAll,
        vec![hook(&[
            "sh",
            "-c",
            "grep -q '\"program\":\"rm\"' && { echo 'no rm here; use cargo clean'; exit 2; }; exit 0",
        ])],
    );

    let refused = policy
        .decide(&exec("rm"), &ToolAction::process("rm -rf target"))
        .await;
    let allowed = policy.decide(&exec("ls"), &ToolAction::process("ls")).await;

    assert_eq!(
        refused,
        ApprovalDecision::denied("no rm here; use cargo clean")
    );
    assert!(
        matches!(allowed, ApprovalDecision::Allowed { .. }),
        "{allowed:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_hook_that_passes_cannot_approve_what_the_policy_refuses() {
    let root = fixture("stricter");
    let policy = gate(&root, ReadOnly, vec![hook(&["true"])]);

    let decision = policy
        .decide(&exec("rm"), &ToolAction::process("rm -rf target"))
        .await;

    assert!(
        matches!(decision, ApprovalDecision::Denied { .. }),
        "{decision:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_failing_before_call_hook_refuses_and_names_only_its_program() {
    let root = fixture("broken");
    let policy = gate(
        &root,
        AllowAll,
        vec![hook(&["sh", "-c", "exit 1", "sh", "SECRET"])],
    );

    let decision = policy.decide(&exec("ls"), &ToolAction::process("ls")).await;

    assert_eq!(
        decision,
        ApprovalDecision::denied("the before_call hook `sh` exited 1")
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_hook_for_a_program_runs_for_that_program_only() {
    let root = fixture("program");
    let only_rm = Hook {
        program: Some("rm".into()),
        ..hook(&["sh", "-c", "exit 2"])
    };
    let policy = gate(&root, AllowAll, vec![only_rm]);
    let shell = |line: &str| ToolCall {
        id: "call-2".into(),
        name: "shell".into(),
        arguments: json!({"command": line}),
    };
    let decide = |call: ToolCall| {
        let policy = &policy;
        async move {
            matches!(
                policy.decide(&call, &ToolAction::process("x")).await,
                ApprovalDecision::Denied { .. }
            )
        }
    };

    assert!(decide(exec("rm")).await);
    assert!(decide(exec("/bin/rm")).await, "matched by file name");
    assert!(!decide(exec("ls")).await);
    assert!(decide(shell("rm -rf target")).await);
    assert!(
        !decide(shell("ls; rm -rf target")).await,
        "the first word only"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Returns `ran`, as any tool's result.
struct Echo;

impl ToolRuntime for Echo {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    fn classify(&self, _call: &ToolCall) -> Result<ToolAction, ToolError> {
        Ok(ToolAction::read_only())
    }

    fn execute(&self, _call: &ToolCall) -> ToolFuture<'_> {
        Box::pin(async { Ok(ToolOutput::success(json!({"ran": true}).to_string())) })
    }
}

#[tokio::test]
async fn an_after_call_hook_sees_the_result_and_adds_what_it_prints() {
    let root = fixture("after");
    let for_read = Hook {
        tool: Some("read".into()),
        ..hook(&["sh", "-c", "grep -q 'ran' && echo logged"])
    };
    let tools = AfterCall::new(
        Box::new(Echo),
        vec![for_read, hook(&["true"])],
        &root,
        Arc::new(Unconfined),
    );
    let call = |name: &str| ToolCall {
        id: "call-3".into(),
        name: name.into(),
        arguments: json!({}),
    };

    let read: Value =
        serde_json::from_str(&tools.execute(&call("read")).await.unwrap().content).unwrap();
    let list = tools.execute(&call("list")).await.unwrap();

    assert_eq!(read["ran"], true);
    assert_eq!(
        read["after_call"],
        json!([{"program": "sh", "output": "logged"}])
    );
    assert_eq!(
        list.content,
        json!({"ran": true}).to_string(),
        "nothing said"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn prompt_hooks_add_context_or_refuse() {
    let root = fixture("prompt");
    let ticket = hook(&["sh", "-c", "cat > /dev/null; echo ticket DINO-7"]);
    let refuse = hook(&[
        "sh",
        "-c",
        "grep -q deploy && { echo 'not on a Friday'; exit 2; }; exit 0",
    ]);
    let broken = hook(&["sh", "-c", "exit 4", "sh", "SECRET"]);
    let hooks = PromptHooks::new(vec![ticket, broken, refuse], &root, Arc::new(Unconfined));

    let passed = hooks.submitted("fix the build").await;
    let refused = hooks.submitted("deploy it").await;

    assert_eq!(
        passed.context,
        [(
            "sh -c cat > /dev/null; echo ticket DINO-7".to_owned(),
            "ticket DINO-7".to_owned()
        )]
    );
    assert_eq!(passed.refused, None);
    assert_eq!(passed.problems, ["the prompt_submit hook `sh` exited 4"]);
    assert_eq!(refused.refused.as_deref(), Some("not on a Friday"));
    assert_eq!(
        dino_exec::with_context("fix the build".into(), &passed.context),
        "fix the build\n\nprompt_submit: sh -c cat > /dev/null; echo ticket DINO-7\nticket DINO-7"
    );
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn awaiting_hooks_are_told_the_tool_and_the_message() {
    let root = fixture("awaiting");
    let told = hook(&[
        "sh",
        "-c",
        "printf '%s|%s|%s' \"$DINO_TOOL\" \"$DINO_MESSAGE\" \"$1\" > told.txt",
        "sh",
        "{tool}",
    ]);

    let problems =
        dino_exec::run_awaiting_hooks(&[told, hook(&["false"])], &root, "exec", "stuck").await;

    assert_eq!(problems, ["the awaiting_approval hook `false` exited 1"]);
    assert_eq!(
        fs::read_to_string(root.join("told.txt")).unwrap(),
        "exec|stuck|exec"
    );
    let _ = fs::remove_dir_all(&root);
}
