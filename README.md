# Gyrfalcon

Gyrfalcon is an open-source terminal coding agent written in Rust and designed
for Rust work. It is built around open-weight models: by default Qwen3.8 27B,
called through Alibaba Cloud Model Studio, whose Apache-2.0 weights anyone can
serve on their own hardware instead. Hosted Claude models are kept as the
baseline the open model is measured against. The catalogue is deliberately
bounded, rather than assuming that every model behaves like one API wearing a
false moustache.

**The default has not yet completed a single turn.** Every measurement in this
README is of Claude. Requests to Qwen3.8 27B on Model Studio on 2026-10-19
reached Alibaba and were refused (`403 AccessDenied.Unpurchased`): the author's
new account was held by Alibaba's risk control, so the model had no chance to
answer. Until it has passed the eval corpus, treat the default
as a stated intention, not as a recommendation. RFC-0017 is the plan.

It is also a serious educational implementation of the machinery behind tools
such as Claude Code and Codex, published with its design record, its failures
and its measurements. Several of those measurements went against the design, and
those are written down too.

## Status

`gyr` opens an interactive session. It selects a model, builds a provider-native
session, streams each turn, classifies every tool call, enforces an approval
policy in code, runs processes inside an operating-system sandbox, records
everything to an append-only log, and cancels a turn on Ctrl-C without inventing
a terminal event the provider never sent or losing the conversation.

`gyr run` is the same machinery as one submission and an exit code, for scripts,
CI and evals.

### What works

- **Three native transports.** OpenAI Responses, Anthropic Messages and Qwen
  Chat Completions, each keeping its own conversation state so continuation does
  not lose reasoning items or content ordering. Recorded parser tests
  throughout, and a cross-provider conformance suite drives all three over a
  real socket and asserts the same normalised events from each: three wires, one
  semantics. Only Anthropic has been driven against a live endpoint, so what the
  suite proves is that the adapters agree, not that a provider accepts what they
  send. In particular, nobody yet knows whether Model Studio honours the
  thinking control, which the Qwen adapter sends in vLLM's form.
- **Six tools.** `read` returns bounded, numbered, fingerprinted ranges.
  `search` is literal and ignore-aware with explicit totals. `list` shows what is
  in the workspace through the same ignore rules, with `all` to see what those
  rules hide. `apply_patch` replaces one exact occurrence and refuses a stale or
  ambiguous edit. `exec` runs one program with one argument vector, no shell.
  `cargo` runs a closed set of subcommands and returns parsed diagnostics, so an
  `E0308` arrives as a level, a code, a file, a line and a column rather than as
  a page of compiler output.
- **Approval enforced below the model.** Every call is classified read-only,
  mutating or process. Read-only calls proceed. Anything else asks, and a session
  rule is keyed on the tool and the resolved target, never on a description of
  what a command probably does. A refusal returns to the model as an ordinary
  tool result under its original call ID.
- **A line-based session.** Not an alternate-screen interface, deliberately: the
  transcript belongs in the terminal's scrollback, where a person goes to find
  what the agent did an hour ago and where selection and copying still work.
  Paste arrives as one message; a trailing backslash continues a typed line.
- **Resumable sessions.** `gyr --resume` continues the most recent conversation
  in the workspace, or a named one. What is persisted is the adapter's own native
  history rather than the session log, because the log holds normalised events
  and rebuilding a provider's content ordering from those would reconstruct
  something plausible rather than something identical. A state from another
  provider, model or payload version is refused rather than half-loaded.
- **A context budget.** A session used to grow until the provider refused the
  request and then keep refusing, without saying why. It now watches reported
  input tokens against the model's documented window, says so once at 70%, and
  past 85% hollows out the oldest tool results, keeping the calls, the text and
  every call ID, with a marker naming what went. Elision rather than
  summarisation: no extra request, deterministic, and it does not quietly rewrite
  what the model believes happened.
- **A session log, and a way to read it back.** Append-only JSONL holding what
  you asked, the proposed action, the decision, the execution and the result.
  `gyr replay` renders a past session through the same renderer the live one
  uses, with `--last N` for the recent exchanges.
- **Configuration, split by who wrote it.** `~/.config/gyr/config.toml` for your
  defaults, `<workspace>/.gyr/config.toml` for a project's. The second arrives
  with the repository, so it may set preferences and may **not** set `approvals`,
  `sandbox` or `api_base`: the first two weaken a boundary and the third
  redirects where a credential is sent. A project file that tries is an error
  naming the file and the key. No file holds a credential at any layer.
