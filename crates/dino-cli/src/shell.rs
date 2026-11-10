//! `!command` at the prompt: a shell line the person runs as themselves.
//!
//! Outside the model, the approvals and the sandbox, because the person typed
//! it. A command body or anything a model wrote never comes here.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use tokio::io::AsyncReadExt as _;

/// The most of one command's output that goes with the next message.
const MAX_CARRIED_BYTES: usize = 16 * 1_024;

/// One line run, and what it printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    pub command: String,
    pub exit_code: Option<i32>,
    pub output: String,
}

/// Runs `command` through `sh -c` in the workspace, showing its output as it
/// arrives. Ctrl-C stops it, not the session.
pub async fn run(command: &str, workspace: &Path) -> Ran {
    let spawned = tokio::process::Command::new("sh")
        .arg("-c")
        // One stream, so the output keeps the order it was written in.
        .arg(format!("exec 2>&1\n{command}"))
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return Ran {
                command: command.to_owned(),
                exit_code: None,
                output: format!("could not start sh: {error}\n"),
            };
        }
    };
    let mut stdout = child.stdout.take();
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8 * 1_024];
    let mut interrupted = false;
    while let Some(pipe) = stdout.as_mut() {
        tokio::select! {
            read = pipe.read(&mut buffer) => match read {
                Ok(0) | Err(_) => stdout = None,
                Ok(count) => {
                    let mut terminal = std::io::stdout();
                    let _ = terminal.write_all(&buffer[..count]);
                    let _ = terminal.flush();
                    output.extend_from_slice(&buffer[..count]);
                }
            },
            _ = tokio::signal::ctrl_c(), if !interrupted => {
                interrupted = true;
                let _ = child.start_kill();
            }
        }
    }
    let exit_code = child.wait().await.ok().and_then(|status| status.code());
    let output = String::from_utf8_lossy(&output).into_owned();
    if !output.is_empty() && !output.ends_with('\n') {
        println!();
    }
    Ran {
        command: command.to_owned(),
        exit_code,
        output,
    }
}

/// How a command ended, in a few words.
pub fn exit_label(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => format!("exit {code}"),
        None => "stopped by a signal".to_owned(),
    }
}

/// What the commands the person ran add to their next message.
pub fn carried(ran: &[Ran]) -> String {
    let mut text = String::new();
    for one in ran {
        let output = dino_core::text::capped_output(one.output.trim_end(), MAX_CARRIED_BYTES);
        let _ = write!(
            text,
            "\n\nThe person ran `{}` in the workspace ({}):\n\n```\n{output}\n```",
            one.command,
            exit_label(one.exit_code)
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_line_runs_in_the_workspace_with_both_streams_in_order() {
        let root = std::env::temp_dir().join(format!("dino-bang-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("here.txt"), "").unwrap();

        let ran = run("ls; echo oops >&2; echo after; exit 3", &root).await;

        assert_eq!(ran.exit_code, Some(3));
        assert_eq!(ran.output, "here.txt\noops\nafter\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn output_goes_with_the_next_message_named_and_capped() {
        let ran = [
            Ran {
                command: "git status".into(),
                exit_code: Some(0),
                output: "clean\n".into(),
            },
            Ran {
                command: "yes".into(),
                exit_code: None,
                output: "y\n".repeat(20_000),
            },
        ];

        let text = carried(&ran);

        assert!(
            text.contains("ran `git status` in the workspace (exit 0):\n\n```\nclean\n```"),
            "{text}"
        );
        assert!(text.contains("(stopped by a signal)"), "{text}");
        assert!(text.contains("bytes omitted"), "{text}");
        assert!(text.len() < 2 * MAX_CARRIED_BYTES, "{}", text.len());
    }
}
