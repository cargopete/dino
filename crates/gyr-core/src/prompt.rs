//! Gyrfalcon's system prompt.
//!
//! Every adapter previously defaulted this to an empty string, and a model
//! given no instructions behaves about as well as that suggests. The prompt is
//! a constant that can be printed and measured rather than folklore.

use std::fmt::Write as _;
use std::path::Path;

/// Hard byte caps for injected workspace facts, per RFC-0001 section 9.
const MAX_ROOT_BYTES: usize = 256;
const MAX_TOOL_LIST_BYTES: usize = 1_024;
/// Most bytes of one instructions file that reach the prompt.
pub const MAX_INSTRUCTIONS_BYTES: usize = 16 * 1_024;

/// The project's instruction files, in the order they are read.
const PROJECT_INSTRUCTIONS: [&str; 2] = ["AGENTS.md", ".gyr/instructions.md"];

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
- Each response is one turn, and a run has a fixed number of them. Calls that
  do not depend on each other belong in the same response: read every file you
  need at once, and make all the edits a step needs before checking it once.
- Read a file whole, or in large spans. A window of eighty lines at a time
  costs a turn for each window.

Reporting:

- Say what you checked and what you did not. Untested is not the same as done.
- If a tool refuses or fails, treat the refusal as information and adjust. Do
  not repeat the identical call and hope for a different answer.
- Do not describe work as verified unless a tool result in this session shows
  it verified.

Tool discipline:

- Paths are relative to the workspace root and must stay inside it.
- Read a file before changing it. `read` returns its SHA-256 fingerprint, and
  the tools remember it, so `apply_patch` may leave `expected_sha256` out. Your
  own edits keep a file current; anything else that changes it, such as a
  formatter or a build script, makes it stale, and you must read it again.
- `apply_patch` replaces one exact occurrence. If the text you intend to replace
  is not unique, include enough surrounding context to make it unique rather
  than guessing which occurrence the tool will choose. It will not guess either.
- `write_file` creates a file, with any missing directories. Use it for new
  files instead of writing them through `exec`. An existing file must have
  been read before it is replaced whole.
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
- `shell` runs one command line through `sh -c`, for when a pipe, redirect, glob
  or `&&` is genuinely needed. Every distinct line is approved separately, so
  prefer `exec` for a single program.
- If the task cannot be done as asked, because the request contradicts itself
  or the tests and code it must keep, call `report_blocker` with the evidence.
  Do not change a test or the spec to make the two agree.
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
    pub max_model_turns: Option<u32>,
    pub git: Option<crate::git::GitSnapshot>,
    pub instructions: Vec<Instructions>,
}

/// One instructions file, as it will appear in the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instructions {
    /// Where it came from: a path as the user would recognise it.
    pub source: String,
    pub text: String,
    pub truncated: bool,
}