- **An eval harness.** `gyr eval` runs a corpus of cases: a fixture workspace, a
  prompt, and assertions about the outcome. Assertions decide pass or fail;
  metrics decide nothing and are read back out of the session log, which is also
  how the log's sufficiency gets tested. `--without <tool>` hides a tool as
  completely as if it had never been built, which is how several of the findings
  below were obtained.

### What is enforced, and what is not

On macOS and Linux every process runs inside an operating-system sandbox that
confines writes to the workspace: Seatbelt on one, Landlock on the other, behind
one trait. **Neither confines reads.** A build script can still read a credential
file; what it cannot do is write it outside the workspace or transmit it. That
combination is the guarantee, and RFC-0009 section 2 explains why a narrower read
profile was rejected rather than attempted badly.

The two are not identical and are not labelled as though they were. Seatbelt
denies the network outright; Landlock ABI 4 denies TCP and leaves UDP open, so it
says `TCP denied` rather than `network denied`. Making the weaker one wear the
stronger one's words is the failure that document exists to avoid.

One consequence is load-bearing. RFC-0001 lists automatic commits, pushes,
deployments and purchases as non-goals. Under confinement `git push` fails with
`connect to host github.com port 22: Operation not permitted`, because the
sandbox refused the socket rather than because its name is on a list. There is no
list.

On every other platform the sandbox is unimplemented and Gyrfalcon refuses to run
any process rather than quietly running it unconfined. `--sandbox none` remains
available, is never the default, appears in the approval prompt as `unconfined`,
and is written into the session log. Windows is not a target.

### What does not exist yet

Shells and pipelines. Context relief for OpenAI, whose server-side continuation
keeps no local history to reduce, and which wants a live credential to verify
rather than merely to write. Summarising compaction. Rollback: the diagnostic
gate refused rather than reverting, because a shadow copy of the workspace is a
worse version of git. Resuming *from* a log as opposed to replaying one, which
RFC-0014 section 2 argues cannot be done honestly. A conformance run against a live
endpoint: the suite exists and has only ever seen recorded traffic. `gyr probe`,
which would let you run it against your own endpoint, is RFC-0017's next step.

These are missing features. None is delegated to the system prompt and none is
claimed to be present.

## What the corpus has changed

The eval corpus is nine cases, which is a start rather than a corpus. All nine
pass against `claude-sonnet-5`. It exists to answer questions this repository
would otherwise settle by argument, and it has now overruled the argument four
times.

**A missing capability, not a missing syntax.** The one `exec` call across six
cases was `find . -name "*.rs"`. Not a pipeline: a directory listing, which
nothing else offered. `list` was built on that, and re-running the same corpus
took `exec` to zero. A later run caught `exec find . -name .gyr`, because `list`
respects ignore rules and so cannot show what they hide; it now takes `all`.

**`search` earns its place, and only at scale.** Withheld on a ten-file crate it
cost nothing, which was a finding about the corpus rather than about search.
Withheld on a twenty-nine-module one it cost 41% more tokens and the model
reached for `exec grep`.

**The diagnostic gate was built, argued for, and withdrawn.** It tracked whether
an edit batch was converging. Two ablations at two workspace sizes found it cost
turns and tokens and changed no outcome, because `cargo check` already names the
failing files and so its verdict added nothing about *where*. On the case built
to be its best scenario, removing it made the run two turns and 8,560 tokens
shorter. The code and RFC-0011 stay; it is not in the tool set.

**A tool surface has a price.** A leaner one cost 26% fewer input tokens for
identical outcomes, well outside the measured 6.6% run-to-run noise floor.

**Two models solve it identically on paper and differently in fact.** Sonnet and
Opus both pass nine of nine. On the case built so that reading everything is
expensive, Opus read all thirty-one files and Sonnet read five, for the same
correct result and 2.6 times the tokens; across the corpus Opus cost $2.04 to
Sonnet's $0.58. Every assertion missed that difference and the metrics carried
it, which is the clearest argument yet for keeping the two apart.

Two rules the corpus applies to itself. A case where nothing changed fails even
if the code compiled, unless its deliverable is an answer rather than an edit.
And cases run unattended, therefore inside the sandbox, because unattended and
unconfined is the worst combination here.

## Models

The catalogue is explicit data rather than inference from a URL or a model-name
substring. An adapter may expose less than a model can do and never more.

