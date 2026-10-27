# RFC-0002: Predecessors and public coding agents

| | |
|---|---|
| Status | research record |
| Date | 2026-08-23 |
| Sources | Ward, Honeyguide, Codex, Claude Code documentation, Qwen Code |

## 1. Method and evidence labels

This document records what DanchoAI is taking from earlier work and what it is
refusing to repeat. Findings use four labels:

- **Measured**: produced by a recorded experiment with method and result.
- **Observed in source**: read in the named repository at the revision below.
- **Provider-documented**: stated by the provider's current public docs.
- **Inference**: an architectural conclusion drawn from the preceding evidence.

Source revisions inspected on 2026-08-23:

- Ward `38b80be` (`sagelang/ward`, `main`).
- Honeyguide `52a0be5` (`cargopete/honeyguide`, `main`).
- OpenAI Codex, shallow `main` checkout current on the inspection date.
- Qwen Code, shallow `main` checkout current on the inspection date.
- Anthropic's `anthropics/claude-code` public repository. This contains product
  documentation, examples, plugins and issue machinery, but not the complete
  Claude Code implementation. No source claim below pretends otherwise.

## 2. Ward

Ward is a small agent written in Sage. It demonstrates that the essential loop
is not large: gather project context, request one action, execute it, append the
observation and repeat.

### Findings retained

**Observed in source:** Ward has a hard step cap, one action per model response,
bounded reads, gitignore-aware search, read-before-edit, unique-match editing,
visible diffs, confirmation for writes and commands, an inspectable command
allow-list, and transcript compaction.

**Observed in source:** Whole-file writes over existing files are refused until
the file has been read. Empty writes are refused. Edits are confined to the
project by path checks, with the source honestly describing that check as a
fence rather than a sandbox because it does not resolve symlink escapes.

**Inference:** These simple deterministic contracts carry more safety and
reliability than elaborate prompting. DanchoAI keeps them, enforced in Rust
below the provider layer.

### Limits not carried forward

**Observed in source:** The entire model protocol is an inline prompt and a
custom XML-like tag parser because Sage's `divine` primitive accepts only a
string-literal template. This makes provider behaviour, prompt construction and
tool decoding difficult to test independently.

**Observed in source:** A single persistent string transcript serves model
context, restart state and user-visible history. Compaction rewrites that shared
representation.

**Inference:** DanchoAI separates provider-native history, normalised events
and presentation. It also treats malformed or incomplete tool output as a
protocol error rather than silently interpreting ordinary prose as completion.

Ward: <https://github.com/sagelang/ward>

## 3. Honeyguide

Honeyguide explores a different thesis: a strong model builds a semantic map
offline, then a small local model performs narrow edits behind deterministic
gates. Its Rust crates remain largely data models; its Python spike and recorded
measurements contain the useful behavioural evidence.

### Findings retained

**Measured:** Schema-constrained output guaranteed shape but not truth. A model
returned a schema-perfect edit containing fabricated file contents. Therefore
tool arguments must be checked against disk and prior observations.

**Measured:** A gate which reverts every red edit rejects correct multi-site
Rust changes. Adding a struct field breaks every initializer until subsequent
edits repair them. Edits must be allowed to accumulate while the compiler's
diagnostic set improves.

**Measured:** Lexical propagation of a method rename selected 84 sites in 15
files, none of them references to the target method. rust-analyzer SCIP selected
three sites, exactly matching the compiler errors. Semantic reference identity
cannot be approximated by whole-word search.

**Measured:** A stale SCIP index pointed two lines away from current content.
Verifying the expected identifier at every current file location caused the
unsafe sites to be skipped. An index is a map, never authority for file bytes.

**Measured:** A green compile gate counted byte-identical replacements as task
success. A verifier must require a material diff and a task-specific oracle.

**Measured:** Three trials per configuration could not support claimed model or
harness comparisons. Honeyguide withdrew those comparisons and retained only
deterministic behavioural findings. DanchoAI's evals will report intervals and
separate debugging runs from comparative evidence.

**Measured:** Ollama timing probes taken after model unload folded load time into
prefill measurements, making throughput appear five to eight times worse and a
working prefix cache appear broken. Warm-up, explicit keep-alive, fresh inputs,
multiple trials and end-to-end timing are mandatory.

### Limits not carried forward

