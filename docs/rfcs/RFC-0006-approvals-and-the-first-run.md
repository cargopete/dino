# RFC-0006: Approvals, session log and the first interactive run

| | |
|---|---|
| Status | implemented M1 |
| Date | 2026-08-27 |
| Depends on | RFC-0001, RFC-0003, RFC-0005 |
| Scope | tool classification, approval policy, session event log, cancellation, credentials, the `dino run` slice |

## 1. Purpose

Dino currently has a tested act-observe loop, three tested provider
transports and three tested workspace tools, none of which are connected to one
another. RFC-0005 deliberately refused to wire the filesystem tools to a live
model until an approval layer existed. This RFC specifies that layer and the
smallest honest vertical slice that turns the parts into a program a person can
run: a one-shot `dino run` against a real provider, with approvals enforced in
code, every decision recorded, and the turn cancellable.

The interactive terminal interface is explicitly **not** in this slice. It is
deferred to RFC-0007 so that it can be designed rather than accreted.

## 2. Decision

Approval is enforced by `dino-core`, between the agent loop and the tool
runtime. It is not a decorator around `ToolRuntime`, because RFC-0001 section 8
requires the event log to record the proposed action, the decision, the
execution and the result. A decorator would hide the first two from the log,
which is the same class of dishonesty as rendering absent data as healthy.

The tool runtime classifies; the policy decides; the core records and
dispatches. Three responsibilities, three owners, one direction of travel.

## 3. Tool classification

`ToolRuntime` gains a second required method:

```rust
fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError>;
```

`ToolAction` carries a class and an optional narrow subject:

```rust
pub enum ToolClass {
    ReadOnly,
    Mutating,
}

pub struct ToolAction {
    pub class: ToolClass,
    pub subject: Option<String>,
}
```

Two classes, not five. `exec` will need at least a distinction between
workspace-local and external effects, and a costly class may follow, but
Dino has no process tool yet and inventing policy it cannot enforce would
be decoration. Classes are added when the tool that needs them exists.

The runtime, not the core, performs classification because the runtime owns the
tool schemas. `dino-tools` classifies `read` and `search` as `ReadOnly` and
`apply_patch` as `Mutating` with the workspace-relative path as its subject.

**Classification resolves paths through the same helper as execution.** An
approval granted for one resolved path and spent on another would be worse than
no approval at all. A call whose arguments do not parse, or whose tool is
unknown, fails classification and is never executed; the failure is returned to
the model as an ordinary tool error.

Classification remains a filesystem-level fence with the time-of-check to
time-of-use window already recorded in RFC-0005 section 2. The operating-system
sandbox is still owed.

## 4. Approval policy

```rust
pub trait ApprovalPolicy: Send + Sync {
    fn decide(&self, call: &ToolCall, action: &ToolAction) -> DecisionFuture<'_>;
}
```

The decision is asynchronous because a human is one of its possible sources.

```rust
pub enum ApprovalDecision {
    Allowed { source: DecisionSource },
    Denied { reason: String },
}

pub enum DecisionSource {
    Policy,       // auto-allowed by class
    SessionRule,  // a narrow rule granted earlier in this session
    User,         // approved once, now
}
```

Three implementations ship in this slice:

- `AllowAll`, for tests and an explicit `--dangerously-allow-all` flag whose
  name is intended to be read aloud with some reluctance.
- `ReadOnly`, which allows `ReadOnly` and denies `Mutating`.
- `Interactive<A: Approver>`, which allows `ReadOnly` without prompting,
  consults the approver for `Mutating`, and caches granted session rules.

A session rule is keyed on `(tool name, subject)` and nothing else. It is never
keyed on a rendered description of the action, per RFC-0001 section 8. A rule
granted for `apply_patch` on `src/lib.rs` does not extend to `src/main.rs`, and
there is no wildcard grant in this slice.

**A denial is an observation, not a failure.** The core turns a denial into a
`ToolResult` with `is_error` set and the original call ID preserved, exactly as
RFC-0001 section 6 requires for tool errors. The model sees that it was
refused, and by whom, and may continue. It does not see a stack trace and the
process does not exit.