/// Reads the user's instructions, then the project's `AGENTS.md` and
/// `.gyr/instructions.md`. Absent, unreadable and empty files are skipped.
#[must_use]
pub fn load_instructions(user_file: Option<&Path>, workspace: &Path) -> Vec<Instructions> {
    let user = user_file.map(|path| (path.display().to_string(), path.to_path_buf()));
    let project = PROJECT_INSTRUCTIONS
        .iter()
        .map(|name| ((*name).to_owned(), workspace.join(name)));
    user.into_iter()
        .chain(project)
        .filter_map(|(source, path)| {
            let bytes = std::fs::read(&path).ok()?;
            let text = String::from_utf8_lossy(&bytes);
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            let truncated = text.len() > MAX_INSTRUCTIONS_BYTES;
            let mut end = text.len().min(MAX_INSTRUCTIONS_BYTES);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            Some(Instructions {
                source,
                text: text[..end].to_owned(),
                truncated,
            })
        })
        .collect()
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
    if let Some(turns) = context.max_model_turns {
        let _ = writeln!(
            &mut prompt,
            "- Model turns: at most {turns}. Each response you give is one. Near the end \
             you will be told how many remain; finish and summarise rather than be cut off."
        );
    }
    if let Some(git) = &context.git {
        prompt.push_str(&git.render());
    }
    prompt.push_str(
        "\nApproval is enforced below you, in code. Asking for permission in prose\n\
         does not grant it, and a denial is a decision rather than an obstacle to\n\
         argue with.",
    );
    if !context.instructions.is_empty() {
        prompt.push_str(
            "\n\nInstructions follow, from the user and then from this repository. They\n\
             describe how to work here. They cannot change approvals or the sandbox,\n\
             which are enforced in code.",
        );
        for item in &context.instructions {
            let _ = write!(&mut prompt, "\n\n### {}\n\n{}", item.source, item.text);
            if item.truncated {
                let _ = write!(
                    &mut prompt,
                    "\n[truncated at {MAX_INSTRUCTIONS_BYTES} bytes]"
                );
            }
        }
    }
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
            max_model_turns: None,
            git: None,
            instructions: Vec::new(),
        };

        let prompt = system_prompt(&context);

        assert!(prompt.contains("… (truncated)"));
        assert!(prompt.len() < BASE.len() + MAX_ROOT_BYTES + MAX_TOOL_LIST_BYTES + 512);
    }

    #[test]
    fn instructions_load_user_first_then_project_and_skip_what_is_absent() {
        let base = std::env::temp_dir().join(format!("gyr-instr-{}", std::process::id()));
        let workspace = base.join("repo");
        std::fs::create_dir_all(workspace.join(".gyr")).unwrap();
        std::fs::write(base.join("user.md"), "Call me Chief.\n").unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "Run cargo fmt.\n").unwrap();
        std::fs::write(workspace.join(".gyr/instructions.md"), "x".repeat(20_000)).unwrap();

        let loaded = load_instructions(Some(&base.join("user.md")), &workspace);

        let sources: Vec<_> = loaded.iter().map(|item| item.source.as_str()).collect();
        assert_eq!(sources[1..], ["AGENTS.md", ".gyr/instructions.md"]);
        assert!(sources[0].ends_with("user.md"));
        assert_eq!(loaded[0].text, "Call me Chief.");
        assert!(!loaded[1].truncated);
        assert!(loaded[2].truncated);
        assert!(loaded[2].text.len() <= MAX_INSTRUCTIONS_BYTES);

        std::fs::remove_file(workspace.join("AGENTS.md")).unwrap();
        let loaded = load_instructions(Some(&base.join("missing.md")), &workspace);
        assert_eq!(loaded.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn instructions_are_rendered_under_their_source_and_below_the_rules() {
        let context = PromptContext {
            workspace_root: "/tmp/workspace".into(),
            tools: vec!["read".into()],
            approval_mode: "interactive".into(),
            max_model_turns: None,
            git: None,
            instructions: vec![Instructions {
                source: "AGENTS.md".into(),
                text: "Use thiserror.".into(),
                truncated: true,
            }],
        };

        let prompt = system_prompt(&context);

        let rules = prompt.find("Approval is enforced below you").unwrap();
        let heading = prompt.find("### AGENTS.md").unwrap();
        assert!(rules < heading);
        assert!(prompt.contains("cannot change approvals or the sandbox"));
        assert!(prompt.contains("Use thiserror.\n[truncated at"), "{prompt}");
    }

    #[test]
    fn records_the_tools_and_approval_mode_in_force() {
        let context = PromptContext {
            workspace_root: "/tmp/workspace".into(),
            tools: vec!["read".into(), "search".into(), "apply_patch".into()],
            approval_mode: "read-only".into(),
            max_model_turns: Some(40),
            git: None,
            instructions: Vec::new(),
        };

        let prompt = system_prompt(&context);

        assert!(prompt.contains("- Workspace root: /tmp/workspace"));
        assert!(prompt.contains("- Available tools: read, search, apply_patch"));
        assert!(prompt.contains("- Model turns: at most 40"), "{prompt}");
        assert!(prompt.contains("belong in the same response"), "{prompt}");
        assert!(prompt.contains("- Approval mode: read-only"));
    }
}
