# RFC-0008: The structured Cargo tool

| | |
|---|---|
| Status | implemented M2 |
| Date | 2026-08-27 |
| Depends on | RFC-0001, RFC-0006 |
| Scope | process execution, the `cargo` tool, diagnostic shape, the third tool class |

RFC-0007 is reserved for the interactive terminal interface and is not yet
written. This RFC takes the next number rather than renumbering the references
already pointing at it.

## 1. Decision

Dino's first process-executing tool is `cargo`, not `exec`.

RFC-0001 section 6 lists both, and section 7 asks for the compiler as a source
of structured evidence. A general `exec` is a shell by another name: its
approval surface is unbounded, and the operating-system sandbox that RFC-0001
section 8 requires does not exist yet. A bounded `cargo` tool over a fixed set
of subcommands is narrower, is the thing the Rust specialisation actually needs,
and can ship honestly before the sandbox.

This is a deferral, not a reprieve. `exec` waits for the sandbox and gets its
own RFC. Until then, Dino cannot run an arbitrary command, and says so.

## 2. What it does not protect you from

`cargo check` runs build scripts. `cargo test` runs test code. Both execute
arbitrary code from the workspace and its dependency graph, on the host, with no
sandbox. **No `cargo` subcommand is read-only**, and this RFC does not pretend
that `check` is safer than `test` because it sounds more passive.

What the tool does provide is a bounded argument surface, an explicit manifest
path, a filtered environment, hard output limits, a wall-clock limit,
cancellation, and an approval decision recorded before anything runs. That is a
smaller claim than a sandbox and it is the one being made.

**Amended 2026-08-27:** RFC-0009 added the sandbox on macOS, so a confined run
also cannot write outside the workspace or reach the network. It can still read
anything on the machine. See RFC-0009 section 2 for what that combination does
and does not guarantee.

## 3. The third tool class

RFC-0006 shipped two classes and said further ones arrive with the tool that
needs them. This is that tool:

```rust
pub enum ToolClass {
    ReadOnly,
    Mutating,
    Process,
}
```

`Process` is never auto-allowed by any policy, including `ReadOnly`, which
refuses it outright. The subject is the normalised argument vector, so a session
rule granted for `cargo check --workspace` does not cover `cargo test`.

**A known weakness, stated rather than discovered later:** an argv rule approves
a command, and the code that command runs may have changed since the rule was
granted. A model that is allowed to run `cargo test` and can also edit test
files can, in principle, arrange for a later approved run to do something new.
The prompt shown at approval time names this. A sandbox is the real answer and
this rule exists because refusing session rules entirely would make the tool
unusable.

## 4. Command surface

One tool with a closed `command` field. No free-form arguments, ever.

| `command` | Runs |
|---|---|
| `metadata` | `cargo metadata --format-version 1 --no-deps` |
| `check` | `cargo check --workspace --all-targets --message-format=json` |
| `clippy` | `cargo clippy --workspace --all-targets --message-format=json` |
| `test` | `cargo test --workspace` |
| `fmt` | `cargo fmt --all --check` |

Optional `package` narrows `check`, `clippy` and `test` to one workspace member,
replacing `--workspace` with `-p <name>`. Optional `filter` narrows `test` to
matching test names. Both are validated against a conservative character set
before they reach a process, so neither can smuggle a second argument.

`build` is deliberately absent. It costs more than `check` and tells an agent
nothing `check` does not, which makes it a slower way to learn the same thing.

Every invocation passes `--manifest-path <root>/Cargo.toml` explicitly. Cargo
searches parent directories for a manifest when not told otherwise, and a tool
confined to a workspace root that then builds its grandparent would be an
entertaining way to undo RFC-0005 section 2.

### 4.1 Amendment, 2026-08-27

RFC-0009 added containment. Two consequences land here. A confined run has no
network, so the `cargo` tool passes `--offline` on every subcommand except
`fmt`, which does not accept the flag and does not need it. And the child's
`TMPDIR` is set to a directory inside the workspace, because a confining profile
denies the system one and widening the writable set to reach it would defeat the
point.

## 5. Environment

The child process starts from a cleared environment holding only `PATH`, `HOME`,
`CARGO_HOME`, `RUSTUP_HOME`, `CARGO_TERM_COLOR=never` and `TERM=dumb`. The
inherited environment is not passed through, so the agent's own provider
credentials are not visible to a build script.

The network is not restricted. Cargo may fetch dependencies. RFC-0001 section 8
requires network policy to be separate from filesystem policy, and neither
policy is enforced at the operating-system level yet. This is recorded as a gap.

## 6. Output shape

The tool returns JSON, never raw compiler noise:

```json
{
  "command": ["cargo", "check", "--workspace", "..."],
  "exit_code": 101,
  "timed_out": false,
  "counts": {"errors": 2, "warnings": 7},
  "diagnostics": [
    {"level": "error", "code": "E0308", "message": "mismatched types",
     "file": "crates/dino-core/src/lib.rs", "line": 214, "column": 9,
     "rendered": "error[E0308]: mismatched types\n ..."}
  ],
  "dropped_diagnostics": 0,
  "output": "",
  "truncated": false
}
```