## 5. Session event log

`AgentEvent` gains one variant:

```rust
ToolDecided {
    model_turn: u32,
    call_id: String,
    action: ToolAction,
    decision: ApprovalDecision,
}
```

Events reach observers through a sink rather than only through the returned
vector, because a one-shot function returning its transcript at the end is not
a streaming agent:

```rust
pub trait EventSink: Send {
    fn emit(&mut self, event: &AgentEvent) -> Result<(), SinkError>;
}
```

`emit` is fallible on purpose. A log that quietly stops recording is precisely
the failure mode this project keeps promising not to ship. A sink error is
terminal for the run.

The log is append-only JSONL, one record per line, flushed per record:

```rust
enum SessionRecord {
    Started  { seq, unix_millis, session: SessionMeta },
    Event    { seq, unix_millis, event: AgentEvent },
    Finished { seq, unix_millis, outcome: RunOutcome },
}
```

`SessionMeta` records the session ID, the model key, the provider, the
canonical workspace root, the approval mode and the Dino version. It does
not record credentials.

Timestamps are Unix milliseconds. No date-formatting dependency is added for
M0; a reader may format them however it likes.

Records are flushed but not `fsync`ed per line, so a power loss may lose the
tail of a log. That is stated here rather than implied to be durable.

Native provider continuation state still lives in the adapter and is not
written to this log, per RFC-0001 section 9. Replay from a session log is
therefore not yet implemented; the log's first jobs are debugging, approval
audit and the eval corpus.

**Amended 2026-08-27.** RFC-0012 made good on the corpus. RFC-0016 made good on
presentation, and in doing so found that this log had never recorded what the
person asked, so the claim in RFC-0001 section 9 that presentation derives from
it could not have been true. `AgentEvent::Submitted` fixed that. Continuation is
still not replayed from here and cannot be, for the reason RFC-0014 section 2
gives.

## 6. Cancellation

`Agent::run` takes a `CancellationToken`. This adds `tokio-util` to the
workspace. An `AtomicBool` polled between stream events was considered and
rejected: it cannot interrupt an await on a model that has produced no output
for a minute, which is the exact case a user reaches for Ctrl-C.

Cancellation is observed at two points: while awaiting the next model event,
and between tool calls. A tool already executing is not interrupted in this
slice, because the filesystem tools are synchronous and short. `exec` will
require real process cancellation and will say so in its own RFC.

**Verified in source on 2026-08-27:** all three adapters commit native
conversation history only on the terminal event, in `commit_history` guarded by
a `terminal` check. A cancelled stream therefore leaves the provider session's
history unmodified and the interrupted turn is simply not part of it. No
cleanup path is required, and none is invented.

A cancelled run returns `StopReason::Cancelled` with the transcript accumulated
so far. It does not fabricate a `Finished` model event that the provider never
sent.

## 7. Configuration and credentials

This slice reads configuration from flags and the environment. There is no
configuration file yet, and pretending otherwise would only produce a format to
regret later.

**Amended 2026-08-27.** The surface settled and RFC-0015 built one, with the
condition this paragraph set as its stated trigger. The interesting part turned
out not to be the format: a project file arrives with a repository someone else
wrote, so it may not weaken a boundary or redirect a credential.

| Setting | Flag | Environment |
|---|---|---|
| Model key | `--model` | `DINO_MODEL` |
| Workspace root | `--workspace` | current directory |
| Session log | `--log` | `.dino/sessions/<id>.jsonl` |
| OpenAI key | | `OPENAI_API_KEY` |
| Anthropic key | | `ANTHROPIC_API_KEY` |
| Qwen endpoint | `--api-base` | `QWEN_API_BASE` |
| Qwen key | | `QWEN_API_KEY` |

Selecting a model whose credential is absent fails at configuration time with
the name of the missing variable. It does not fail later, inside a stream, in a
provider's own words.

