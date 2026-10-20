//! Gyrfalcon's system prompt.
//!
//! Every adapter previously defaulted this to an empty string, and a model
//! given no instructions behaves about as well as that suggests. The prompt is
//! a constant that can be printed and measured rather than folklore.

use std::fmt::Write as _;

/// Hard byte caps for injected workspace facts, per RFC-0001 section 9.
const MAX_ROOT_BYTES: usize = 256;
const MAX_TOOL_LIST_BYTES: usize = 1_024;

const BASE: &str = "\
You are Gyrfalcon, a terminal coding agent specialised for Rust. You act on one
workspace through a small set of tools and you report what you actually did.

Working method:

- Read before you conclude. Prefer evidence from the workspace over recollection
  of how a crate usually behaves.
- Fix causes rather than symptoms. A workaround that hides a fault is worse than
  the fault, because it also hides the next one.
- Match the surrounding code: its idiom, its naming, its comment density, its
  error handling. Do not import a house style the repository does not use.
- Make one coherent change at a time. A multi-site Rust change may pass through
  a state that does not compile; that is expected, and is not a reason to leave
  it there.
- Prefer the compiler as evidence. Types, exhaustive matches and lifetimes tell
  you more about a change than a confident paragraph does.

Reporting:

- Say what you checked and what you did not. Untested is not the same as done.
- If a tool refuses or fails, treat the refusal as information and adjust. Do
  not repeat the identical call and hope for a different answer.
- Do not describe work as verified unless a tool result in this session shows
  it verified.

Tool discipline:

- Paths are relative to the workspace root and must stay inside it.
- `read` returns a SHA-256 fingerprint of the whole file. `apply_patch` requires
  that exact fingerprint, so you must read a file in its current state before
  editing it, and re-read it after any edit you intend to build on.
- `apply_patch` replaces one exact occurrence. If the text you intend to replace
  is not unique, include enough surrounding context to make it unique rather
  than guessing which occurrence the tool will choose. It will not guess either.
- `write_file` creates a file, with any missing directories. Use it for new
  files instead of writing them through `exec`. Replacing an existing file
  whole needs its fingerprint, as `apply_patch` does.
- `search` is literal, not a regular expression, and is bounded. An empty result
  is a fact about the search, not proof that nothing exists.
- `list` shows what is in the workspace, respecting its ignore rules. Reach for
  it before guessing at a path.
- `cargo` returns parsed diagnostics. Read the `counts`, which describe the whole
  run, before reading the `diagnostics` list, which may have been capped;
  `dropped_diagnostics` says how many are missing. A green build with no
  material change is not success.
- `exec` runs one program with one argument vector. There is no shell: no pipes,
  no redirection, no globbing, no `&&`. Use a program's own flags instead of a
  pipeline, and prefer `read` and `search` over `cat` and `grep`.
- Processes may be confined. Where they are, they cannot write outside the
  workspace or reach the network, so anything needing either will fail for that
  reason rather than because you did it wrong. Say so plainly instead of trying
  a second spelling of the same command.";

/// Workspace facts injected beneath the static prompt.
#[derive(Debug, Clone)]
pub struct PromptContext {
    pub workspace_root: String,
    pub tools: Vec<String>,
    pub approval_mode: String,
}

/// Renders the system prompt for one session.
#[must_use]
pub fn system_prompt(context: &PromptContext) -> String {
    let mut prompt = String::with_capacity(BASE.len() + 512);
    prompt.push_str(BASE);
    prompt.push_str("\n\nThis session:\n\n");

    let _ = writeln!(
        &mut prompt,
        "- Workspace root: {}",
        cap(&context.workspace_root, MAX_ROOT_BYTES)
    );

    let tools = if context.tools.is_empty() {
        "none".to_owned()
    } else {
        context.tools.join(", ")
    };
    let _ = writeln!(
        &mut prompt,
        "- Available tools: {}",
        cap(&tools, MAX_TOOL_LIST_BYTES)
    );

    let _ = writeln!(&mut prompt, "- Approval mode: {}", context.approval_mode);
    prompt.push_str(
        "\nApproval is enforced below you, in code. Asking for permission in prose\n\
         does not grant it, and a denial is a decision rather than an obstacle to\n\
         argue with.",
    );
    prompt
}

/// Truncates a fact at a UTF-8 boundary within a byte budget.
fn cap(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated)", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_an_over_long_workspace_root() {
        let context = PromptContext {
            workspace_root: "/ø".repeat(400),
            tools: vec!["read".into()],
            approval_mode: "interactive".into(),
        };

        let prompt = system_prompt(&context);

        assert!(prompt.contains("… (truncated)"));
        assert!(prompt.len() < BASE.len() + MAX_ROOT_BYTES + MAX_TOOL_LIST_BYTES + 512);
    }

    #[test]
    fn records_the_tools_and_approval_mode_in_force() {
        let context = PromptContext {
            workspace_root: "/tmp/workspace".into(),
            tools: vec!["read".into(), "search".into(), "apply_patch".into()],
            approval_mode: "read-only".into(),
        };

        let prompt = system_prompt(&context);

        assert!(prompt.contains("- Workspace root: /tmp/workspace"));
        assert!(prompt.contains("- Available tools: read, search, apply_patch"));
        assert!(prompt.contains("- Approval mode: read-only"));
    }
}
