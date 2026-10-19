# RFC-0017: Open-weight first

|            |                                                        |
| ---------- | ------------------------------------------------------ |
| Status     | proposed                                               |
| Date       | 2026-10-19                                             |
| Depends on | RFC-0001, RFC-0003, RFC-0012, RFC-0013                 |
| Amends     | RFC-0001 §1, §3 and §10; RFC-0003 §3.1; RFC-0012 §10   |
| Scope      | product position, serving stacks, model tiers, harness |

## 1. Decision

Gyrfalcon becomes an open, hobbyist alternative to Claude Code whose primary
target is an open-weight model on hardware one person owns or rents by the hour.
The Qwen family in RFC-0003 is that target. The hosted Anthropic and OpenAI
profiles stay, but their role changes: they are **baselines**, the thing an
open model is measured against, rather than the thing the product is built
around.

"Alternative" means a person can do the everyday work they would otherwise do
in Claude Code (read a repository, ask about it, make multi-file changes, run
the build and tests, iterate on compiler output) with no subscription, no
account and no data leaving machines they control. It does not mean feature
parity. Section 4 says which features are in, and section 5 says which are not
and why.

The project remains Rust-first, educational and honest about what it has and
has not measured. Nothing in this RFC relaxes RFC-0001 §8. Safety is still code.

## 2. Why now

Three things have changed since RFC-0001 put three providers side by side.

1. **The loop works.** The act-observe loop, six tools, approvals, the sandbox,
   the session log and the eval harness exist. The remaining work is mostly
   about which models it is good with, which is a question the project is now
   equipped to answer by measurement rather than argument.
2. **Every number the corpus has produced is a Claude number.** Nine of nine
   cases pass on `claude-sonnet-5` and `claude-opus-5`. The Qwen adapter has
   passed the cross-provider conformance suite (RFC-0003 §7.1), which proves the
   wire and not the model: it has **never been driven against a live endpoint**.
   Even so, five Qwen profiles, and the OpenAI profile `terra`, are marked
   `supported` in the catalogue (observed in source, `gyr-model/src/lib.rs`,
   2026-10-19), which RFC-0003 §3.1 says requires a completed behavioural eval. The profile the project describes
   as its widest behavioural range is the one with the least evidence, and the
   catalogue currently claims otherwise.
3. **Honeyguide measured the failure modes.** The sibling project ran a
   ~3B-active Qwen MoE against a real 14.7k-line Rust workspace and found
   problems that are properties of open models and their serving stacks rather
   than of honeyguide's design: silent context truncation, fabricated edit
   anchors, stalling on re-reads, no-op edits passing a compile gate, and Q3
   quantisation degrading tool calls before chat. Those findings transfer.
   Section 7 imports the ones that are deterministic and cheap.

## 3. Who this is for

A hobbyist here is someone with one of the following, and no budget for a
per-seat subscription:

| Tier | Hardware, roughly                                   | Example profiles                                        |
| ---- | --------------------------------------------------- | ------------------------------------------------------- |
| A    | rented multi-GPU node, by the hour                  | `qwen3-coder-480b-a35b`, `qwen3-coder-next`             |
| B    | one 24 to 48 GB GPU, or 64 GB+ unified memory       | `qwen3.6-27b`, `qwen3.6-35b-a3b`, `qwen3-coder-30b-a3b` |
| C    | 8 to 16 GB GPU laptop, MoE experts offloaded to CPU | `qwen3.6-35b-a3b`, `qwen3-coder-30b-a3b` at Q4_K_M      |

**Inferred, not measured by Gyrfalcon.** The tier boundaries are estimates from
model sizes and quantisation, not runs. The only throughput figure the
project family has for tier C is honeyguide's: about 359 tok/s prefill and
25 tok/s decode for `Qwen3.6-35B-A3B` Q4_K_M on Ollama, measured 2026-08-12
with a different harness. It is cited here as an order of magnitude and for
one consequence: decode costs about fourteen prompt tokens per generated token,
so on tier C the binding cost is what the model writes, not what it reads.

Tier A is where open models are most likely to match the hosted baselines. Tier
C is where most hobbyists are. The product has to be honest about the gap
between them rather than average it away.

## 4. What "alternative" includes

The bar is the work, not the brochure. Each item below is something a person
would miss within the first hour of switching.

### 4.1 Already present

- A streaming interactive session with cancellation and resumption.
- Bounded reads, literal search, ignore-aware listing, exact patching.
- Structured `cargo` with parsed diagnostics.
- Approvals below the model and an OS sandbox on macOS and Linux.
- An append-only log with replay, and a one-shot `gyr run` for scripts and CI.

