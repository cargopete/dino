//! The hooks a session runs itself: `prompt_submit` on each message, and
//! `awaiting_approval`, which tells a phone the session is stuck on a question.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dino_core::approval::Approver;
use dino_core::approval::AskContext;
use dino_core::approval::ReplyFuture;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;

use crate::style;

/// What the `prompt_submit` hooks add to `message`, for
/// [`dino_exec::with_context`], or the reason one refused it.
pub async fn submitted(
    hooks: &dino_exec::PromptHooks,
    message: &str,
) -> Result<Vec<(String, String)>, String> {
    let submitted = hooks.submitted(message).await;
    for problem in &submitted.problems {
        eprintln!("{}", style::paint(style::WARN, &format!("  {problem}")));
    }
    match submitted.refused {
        Some(reason) => Err(reason),
        None => Ok(submitted.context),
    }
}

/// An approver that runs the hooks once a question has waited `after`, and
/// goes on waiting. The hooks run on their own task, so an answer given while
/// they run is not held up by them.
pub struct Pinging<A> {
    inner: A,
    hooks: Arc<Vec<dino_exec::Hook>>,
    root: PathBuf,
    after: Duration,
}

impl<A> Pinging<A> {
    pub fn new(inner: A, hooks: Vec<dino_exec::Hook>, root: PathBuf, after: Duration) -> Self {
        Self {
            inner,
            hooks: Arc::new(hooks),
            root,
            after,
        }
    }
}

impl<A: Approver> Pinging<A> {
    fn waiting<'a>(&'a self, call: &ToolCall, mut asked: ReplyFuture<'a>) -> ReplyFuture<'a> {
        if self.hooks.is_empty() {
            return asked;
        }
        let tool = call.name.clone();
        Box::pin(async move {
            tokio::select! {
                reply = &mut asked => return reply,
                () = tokio::time::sleep(self.after) => {}
            }
            let hooks = Arc::clone(&self.hooks);
            let root = self.root.clone();
            let message = message(&root, &tool, self.after);
            tokio::spawn(async move {
                for problem in dino_exec::run_awaiting_hooks(&hooks, &root, &tool, &message).await {
                    eprintln!("{}", style::paint(style::WARN, &format!("  {problem}")));
                }
            });
            asked.await
        })
    }
}

/// What the ping says. The call's subject is left out: a command line can
/// carry a token, and this goes off the machine.
fn message(root: &std::path::Path, tool: &str, after: Duration) -> String {
    let place = root.file_name().map_or_else(
        || root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    format!(
        "dino in {place} has waited {} for an answer about a call to {tool}",
        crate::render::duration(after)
    )
}

impl<A: Approver> Approver for Pinging<A> {
    fn ask(&self, call: &ToolCall, action: &ToolAction) -> ReplyFuture<'_> {
        self.waiting(call, self.inner.ask(call, action))
    }

    fn ask_in(
        &self,
        call: &ToolCall,
        action: &ToolAction,
        context: &AskContext,
    ) -> ReplyFuture<'_> {
        self.waiting(call, self.inner.ask_in(call, action, context))
    }
}

#[cfg(test)]
mod tests {
    use dino_core::approval::ApprovalReply;
    use serde_json::json;

    use super::*;

    /// Answers once it is told to, as a person at the terminal would.
    struct Slow(Arc<tokio::sync::Notify>);

    impl Approver for Slow {
        fn ask(&self, _call: &ToolCall, _action: &ToolAction) -> ReplyFuture<'_> {
            let answer = Arc::clone(&self.0);
            Box::pin(async move {
                answer.notified().await;
                ApprovalReply::Once
            })
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "exec".into(),
            arguments: json!({"program": "curl", "args": ["https://example.com/TOKEN"]}),
        }
    }

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dino-waiting-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn hook(parts: &[&str]) -> dino_exec::Hook {
        dino_exec::Hook {
            command: parts.iter().map(|part| (*part).to_owned()).collect(),
            extension: None,
            tool: None,
            program: None,
        }
    }

    #[tokio::test]
    async fn an_unanswered_question_pings_once_and_still_waits_for_the_answer() {
        let root = root("ping");
        let answer = Arc::new(tokio::sync::Notify::new());
        let approver = Pinging::new(
            Slow(Arc::clone(&answer)),
            vec![hook(&[
                "sh",
                "-c",
                "printf '%s|%s|%s' \"$1\" \"$DINO_TOOL\" \"$DINO_MESSAGE\" >> pinged",
                "sh",
                "{tool}",
            ])],
            root.clone(),
            Duration::from_millis(50),
        );
        let call = call();
        let asked = approver.ask(&call, &ToolAction::process("curl"));
        let answered = async {
            for _ in 0..200 {
                if root.join("pinged").exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            answer.notify_one();
        };
        let (reply, ()) = tokio::join!(asked, answered);

        assert_eq!(reply, ApprovalReply::Once);
        let pinged = std::fs::read_to_string(root.join("pinged")).unwrap();
        assert!(pinged.starts_with("exec|exec|dino in "), "{pinged}");
        assert!(
            pinged.ends_with("for an answer about a call to exec"),
            "{pinged}"
        );
        assert!(
            !pinged.contains("TOKEN"),
            "the subject stays home: {pinged}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_question_answered_in_time_pings_nobody() {
        let root = root("quiet");
        let answer = Arc::new(tokio::sync::Notify::new());
        answer.notify_one();
        let approver = Pinging::new(
            Slow(answer),
            vec![hook(&["sh", "-c", "touch pinged"])],
            root.clone(),
            Duration::from_millis(200),
        );

        let reply = approver.ask(&call(), &ToolAction::process("curl")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(reply, ApprovalReply::Once);
        assert!(!root.join("pinged").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