| Key | Provider | Model | Status |
|---|---|---|---|
| `qwen3.8-27b` | Qwen Chat Completions | `qwen3.8-27b` | **default**, development |
| `terra` | OpenAI Responses | `gpt-5.6-terra` | development |
| `claude-opus` | Anthropic Messages | `claude-opus-5` | supported |
| `claude-sonnet` | Anthropic Messages | `claude-sonnet-5` | supported |
| `qwen3-coder-480b-a35b` | Qwen Chat Completions | `Qwen/Qwen3-Coder-480B-A35B-Instruct` | development |
| `qwen3-coder-next` | Qwen Chat Completions | `Qwen/Qwen3-Coder-Next` | development |
| `qwen3-coder-30b-a3b` | Qwen Chat Completions | `Qwen/Qwen3-Coder-30B-A3B-Instruct` | development |
| `qwen3.6-27b` | Qwen Chat Completions | `Qwen/Qwen3.6-27B` | development |
| `qwen3.6-35b-a3b` | Qwen Chat Completions | `Qwen/Qwen3.6-35B-A3B` | development |
| `qwen3-8b` | Qwen Chat Completions | `qwen3:8b` | development, not a coding target |

`supported` means the corpus has passed against a live endpoint, which so far is
true of the two Claude profiles and nothing else. The rest have passed the wire
conformance suite and no live run; RFC-0017 says what promotes them.

`qwen3.8-27b` is the default because it has open weights and a hosted endpoint,
and a single profile serves both. With no endpoint configured it calls Model
Studio's international (Singapore) endpoint with `QWEN_API_KEY`. With
`--api-base`, `QWEN_API_BASE` or `api_base` in the user config file, it calls
that endpoint instead and sends the same model name, so a self-hosted server must
serve the weights under the name `qwen3.8-27b`. Its parsers, sampling and thinking
behaviour are left unset in the catalogue until something has been measured.

Qwen's reference serving stacks are vLLM and SGLang. Ollama and other local
runtimes may follow once they pass the same conformance suite. `qwen3-8b` is
present so the loop can be driven by real inference on a laptop; it is not a
coding target and never will be.

## Using it

```console
cargo install --path crates/gyr-cli --locked
```

That installs two binaries: `gyr`, and `gyr-confine`, which is the Linux sandbox
helper. They must sit together, because `gyr` looks for the helper beside itself;
`cargo install` puts both in the same directory, and `GYR_CONFINE` overrides for
a packager who does otherwise. Without the helper, Linux refuses to run any
process rather than running it unconfined.

```console
gyr models
gyr prompt --model claude-sonnet
```

The default needs a Model Studio API key from the Singapore region, on an
account that can call Qwen3.8 27B. It has no free quota and is billed per
token. A key that cannot call it gets `403 AccessDenied.Unpurchased` on the
first turn, whatever the reason the account cannot.

```console
export QWEN_API_KEY=...
gyr
```

To serve the same model yourself, give the server the name Gyrfalcon sends and
point `gyr` at it. **Not yet tried:** this is the documented vLLM flag, not a
tested quickstart, and RFC-0017 owes one per hardware tier.

```console
vllm serve Qwen/Qwen3.8-27B --served-model-name qwen3.8-27b
gyr --api-base http://localhost:8000/v1
```

The other profiles need their provider's credential: `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, or `QWEN_API_BASE` for the Qwen profiles without a hosted
endpoint. A missing one fails before any request is sent, naming the variable.

```console
export ANTHROPIC_API_KEY=...
gyr --model claude-sonnet
```

Or put the model in `~/.config/gyr/config.toml` and run `gyr`. `gyr config`
prints every setting and where its value came from.

Inside a session: `/help`, `/status`, `/log`, `/exit`. Ctrl-C cancels the current
turn and keeps the conversation; Ctrl-D leaves. Paste a stack trace and it
arrives as one message; end a typed line with a backslash to carry on. History
persists to `.gyr/history` and the conversation to `.gyr/sessions`, so
`gyr --resume` picks it up where you left it and `gyr replay` shows what
happened.

Mutations and processes ask before they happen. `--read-only` refuses both
instead; `--dangerously-allow-all` does not ask, which is a decision worth making
deliberately. A sandboxed run has no network, so Cargo runs `--offline` and
cannot fetch a dependency that is not already in the local registry cache: adding
a crate is a job for a person, or for `--sandbox none`.

For a script, CI or an eval, one submission and an exit code:

```console
gyr run --model claude-sonnet "what does gyr-core::Agent::run guarantee?"
```