### 4.2 Required for this RFC to be accepted as done

1. **Project instructions.** Read `AGENTS.md` at the workspace root, and
   `.gyr/instructions.md` if present, into the system prompt under a hard byte
   cap that is reported when it truncates. `AGENTS.md` rather than a new name,
   because the repositories hobbyists already have use it. A project file is
   repository content and may not change approvals, sandbox or `api_base`, as
   RFC-0015 already requires.
2. **`gyr probe`.** A command that runs the conformance suite against the
   user's own endpoint, not a mock. It reports, per check, pass or fail with
   the evidence:
   - the served context window, measured by a canary near the declared limit,
     not read from documentation;
   - whether the tool parser is active, by requesting one tool call and
     inspecting whether it arrived structured or as text in `content`;
   - whether thinking controls are honoured;
   - whether the system prompt and tool schemas arrived whole;
   - decode and prefill throughput, warmed, three trials, all reported.
   A profile is usable against an endpoint only after `gyr probe` passes there.
   This turns RFC-0003 §7's "a real model run must complete the behavioural
   evals" into something a user can run, rather than something only the
   maintainer can.
3. **Served context, not native context.** RFC-0013's budget watches reported
   tokens against the documented window. For open models the documented window
   is a ceiling the server may not offer. The budget uses the smaller of the
   documented window and the window `gyr probe` measured, and says which.
4. **A quickstart per tier.** One page each: the exact serving command, the
   parser flags RFC-0003 §6 requires, the expected `gyr probe` output, and what
   it costs (watts, or dollars an hour). A hobbyist who cannot get a model
   serving correctly in twenty minutes is not a hobbyist who will stay.

### 4.3 Planned, in order, each behind its own RFC

1. **Plan mode.** `--read-only` already exists. Plan mode is read-only plus a
   required final artefact: a numbered plan the user accepts, edits or rejects
   before mutations are allowed. Smaller models benefit most from separating
   deciding from doing.
2. **Hooks.** Commands run after an edit or before a turn ends, configured in
   user config only, executed through the same sandbox and approval path as
   `exec`. The first use is `cargo fmt` after edits.
3. **A `rename` tool for Rust.** Exact reference sets from `rust-analyzer scip`,
   applied only after each site is verified against current file contents.
   Honeyguide found that a lexical reference set for a common name is not
   approximately right but wrong (84 sites across 15 files, none of them the
   target), and that the SCIP set matched the compiler's errors site for site.
   No model of any size should be asked to do that job by hand.
4. **MCP, stdio only.** A local MCP client for servers the user starts. Every
   MCP tool is classified like any other tool, defaults to mutating, and runs
   inside the sandbox. Remote MCP and catalogues remain out (section 5).
5. **Summarising compaction**, as an explicit, logged event, and only after
   section 8.4 is answered.

## 5. What it deliberately excludes

These stay out, and the reason is stated so the next person does not have to
rediscover it.

- **Subagents and multi-agent orchestration.** Not until a single agent on a
  tier B model passes the corpus. Parallelism multiplies whatever the single
  loop does, including its failures.
- **Cloud sessions, a desktop app, IDE extensions.** Each is a product of its
  own. The terminal is the product here.
- **A plugin or MCP marketplace.** A catalogue is a supply chain. Gyrfalcon
  does not want to operate one.
- **Arbitrary OpenAI-compatible endpoints as a supported target.** The envelope
  is compatible; the behaviour is not (RFC-0003 §6). An unlisted model can be
  run under a `development` profile and `gyr probe`, never as `supported`.
- **Reusing a Claude or ChatGPT consumer subscription**, as RFC-0001 §3 already
  says, until the provider explicitly supports third-party clients doing so.
- **Windows**, per RFC-0001 §8.

## 6. Serving stacks

RFC-0003 names vLLM and SGLang as reference servers and keeps Ollama out until
it passes conformance. Hobbyists overwhelmingly run Ollama or `llama.cpp`'s
server. This RFC keeps the rule and changes how it is applied.

| Stack            | Status after this RFC | Condition                                                 |
| ---------------- | --------------------- | --------------------------------------------------------- |
| vLLM             | supported             | RFC-0003 §6 parser flags, `gyr probe` and the corpus pass |
| SGLang           | supported             | as vLLM                                                   |
| llama.cpp server | development           | promoted per profile when `gyr probe` and the corpus pass |
| Ollama           | development           | as llama.cpp, and `num_ctx` set explicitly                |

`supported` and `development` are the `ProfileStatus` values from RFC-0003
§3.1, now carried per (profile, stack) pair rather than per profile. A Qwen
model can be supported on vLLM and development on Ollama at the same time,
because it is the same weights behind a different contract. "Supported" on a
stack is a condition each pair must meet, not a property the stack confers:
until a pair has passed, it is `development`, including on vLLM.