RFC-0004's boundary stands: no subscription token belonging to another
application is read, copied or replayed.

## 8. System prompt

`dino-core` gains a prompt module. Until now every adapter defaulted
`system_prompt` to the empty string, and an agent given no instructions behaves
about as well as that suggests.

The prompt is assembled from a static Rust-oriented base and a small set of
workspace facts: the canonical root, the available tools and the approval mode
in force. It states the read-before-edit requirement, which is not advice but a
consequence of `apply_patch` demanding the fingerprint that `read` returns.

Every injected fact carries a byte cap, per RFC-0001 section 9. The prompt is a
constant that can be printed with `dino prompt`, so its cost and content are
inspectable rather than folkloric.

## 9. Command surface

```console
dino models [--json]
dino prompt [--model KEY]
dino run [--model KEY] [--workspace DIR] [--log PATH]
        [--max-turns N] [--read-only | --dangerously-allow-all]
        [--show-reasoning] [PROMPT]
```

`run` is one-shot. It reads the prompt from the argument or from standard
input, streams the turn, prompts for approval on standard error, and exits with
a non-zero status on error or denial-terminated failure.

Rendering lives in a `dino-cli` module that consumes `AgentEvent` and owns no
agent state. The transcript model is kept separate from the writer so that
RFC-0007's terminal interface is a second renderer against the same events, not
a rewrite of the loop. The M0 renderer is deliberately plain.

## 10. Out of scope for this slice

`exec`, the `cargo` diagnostic tool, the Rust verification gate, the operating
system sandbox, the interactive terminal interface, multi-turn conversation
state across invocations, log replay, compaction and configuration files.

These are missing features. None of them is delegated to the system prompt and
none of them is claimed to be present.

## 11. Verification

**Measured on 2026-08-27.** The workspace passes 47 tests and Clippy with
`-D warnings`. The tests this RFC promised, and what each one holds down:

- Loop and approval, in `dino-core/tests/agent.rs`: read-only calls never reach a
  person; a refusal returns to the provider under its original call ID and never
  executes; a session rule covers a second edit to the same file and does not
  cover a different file; an unclassifiable call is neither decided nor run.
- Classification, in `dino-tools`: a plain path and an in-workspace symbolic link
  to the same file both classify as that one resolved subject, so a second
  spelling cannot dodge a decision made about the first. Parent traversal and a
  symlink escape are refused during classification, before any policy sees them.
- Cancellation: a stream that cancels and then never resolves ends the run with
  `StopReason::Cancelled` and no fabricated terminal event. A token cancelled
  before the first turn sends nothing to the provider at all.
- Session log, in `dino-core/tests/session_log.rs`: records ordered
  `started`/`event`/`finished` with an unbroken sequence, parent directories
  created, and a creation failure raised at creation rather than later.
- Credentials, in `dino-cli`: each provider names the environment variable it is
  missing, before a session is built.

**Observed on 2026-08-27**, driving the built `dino` binary against a local fake
vLLM endpoint on the loopback interface, because no vendor credential is present
on this machine:

- A read round trip: tool call, auto-allowed decision, execution, result and a
  final text turn, with the 2,301-byte system prompt and all three tool schemas
  present in the request body.
- A refusal under `--read-only`: the file's SHA-256 is unchanged and the
  provider's next request carries `"Tool error: refused by approval policy: this
  session runs in read-only mode"` against `call-1`.
- An interactive approval answered `a`: one prompt, then a second edit to the
  same file allowed by `SessionRule` without a second prompt, both edits written.

This exercises the transport, not a vendor endpoint. Live provider runs remain
opt-in and are not part of `cargo test --workspace`.

## 12. Open questions

- Whether a session rule should be expressible for a directory subtree once
  `exec` exists, and if so how a subtree grant is displayed without becoming
  the friendly description this RFC forbids.
- Whether denial should count against the model-turn budget. It currently does,
  which bounds a model that responds to refusal by trying the same edit again.
- Whether the log should record redacted tool arguments for large patches, or
  their fingerprints only.