Against a local endpoint:

```console
gyr --model qwen3-8b --api-base http://thinkpad.local:11434/v1 \
    --no-thinking --read-only
```

Ollama serves a context far smaller than the model's native window unless
`num_ctx` is raised, and it truncates rather than complaining. RFC-0003 section
3.1 records what has and has not been measured there.

The interface uses a warm dark palette because that is what it was drawn for.
Gyrfalcon ships the ink and leaves the background to the terminal, since a
line-based program does not own its ground. On a light terminal, `--plain`.

**What lands on disk.** `.gyr/` in the workspace holds the session log, the
resumable state and the input history. The log records what you typed; the state
holds the conversation, and the conversation holds your source, because tool
results are file contents. It is git-ignored and mode 0600, and it is worth
knowing it is there.

The command is `gyr`; Gyrfalcon is the project.

## Design record

- [RFC-0001: Architecture](docs/rfcs/RFC-0001-architecture.md)
- [RFC-0002: Predecessors and public agents](docs/rfcs/RFC-0002-predecessors.md)
- [RFC-0003: Provider protocol](docs/rfcs/RFC-0003-provider-protocol.md)
- [RFC-0004: Local subscription model probes](docs/rfcs/RFC-0004-local-model-probes.md)
- [RFC-0005: Workspace filesystem tools](docs/rfcs/RFC-0005-filesystem-tools.md)
- [RFC-0006: Approvals, session log and the first interactive run](docs/rfcs/RFC-0006-approvals-and-the-first-run.md)
- [RFC-0007: The interactive session](docs/rfcs/RFC-0007-interactive-session.md)
- [RFC-0008: The structured Cargo tool](docs/rfcs/RFC-0008-cargo-tool.md)
- [RFC-0009: Operating-system sandbox](docs/rfcs/RFC-0009-sandbox.md)
- [RFC-0010: Process execution](docs/rfcs/RFC-0010-exec.md)
- [RFC-0011: The Rust diagnostic gate](docs/rfcs/RFC-0011-diagnostic-gate.md)
- [RFC-0012: The eval corpus and harness](docs/rfcs/RFC-0012-eval-harness.md)
- [RFC-0013: The context budget](docs/rfcs/RFC-0013-context-budget.md)
- [RFC-0014: Resuming a session](docs/rfcs/RFC-0014-session-resumption.md)
- [RFC-0015: Configuration files](docs/rfcs/RFC-0015-configuration.md)
- [RFC-0016: Replaying a session](docs/rfcs/RFC-0016-replay.md)
- [RFC-0017: Open-weight first](docs/rfcs/RFC-0017-open-weight-first.md)

The RFCs are part of the project. Findings are labelled as measured, observed in
source, documented by a provider, or inferred. Quantitative claims carry a date
and method, because agent software changes too quickly for folklore to be a sound
dependency. Where a design was wrong and had to be corrected, the RFC records the
correction rather than quietly reading as though it were right all along.

## Development

```console
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
gyr eval --model claude-sonnet   # needs a credential; not part of cargo test
```

CI runs the first three on macOS and Linux against a pinned toolchain, and
separately installs the binaries and has the helper refuse a write outside a
workspace. It has now caught two defects invisible to the ordinary test run: a
macOS-only helper left undead on Linux, and a sandbox helper that `cargo install`
did not deliver at all.

Nine crates:

- `gyr-protocol` — values crossing crate and frontend boundaries.
- `gyr-model` — provider session traits and the explicit model catalogue.
- `gyr-core` — the act-observe loop, approval, the session log, the workspace
  fence, the context budget and the system prompt.
- `gyr-tools` — workspace filesystem tools and their hard output limits.
- `gyr-sandbox` — operating-system containment. Rewrites a command; never spawns.
- `gyr-exec` — the process runner and the `exec` tool.
- `gyr-rust` — the `cargo` tool, diagnostic parsing, and the withdrawn gate.
- `gyr-eval` — the case format, the runner, the metrics read from a log, and the
  one definition of what tools a session gets.
- `gyr-cli` — the `gyr` executable, its session loop, renderer, palette and
  approval prompt, and the `gyr-confine` helper that restricts itself with
  Landlock and then `exec`s.

Tests that invoke a real `cargo` build a dependency-free fixture in a temporary
directory, so they need a toolchain but no network. The six confinement tests run
on both macOS and Linux, including the pair that rules out a sandbox which simply
refuses everything.

## Licence

MIT.