Two known hazards are restated because they fail silently:

- **Ollama truncates rather than refusing** when the prompt exceeds `num_ctx`,
  and the default is far below the native window. `gyr probe` must detect this.
  Gyrfalcon refuses to start a session against an Ollama endpoint whose measured
  window is smaller than the system prompt plus tool schemas plus a working
  margin, and says so with the numbers.
- **Quantisation below Q4_K_M** degrades tool calling before chat (measured in
  honeyguide). Profiles declare a minimum quantisation; `gyr probe` reports the
  served one where the stack exposes it and warns where it cannot tell.

## 7. Harness changes for open models

Each item is deterministic, costs no model tokens and is cheap to test. Each is
imported from a honeyguide finding and must be re-measured here before its
effect is claimed.

1. **Refuse no-op edits.** An `apply_patch` whose replacement is byte-identical
   to its target is rejected as an ordinary tool error. Honeyguide recorded two
   of five tasks in one trial "passing" this way.
2. **Detect stalls.** A `read` of a range already returned this turn, with the
   file unchanged since, returns a short result saying so instead of the
   contents again. The model's failure mode on honeyguide's driver was not
   fabrication but re-reading; refusing it is deterministic.
3. **Measure the tool surface per profile.** The corpus showed a leaner surface
   cut input tokens by 26% for identical outcomes on Sonnet (RFC-0012 §9.9).
   Smaller models are likelier to degrade past a handful of tools. Profiles may
   declare a reduced tool set, and the eval harness's `--without` already
   supports measuring it.
4. **Bound output, not input.** Per profile, cap `max_tokens` per turn at a
   value the corpus justifies, and keep tool results bounded as now. On tier C,
   a runaway generation is the most expensive event in the system.
5. **Keep the prefix stable.** vLLM, SGLang and llama.cpp all reuse a cached
   prompt prefix. Anything that rewrites early history invalidates it. Elision
   under RFC-0013 does exactly that. Section 8.4 is about this trade.
6. **Profile sampling stays per profile and per mode**, as RFC-0003 §6 already
   requires. Not new; restated because a shared default is the easiest way to
   make a Qwen model look worse than it is.

**Not adopted:** honeyguide's constrained JSON action mode and offline strong-
model index. Both exist because a ~3B-active model cannot drive native tools.
The tier B and C models in section 3 are expected to, and the point of this RFC
is to find out whether they do. If the corpus says they do not, a constrained
mode becomes a profile option in a later RFC, not a fork.

## 8. Evaluation and acceptance

### 8.1 What has to be true

This RFC is done when, for at least one tier B profile on a supported stack:

- `gyr probe` passes;
- the corpus has grown to at least twenty cases, including those in
  section 8.2;
- every case has run at least ten trials, with a Wilson interval printed beside
  every rate;
- the same corpus has run against `claude-sonnet-5` at the same trial count, as
  the baseline.

**Proposed threshold, to be argued in review:** the tier B profile is called a
working alternative when the lower bound of its interval for completion is at
least 50% overall and its interval overlaps the baseline's on single-file
cases. Multi-site cases are reported separately and do not gate acceptance,
because the honest expectation is that they will lag.

### 8.2 What the new cases must be able to ask

Twenty cases that are all as easy as the current nine would measure only that
open models can do easy things. The growth has to include cases that answer the
questions RFC-0011 and RFC-0012 left open:

1. **At least four multi-site edits**, which are the cases honeyguide found no
   small model could complete.
2. **At least one workspace too large to read whole.** RFC-0012 §9.9 found
   `search` and `list` cost tokens and changed no outcome, on a corpus where
   reading everything was always an option. Whether either earns its place
   cannot be asked until it is not. This matters more for open models than for
   the baselines, because the served window is smaller.
3. **At least one case that goes red partway through**, in a way the model
   cannot read ahead. The open question beneath the withdrawn gate (RFC-0011
   §12) is whether a mid-task compiler verdict ever changes a decision. No case
   yet produces one.
4. **Cases that discriminate the baselines on assertions.** RFC-0012 §9.12
   found Opus and Sonnet indistinguishable on pass and fail and different only
   in metrics. A corpus that cannot separate two hosted models will not
   separate a hosted model from an open one in any way worth reporting. Harder
   cases are tried before a judge model.

### 8.3 What the corpus must report beyond pass and fail

