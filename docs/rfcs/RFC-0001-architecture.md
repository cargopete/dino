# RFC-0001: Gyrfalcon architecture

| | |
|---|---|
| Status | accepted for M0 |
| Date | 2026-08-23 |
| Scope | product boundary, core architecture, safety, Rust specialisation |

## 1. Purpose

Gyrfalcon is a terminal coding agent written in Rust. It is intended to be a
serious educational implementation of the machinery behind tools such as
Claude Code and Codex, and a useful Rust coding agent in its own right. It is
open source, including its design record, failures, measurements and evals.

The MVP supports three provider families:

1. OpenAI Terra through the native Responses API.
2. Anthropic Claude Opus through the native Messages API.
3. A bounded Qwen family through vLLM or SGLang's OpenAI-compatible Chat
   Completions surface, with Qwen-native tool and reasoning parsers.

"Bring your own AI" means the user supplies credentials or a serving endpoint
for one of those supported contracts. It does not mean arbitrary provider
compatibility in the MVP.

## 2. Goals

- A real streaming act-observe loop with cancellation and bounded turns.
- Native provider adapters which preserve each provider's continuation state.
- Explicit approval and sandbox policies, enforced below the model.
- Append-only session events sufficient for replay, debugging and evals.
- Exact file edits, visible diffs and refusal of ambiguous application.
- Rust-aware repository discovery, diagnostics and verification.
- Honest degraded modes. Missing data is never rendered as healthy state.
- A small public codebase whose important invariants can be understood.

## 3. Non-goals for the MVP

- A universal LLM abstraction.
- Bedrock, Vertex, OpenRouter, Azure, Gemini, or arbitrary compatible APIs.
- Claude Max credential reuse before Anthropic explicitly supports third-party
  clients doing so.
- A plugin marketplace, remote MCP catalogue, desktop application, cloud agent,
  multi-agent orchestration, or background daemon.
- Automatic commits, pushes, pull requests, deployments or purchases.
- Semantic indexing before the ordinary tool loop and eval harness work.

## 4. Architecture

The central boundary is a provider-owned session and a normalised event stream.

```text
 user submission
       |
       v
  gyr-core agent loop ------> append-only AgentEvent log
       |                                  |
       | TurnInput                        +--> CLI/TUI
       v                                  +--> replay
 ModelSession                             +--> eval corpus
       |
       +--> OpenAI Responses adapter
       +--> Anthropic Messages adapter
       `--> Qwen Chat Completions adapter

 tool calls --> policy --> approval --> sandbox --> tool result
                                          |
                                          `--> Rust verification pipeline
```

The provider session owns native conversation state. The core does not rebuild
an OpenAI response item, Anthropic content block or Qwen reasoning field from a
lossy generic transcript. It receives normalised events for presentation and
dispatch, while the adapter retains whatever native items are required for the
next request.

### 4.1 Crates

- `gyr-protocol`: stable internal values crossing crate and frontend boundaries.
- `gyr-model`: provider session traits and the explicit model catalogue.
- `gyr-core`: agent state machine, tool dispatch and session event emission.
- `gyr-tools`: workspace-rooted filesystem tools and their hard output limits.
- `gyr-cli`: the `gyr` executable. A line UI comes before a full TUI.
- Provider crates will be introduced one at a time once their contract tests
  exist. Empty crates are not architecture.

### 4.2 Agent state machine

For one user submission:

1. Send `TurnInput::User` to the provider session.
2. Stream model events to the session log and frontend.
3. Collect completed tool calls by call ID.
4. On a tool stop, validate and execute every call permitted by policy.
5. Send ordered tool results to the same provider session.
6. Continue until the provider ends the turn, the user cancels, an error is
   terminal, or the model-turn budget is exhausted.

A stream ending without a terminal event is an error. A provider claiming the
turn ended while completed tool calls remain unresolved is an error. Duplicate
call IDs are an error. These cases are protocol failures, not prose to show the
model and hope it feels contrite.

## 5. Provider capabilities

Capabilities are explicit model data, not inferred from a URL or model-name
substring. The first catalogue records:

- provider protocol;
- native context size where documented;
- reasoning support and allowed effort values;
- parallel tool-call support;
- image-input support;
- required serving parsers;
- recommended sampling parameters where documented.

An adapter is allowed to expose less than a model can theoretically do. It may
not expose more. Unsupported capabilities fail during configuration.

## 6. Tool surface

The initial surface is deliberately small:

- `read`: bounded line ranges, with line numbers and truncation metadata.
- `search`: gitignore-aware text search with an explicit total and output cap.
- `apply_patch`: a diff-based edit primitive with path and stale-file checks.
- `exec`: cancellable process execution through the sandbox and approval layer.
- `cargo`: structured Rust checks and diagnostics, not merely an alias for a
  free-form shell command.

Tools return structured success or error results. Tool errors are ordinary
observations and preserve their call IDs. Provider adapters translate those
results into their native history shape.

## 7. Rust specialisation

Gyrfalcon treats the compiler as a source of structured evidence.

- `cargo metadata` supplies the workspace and package graph.
- `cargo check --workspace --all-targets --message-format=json` supplies
  machine-readable diagnostics.
- `cargo fmt --check` verifies formatting without quietly rewriting unrelated
  files.
- Tests are selected from the changed package graph before a workspace-wide
  run is considered.
- rust-analyzer SCIP data may later provide exact symbol references. It is a
  map and never authority for current file contents.

An edit batch does not have to compile after every individual change. Multi-site
Rust changes often pass through a red state. The gate tracks the diagnostic set
and permits accumulated edits when they make measurable progress. It rolls back
or refuses a batch that stops improving. A green build with no material diff is
not task success.

## 8. Safety model

Prompt instructions are guidance. Safety is code.

- Reads and writes are resolved against explicit filesystem roots.
- Symlink and canonical-path handling happens at the boundary, not in prompts.
- Network policy is separate from filesystem policy.
- Commands are classified before execution and may require approval.
- Approval applies to an exact action or a narrow reusable rule, never to a
  friendly textual description of what the command probably does.
- Destructive, external, costly and scope-expanding actions stop for approval.
- The event log records the proposed action, decision, execution and result.

Gyrfalcon supports macOS and Linux. **Windows is not a target**, decided
2026-08-27 rather than deferred: it would need a containment design of its own,
and claiming support because the Rust compiles there is exactly the kind of
claim this document exists to refuse. On Windows, `detect` reports unavailable
and processes are refused, as on any platform without an implementation.

## 9. Persistence and context

The event log is append-only JSONL. Presentation is derived from it, but native
provider continuation state is stored separately and versioned. Every injected
context item has a hard byte or token cap. Compaction is an explicit event with
the replaced range recorded.

Provider-side continuation IDs are an optimisation and may expire. Replayable
local history remains the recovery path. Encrypted or opaque reasoning items are
stored only when the provider contract requires their replay and the user has
chosen that retention mode.

## 10. Build order

1. Provider-neutral loop and fake-provider integration tests. **Done.**
2. Read, search and exact patch tools in a temporary workspace. **Done.**
3. Qwen adapter, because its self-hosted protocol is easiest to inspect and the
   model family exposes the widest behavioural range. **Done.**
4. OpenAI Responses adapter and API-key authentication. **Done.**
5. Anthropic Messages adapter and API-key authentication. **Done.**
6. Approval UI and OS sandbox enforcement. **Approval done** in RFC-0006, along
   with the session log, cancellation and a one-shot `gyr run`. **The sandbox is
   not**, so the current boundary is a filesystem fence and a policy, not an
   operating-system boundary.
7. The structured `cargo` tool. **Done** in RFC-0008. General `exec` was
   deliberately deferred behind the sandbox rather than shipped beside it.
8. OS sandbox enforcement. **Done on macOS** in RFC-0009; Linux is unimplemented
   and fails closed rather than running unconfined. `exec` followed in RFC-0010,
   with argument vectors and no shell.
9. Rust diagnostic gate and representative eval corpus. **Gate done** in
   RFC-0011, which answers "rolls back or refuses" with refuses and says why.
   **Harness and a first two cases done** in RFC-0012. A corpus large enough to
   answer the open questions is still owed; two cases is a mechanism, not
   evidence.
10. Interactive terminal interface. **Done** in RFC-0007: line-based, so the
    transcript stays in the terminal's scrollback.
11. ChatGPT OAuth, after the transport is independently sound.
    **Moved 2026-10-19** behind RFC-0017 §4.2, which makes an open-weight
    model the primary target and the hosted profiles its baselines.

Each stage must produce a working vertical slice. A directory full of interfaces
whose behaviour is promised for later is not progress, although it can look
very handsome in a tree listing.

## 11. Open questions

- Whether the first patch primitive should be unified diff or exact
  search/replace plus an internal diff. Both must be evaluated against the
  target model family.
- Whether provider-native parallel tool calls should execute concurrently by
  default when all calls are read-only.
- Which sandbox substrate gives the smallest honest macOS/Linux MVP without
  delegating the security boundary to shell quoting.
- Whether Qwen3.6 vision input belongs in the MVP. Model support does not by
  itself establish product need.