Diagnostics come from `--message-format=json`, one `compiler-message` per line.
`metadata` returns a summarised package graph rather than the raw document,
which is routinely larger than the file the agent was asked about. `fmt` and
`test` have no machine-readable diagnostic stream worth the name, so their
captured output is returned under `output`, capped.

**Nothing is dropped silently.** When the diagnostic cap is reached, errors are
retained ahead of warnings and `dropped_diagnostics` states how many were not
returned. A truncated byte stream sets `truncated`. When the rendered-text
budget runs out, the rendering is dropped and the diagnostic is kept, because
losing a location is worse than losing a pretty version of it. An agent that
reads `"errors": 0` must be able to trust it, so the counts describe the whole
run even when the returned list does not.

**Diagnostics are deduplicated, and the counts follow.** `--all-targets`
compiles a library once as a library and again as its own test target, so one
mistake in `src/lib.rs` arrives twice on the wire. Two identical diagnostics,
matched on level, code, file, line, column and message, are one mistake. This
was discovered during implementation rather than designed in: the first check
test asserted one error and got two, which is the correct number of messages and
the wrong number of problems.

Default limits: 50 diagnostics, 32 KiB of rendered text, 32 KiB of captured
output, and a 600-second wall clock. A run that exceeds the clock is killed, and
returns `timed_out: true` rather than a fabricated failure.

## 7. Cancellation

RFC-0006 cancelled between tool calls but never during one, because the
filesystem tools were synchronous and brief. A `cargo test` that hangs is
precisely the case a person reaches for Ctrl-C, so the agent core now wraps tool
execution in the cancellation token as well.

The child is spawned with `kill_on_drop`, so dropping the cancelled future kills
the process group's leader. The run then reports `StopReason::Cancelled` and no
tool result is fabricated for the call that was interrupted.

## 8. Composing tool sets

`Agent` held one `ToolRuntime`. A session now needs filesystem tools and Cargo
tools at once, so `ToolRuntime` gains a `definitions` method and `dino-core`
gains a `ToolSet` that dispatches by tool name across several runtimes.

Declaring definitions on the trait rather than as an associated function means a
runtime describes its own surface, and `ToolSet` can refuse two runtimes that
claim the same tool name at construction rather than picking one at dispatch
time and being quietly wrong for the rest of the session.

## 9. Crate

A new `dino-rust` crate holds the Cargo tool and the diagnostic types. It is not
an empty crate awaiting content, per RFC-0001 section 4.1: it ships the tool,
the JSON parsing, the environment policy and its tests together.

The Rust diagnostic gate from RFC-0001 section 7, which tracks a diagnostic set
across an edit batch and refuses a batch that stops improving, is the natural
next occupant of this crate. It is not in this slice.

## 10. Out of scope

General `exec`, the operating-system sandbox, network policy, the diagnostic
gate, `cargo metadata` dependency graphs beyond workspace members, rust-analyzer
SCIP data, and test selection from the changed package graph.

## 11. Verification

**Measured on 2026-08-27.** The workspace passes 65 tests and Clippy with
`-D warnings`. Tests that invoke a real `cargo` run a dependency-free
single-package fixture in a temporary directory, so they need a toolchain but no
network, and they are part of `cargo test --workspace`.

- Clean check: exit 0, no errors, and `--manifest-path Cargo.toml` present in
  the recorded command.
- Failing check: exit 101, one error, `E0308` at `src/lib.rs:2:5` with its
  rendered text.
- `metadata` summarised to one package with its edition and relative manifest
  path, rather than the raw document.
- The wall clock: a fixture test sleeping for 120 seconds, killed after 2, with
  `timed_out: true` and a null exit code. This is the same drop-kills-the-child
  path a cancelled agent run takes.
- Every subcommand classifies as `Process`, including `metadata`, and every
  subject carries the explicit manifest path.
- Session rules: `check`, `test` and `check -p fixture` produce three different
  rule keys.
- Argument validation refusing `--offline`, `-p`, `fixture --offline`,
  `fixture;ls` and the empty string, and refusing an unknown field rather than
  ignoring it, and refusing `filter` outside `test`.
- `ReadOnly` refusing a `Process` call, and `Interactive` asking about one
  rather than auto-allowing it.
- Deduplication, capping and the rendered budget, unit tested against recorded
  compiler JSON.

**Observed on 2026-08-27**, driving the built `dino` binary against a local fake
vLLM endpoint over a fixture with a deliberate type error: the approval prompt
names the full command and the unsandboxed caveat, and the model receives 566
bytes of structured diagnostics naming `E0308` at `src/lib.rs:2:5` in place of a
page of compiler output.

The claim that the manifest path prevents an upward escape is asserted on the
argument vector, not by building a nested workspace. `CargoTool::new` already
refuses a root without a manifest, so the flag is belt as well as braces, and
this RFC does not claim a test it did not write.

## 12. Open questions

- Whether `test` should gain a machine-readable path once libtest's JSON output
  stabilises, or whether a `--format json` shim is worth carrying meanwhile.
- Whether `metadata` should include the dependency graph behind a flag, given
  its size against an agent's context budget.
- Whether a `Process` session rule should expire after any `Mutating` call to a
  file the command would compile, which is a stricter rule than section 3's and
  may be more annoying than it is useful.