Tokens in and out, turns, wall time, and dollars or estimated watt-hours per
case. RFC-0012 already keeps metrics out of the pass decision. For a hobbyist,
"passes but takes forty minutes on my laptop" is a different product from
"passes in four", and the corpus should be able to say which one it is.

It follows that each case needs a **wall-time limit distinct from its turn
limit** (RFC-0012 §10). A tier C model can spend many minutes inside one turn,
and a limit counted in turns will not stop it. A case that hits the wall-time
limit is a failure with its own reason, reported as such rather than folded
into "did not pass".

### 8.4 Open measurement

Whether elision (RFC-0013) or summarising compaction is the better trade on a
served window of 32k to 64k tokens, given that both break prefix caching and
summarising also adds a request. This is measured before §4.3 item 5 is built,
not argued.

## 9. Amendments

- **RFC-0001 §1.** The second paragraph's "three provider families" are kept.
  The primary target is the Qwen family; the others are baselines.
- **RFC-0001 §3.** "Arbitrary compatible APIs" stays a non-goal for
  `supported`. Running one under a `development` profile with `gyr probe` is
  allowed.
- **RFC-0001 §10.** Item 11, ChatGPT OAuth, moves behind every item in §4.2 of
  this document.
- **RFC-0003 §3.1.** `ProfileStatus` is keyed on (profile, serving stack).
  Every Qwen profile is `development` until it meets section 6's condition on
  some stack. This corrects the catalogue as it stands, which marks five of
  them `supported` without a live run. `terra` had the same defect for a
  different reason (RFC-0003 §7.1: no credential to verify it) and is demoted
  with them; it returns when a live corpus run passes.
- **RFC-0012 §10.** The questions about a per-case timeout, a corpus that
  discriminates on assertions, and whether `search` and `list` earn their place
  on a large workspace are taken up by section 8 of this document.

## 10. Build order

1. Demote the five Qwen profiles and `terra` to `development`, so the
   catalogue stops claiming what nothing has measured. **Done 2026-10-19.**
2. `gyr probe`, against a vLLM endpoint serving `qwen3-coder-30b-a3b`. Early,
   because every later step depends on knowing the endpoint is what it claims.
3. Served-context budget (§4.2 item 3).
4. Harness support for repeated trials with Wilson intervals, and a per-case
   wall-time limit (§8.3). Without both, a ten-trial run on tier B hardware is
   either unreadable or unbounded.
5. No-op edit refusal and stall detection (§7 items 1 and 2), with corpus cases
   that fail without them.
6. `AGENTS.md` instructions.
7. First live Qwen corpus run at ten trials, published with all results,
   including the bad ones.
8. Corpus growth to twenty cases, meeting §8.2; baseline rerun.
9. Quickstarts for tiers A, B and C, each tested from a clean machine.
10. llama.cpp and Ollama `gyr probe` runs, and promotion of whatever passes.
11. Plan mode, hooks, `rename`, stdio MCP, each with its own RFC.

As in RFC-0001: each step is a working vertical slice. A quickstart nobody has
followed from a clean machine is a hypothesis with formatting.

## 11. Risks

- **Open models may simply not be good enough on tier C.** That is a result,
  not a failure of the RFC. It is published as measured and the README says
  which tier the project recommends.
- **Serving misconfiguration will look like model weakness.** Honeyguide's
  first behavioural readings were dominated by harness and serving defects, five
  of seven of which made the model look worse than it was. `gyr probe` exists to
  separate the two. Until it passes, no model comparison is reported.
- **Offline sandboxed cargo is hobbyist friction.** A sandboxed run has no
  network, so adding a crate needs `--sandbox none` or a person. Landlock
  cannot express per-host rules, so "allow crates.io only" is not available on
  Linux. This stays a known cost rather than a weakened boundary.
- **Numbers decay.** Model releases, serving stacks and quantisations move
  monthly. Every quantitative claim carries a date and method, as the rest of
  the design record does, and `gyr probe` output is the thing to trust over any
  table in this document.

## 12. Open questions

- Whether `AGENTS.md` should be read from subdirectories as the agent enters
  them, or only at the root. Root only is simpler and easier to cap.
- Whether plan mode belongs to the core loop or to the CLI. It changes what
  the approval layer allows, which argues for core.
- Whether non-Rust repositories are in scope beyond the language-neutral tools.
  The current position is that they work and are not specialised for. A
  hobbyist audience will ask for Python and TypeScript diagnostics; the answer
  should come from demand measured in issues, not from symmetry.
- Whether a tier C profile should default to a reduced tool set before the
  corpus has said so, or only after.
- What the README says Gyrfalcon *is* once this lands. Proposed: "an open
  coding agent for open models, that does less than Claude Code and tells you
  exactly what it did."