**Observed in source:** Honeyguide deliberately specialises the runtime loop for
small local models and uses a strong model only to prepare an index or handle an
escalation. DanchoAI instead supports both frontier and open-weight models as
first-class interactive providers.

**Inference:** The semantic index is valuable but not an M0 prerequisite. A
correct ordinary agent loop, tool boundary and eval corpus must exist before an
index is allowed to optimise them.

Honeyguide: <https://github.com/cargopete/honeyguide>

## 4. OpenAI Codex

### Findings retained

**Observed in source:** Codex separates a provider's configured metadata,
authentication, capabilities and API transport. Current provider capabilities
include upper bounds for hosted tools and remote compaction.

**Observed in source:** The sampling loop consumes a typed response-event
stream containing item lifecycle events, text and reasoning deltas, tool
argument deltas, usage, rate limits and a terminal completion. Tool futures can
run while later stream events continue arriving.

**Observed in source:** Context is append-only, bounded, normalised for missing
tool outputs and guarded against orphan outputs. The repository's own review
rules explicitly prohibit unbounded context items and history rewriting.

**Observed in source:** Authentication and sandboxing are substantial,
independent subsystems. The public code supports ChatGPT browser/device login
and API-key login. Filesystem and network permission policies are distinct.

**Inference:** The useful pattern is the evented core and separation of policy,
transport and execution. The current implementation's size and accumulated core
surface are a warning against beginning with every feature it now supports.

Codex: <https://github.com/openai/codex>

## 5. Anthropic Claude

**Provider-documented:** Client tools are expressed as `tool_use` blocks. The
client executes them and replies with `tool_result` blocks. A turn may contain
several tool calls.

**Provider-documented:** Tool results must immediately follow their tool-use
message, and result blocks must precede any ordinary text in the user content
array. Anthropic's tool history is therefore not interchangeable with an
OpenAI-style `tool` role.

**Provider-documented:** Anthropic publishes trained-in schemas for common
client tools, including shell and text editing. These should be tested against
DanchoAI's own schemas rather than copied on aesthetic grounds.

**Inference:** The Anthropic adapter must own message ordering and native
content blocks. The core should never be able to construct an invalid Claude
history by rearranging generic messages.

Sources:

- <https://platform.claude.com/docs/en/agents-and-tools/tool-use/how-tool-use-works>
- <https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls>
- <https://github.com/anthropics/claude-code>

## 6. Qwen Code and the Qwen models

**Observed in source:** Qwen Code supports several provider protocols and a
large amount of provider-specific normalisation, fallback and recovery logic.
Its central chat implementation has grown to several thousand lines and must
handle leaked tool tags, duplicate call IDs, orphaned tool results and provider
finish-reason differences.

**Provider-documented:** Local Qwen serving commonly exposes OpenAI-compatible
Chat Completions through vLLM or SGLang. Compatibility at the HTTP envelope does
not include Qwen reasoning fields or parser configuration.

**Provider-documented:** Qwen3-Coder models require the Qwen3-Coder tool parser.
Qwen3.6 models document the `qwen3` reasoning parser and `qwen3_coder` tool-call
parser. Qwen3-Coder-Next and Qwen3-Coder-30B-A3B-Instruct are non-thinking
models. Qwen3.6-27B and Qwen3.6-35B-A3B are reasoning-capable.

**Inference:** DanchoAI uses one Qwen transport implementation but explicit
model profiles. Parser, reasoning, context and sampling capabilities are data,
not conditionals scattered through the loop.

Sources:

- <https://github.com/QwenLM/qwen-code>
- <https://github.com/QwenLM/Qwen3-Coder>
- <https://huggingface.co/Qwen/Qwen3-Coder-Next>
- <https://huggingface.co/Qwen/Qwen3-Coder-30B-A3B-Instruct>
- <https://huggingface.co/Qwen/Qwen3.6-27B>
- <https://huggingface.co/Qwen/Qwen3.6-35B-A3B>

## 7. Conclusions

The agent loop is small. The difficult work sits at its boundaries:

- preserving provider-native continuation state;
- validating tool calls and matching results exactly;
- applying filesystem and process policy beneath the model;
- keeping context bounded without inventing history;
- distinguishing a compiling repository from a completed task;
- measuring stochastic systems without flattering them accidentally.

DanchoAI's architecture is arranged around those boundaries. Features that do
not strengthen one of them must justify their presence with a real task and an
eval.
