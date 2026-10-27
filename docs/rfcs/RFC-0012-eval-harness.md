# RFC-0012: The eval corpus and harness

| | |
|---|---|
| Status | implemented M6 |
| Date | 2026-08-27 |
| Depends on | RFC-0001, RFC-0006, RFC-0009, RFC-0011 |
| Scope | case format, the runner, what is asserted, what is merely measured |

## 1. Purpose

Three RFCs now end with an open question saying the corpus should answer it, and
there is no corpus. RFC-0010 wants to know how often the absence of pipelines
actually blocks a task. RFC-0011 wants to know whether `exhausted` at two checks
is right, whether coarse diagnostic identity hurts, and whether a model will
launder errors into `allow` attributes. Those are empirical questions and this
repository keeps saying so; it is time to be able to answer one.

## 2. Assertions decide, metrics inform

The single most important line in the design. A case has two kinds of output and
they must not be confused.

**Assertions** decide pass or fail. They are about the outcome and they are
deterministic: does the workspace compile, did the right files change, does the
result contain the thing it was asked for. A person can disagree with an
assertion by reading it.

**Metrics** decide nothing. They are counts recorded from the run: turns, tool
calls by name, refusals, gate verdicts, tokens, wall time. They are what the
open questions above are actually asking for, and they are useless as pass
conditions because a case that fails when a model takes seven turns instead of
six is measuring the weather.

A case that asserted "used the gate" would be prescribing method. Whether a
model reaches for the gate is a metric, and an interesting one.

## 3. Cases

A case is a directory holding `case.toml` and a `workspace/` tree:

```toml
name = "fix-two-type-errors"
prompt = "The crate does not compile. Fix it."
max_turns = 12

[expect]
cargo_check = "clean"
files_changed = ["src/lib.rs"]
files_unchanged = ["Cargo.toml"]

[[expect.contains]]
file = "src/lib.rs"
text = "-> u32"
```

`workspace/` is copied to a temporary directory per run, so a case is repeatable
and the corpus is never the thing being edited.

**`files_changed` must be non-empty for a case to pass**, and that is enforced
by the harness rather than left to each case to remember. RFC-0011 made "a green
build with no material diff is not task success" a verdict a model has to read;
here it is a rule the harness applies to itself, because a corpus that can pass
by doing nothing measures nothing.

## 4. The runner reads its own log

The harness runs a case, then computes every metric by reading the session log
that run produced. It does not instrument the agent.

This is deliberate on two counts. RFC-0006 said the log's first jobs were
debugging, approval audit and the eval corpus, and a claim like that should have
a consumer rather than an intention. And if the log is not sufficient to
reconstruct what happened, that is a defect in the log which the harness will
find, rather than one that hides behind a second parallel record.

## 5. Approvals and containment

An eval runs unattended, so there is nobody to approve anything. Cases run with
the allow-all policy.

**Unattended and unconfined is the worst combination in this project**, so
`dancho eval` requires a working sandbox and refuses to start without one. On a
platform where RFC-0009 has no implementation it refuses, and `--sandbox none`
is available for a person who has read this paragraph and decided anyway. It is
recorded in every case's log, as everywhere else.

## 6. Live and scripted

The harness takes a provider, and a provider need not be a vendor.

**Scripted** cases drive the harness with a provider that makes a fixed sequence
of moves. They are deterministic, need no credential, cost nothing, and are part
of `cargo test --workspace`. What they test is the harness: that a case which
fixes the code passes, that one which changes nothing fails even when the code
already compiled, that a case exceeding its turn limit fails, and that the
metrics read out of the log match the moves that were made.

**Live** cases run against a real model and are opt-in. They need credentials,
they cost money, they are not deterministic, and they are not part of
`cargo test`. They are where the open questions get their evidence.

The distinction matters because a harness whose own correctness depends on a
paid non-deterministic service cannot be trusted to report anything.

## 7. Command surface

```console
dancho eval [--model KEY] [--case NAME]... [--corpus DIR] [--json]
```

Cases run in sequence rather than in parallel. Parallel runs would share a
Cargo registry lock and a machine's cores, which would make the wall-time metric
meaningless and the diagnostics slower to arrive; a corpus of a dozen small
fixtures is not worth the concurrency.

The report names every case, its verdict, and the failing assertion where there
is one. `--json` emits the whole record including metrics, so a run can be
compared against a previous one without re-reading prose.

## 8. Out of scope

A judge model scoring the quality of a change, performance regression tracking,
a corpus large enough to be statistically meaningful, provider conformance cases
from RFC-0003 section 7, parallel execution, and any use of eval results to gate
a commit.

The judge deserves a note. "Did it compile and change the right file" is
checkable by a program; "is this good Rust" is not, and a model scoring another
model's work is a measurement instrument whose calibration nobody has. When the
corpus is large enough that the checkable assertions have stopped discriminating
between models, that is the moment to design one, and not before.

## 9. Verification

**Measured on 2026-08-27.** The workspace passes 119 tests and Clippy with
`-D warnings`. Eight of those are the harness's own, all scripted, none needing
a credential.

- A case that fixes the fixture passes, with `cargo check` run for real.
- A case whose model changes nothing fails, and the fixture chosen for that test
  already compiled, so only the harness's own rule could catch it.
- A case exceeding its turn limit fails, naming `model-turn limit reached
  after 2`.
- A case whose model edits a file declared unchanged fails, naming the file.
- Every metric asserted against the moves the script made, read out of the
  session log rather than out of the agent. That is simultaneously the test that
  the log is sufficient to reconstruct a run.
- A malformed `case.toml` refused with the file named; a case with no
  `workspace/` likewise; and the shipped corpus asserted to parse.

**Observed on 2026-08-27**, driving `dancho eval` against a local fake endpoint:
a passing case reporting `pass fix-type-errors  4 turn(s) · 156 ms ·
apply_patch x2, gate x1`, and a failing run reporting `nothing in the workspace
changed`, `src/lib.rs still contains "\"one\""` and `expected a clean build,
found 2 error(s)`, with exit code 1.

### 9.1 One thing the demonstration caught

The summary line said "the gate was never called" directly beneath a histogram
saying `gate x1`. `gate start` returns no verdict, and the summary was inferring
"never called" from an empty verdict list. Called and silent is not the same as
never called, and a report that contradicts the line above it is worse than one
that says less.

### 9.2 The first live run, 2026-08-27

Two cases against `claude-sonnet-5`, 29,290 input and 909 output tokens for the
pair, about seven pence at the published rate. Both passed. Two findings, and
the first one is the corpus earning its keep on its first outing.

**The corpus found a bad case before it found anything about a model.** The
`fix-type-errors` prompt said two functions "return the wrong type". Sonnet
fixed that by widening the signatures to `-> &'static str`, which compiles
cleanly, satisfies `cargo check`, and is a fair reading of what the prompt
actually said. The case's `not_contains` assertion caught it, so the run failed,
and the failure was mine. The prompt now names which half is wrong and asserts
both signatures survive.

This is the failure mode a corpus exists to expose, and it is worth being blunt
about the direction: the first thing an eval measures is the eval.

**When the gate gets reached for is task-shaped.** On the ambiguous prompt
Sonnet called `gate` twice, taking a baseline and checking after its edit. On
the unambiguous one it never called the gate at all, verifying with a single
`cargo check` after a single `apply_patch`. One observation each way is not a
finding, but it is the shape of the question RFC-0011's open questions are
asking, and the histogram that answers it now exists.

The tool sequence on the passing run was `search`, `read`, `apply_patch`,
`cargo`. No `exec`, which is one data point against RFC-0010's worry that the
absence of a shell blocks tasks, and precisely one.

### 9.3 Six cases, 2026-08-27

The corpus grew to six and ran against `claude-sonnet-5`: 112,942 input and
4,535 output tokens, about twenty-seven pence, six passes. Every result was
correct code. Four findings, two of which are about DanchoAI rather than about
the model, which is the more useful direction.

**The gate was called in one case out of six.** `rename-across-files` was built
specifically to force the red state RFC-0011 exists for: seven occurrences of a
type across three files, unrenameable in one edit. The batch **did** pass
through a red state, twice, between three sequential `apply_patch` calls. The
model simply never looked. It read all three files first, applied all three
patches, then ran `cargo check` once and was done.

So the gate's premise holds and its usefulness does not follow from it. The gate
helps a model that checks mid-batch; a model that reads everything before
editing never reaches a state it needs help navigating. That is a finding about
the gate's value rather than about the model's competence, and it sharpens
RFC-0011's open question about the `exhausted` threshold into a prior one:
whether the system prompt should ask for a mid-batch check at all, and whether
that would help or merely cost a compile.

**The one `exec` call in six cases was `find . -name "*.rs"`.** Not a pipeline. A
directory listing, which is the one thing the tool surface does not offer:
`search` finds text and `read` reads a known path, and neither answers "what
files are here". The model reached for a shell to get a capability rather than a
syntax.

That is a second piece of evidence against RFC-0010's worry that the absence of
pipes blocks tasks, and the first piece of evidence for a missing `list` tool.
One observation is not a mandate, so it is recorded as an open question rather
than built.

**The closed Cargo argument surface was sufficient.** `fix-failing-test` ran
`cargo test`, edited, ran `cargo test` again, then `cargo test` with
`filter: "tests::"`. RFC-0008 guessed at which narrowing arguments were worth
having and this is the first evidence that the guess covered a real use.

**The answer-assertion path works end to end.** `count-without-editing` changed
nothing, which the harness's no-change rule exempts because the case asserts on
the answer, and replied "There are 3 calls to `.unwrap()` in src/lib.rs - on
lines 2, 6, and 10." A corpus that could only ask for edits would only ever have
learned about editing.

One aside worth keeping. In `fix-warnings-at-the-cause` the obvious fix for an
unused binding is to delete it. The model instead used it, as
`HashSet::with_capacity(capacity_hint)`, which is what the prompt asked for and
is the better change. Assertions cannot tell the difference between the obvious
fix and the good one, which is section 8's argument for why a judge is a
different problem and not one to reach for yet.

### 9.4 The loop closing, 2026-08-27

Section 9.3's second finding was that the corpus's one `exec` call was a
directory listing rather than a pipeline. RFC-0005 section 6 added a `list`
tool on that evidence, and the same corpus was re-run against the same model
with nothing else changed.

`exec` went from one call to zero. `list` was reached for in two cases rather
than the one that had used `exec`. Six of six still passed, and the token total
moved by under one percent.

That is the whole point of the thing, so it is worth naming: a corpus produced a
finding, the finding produced a change, and re-running the corpus confirmed the
change did what it was supposed to and cost nothing. Two runs is not a
methodology, but it is the shape the methodology has to take, and this is the
first time this repository has closed that loop rather than reasoned about it.

### 9.5 A seventh case, and a design put in doubt

`thread-a-result` was written to test one thing: whether the gate goes unused
because no case in the corpus is hard enough to need it. It changes a `u32`
return into a `Result` across three files, where which call sites can propagate
and which cannot is knowable from the compiler and not from grep.

It passed, in twelve turns for about fifteen pence, and the code it produced was
idiomatic. And it did use the gate, which is what makes the result useful: the
model called `gate start` **after** applying all three patches, got `unchanged`,
and ran `cargo check` anyway.

Difficulty was not the missing ingredient. The model reaches for the gate as a
terminal verifier, and RFC-0011 built a mid-batch progress tracker. That doubt
is written up in RFC-0011 section 12.1, along with what would settle it, none of
which has been run.

The run also found a bug: `unchanged`'s message accused the model of having
fixed nothing, when it had fixed everything before the baseline was taken. That
is RFC-0011 section 12.2.

Two things worth naming about this. It is the first eval finding that is
uncomfortable for a design decision in this repository rather than for a case in
its corpus, which is what the corpus is for and is a good deal less pleasant
than the previous four. And the case was written to confirm a hypothesis and
refuted it instead, which is the only reason writing it was worth the money.

### 9.6 Measuring the noise before measuring the signal

The plan after section 9.5 was to change the system prompt to ask for a
mid-batch gate check and measure before and after. Two things stopped it, and
both were worth more than the experiment would have been.

The first: comparing the section 9.3 and 9.5 runs, the same case used the gate
twice in one and not at all in the other. That looked like variance large enough
to swamp any prompt effect, which would have made an n=1 A/B unreadable. So the
noise floor was measured first: the same four cases, the same model, the same
configuration, twice. **Gate usage was identical across both runs** — the same
two cases, the same two calls each, the same verdicts. Variance was ±1 turn per
case and 6.6% in input tokens.

The second: with the noise floor known, the apparent variance relocated. It was
not run-to-run. It was the tool-description fix made an hour earlier for an
unrelated reason. RFC-0011 section 12.1.1 has the table.

The prompt experiment was then not run, and that is a result rather than an
omission. It would have measured whether asking produces more gate calls. What
is in doubt is whether a gate verdict ever changes a decision, and every live
verdict so far has been `green` or `unchanged`. More calls to a tool that keeps
answering "nothing to report" is not evidence about a tool's usefulness, and
sixty pence spent producing a number nobody could interpret is worse than sixty
pence unspent.

### 9.7 An eighth case, and the verdict nobody had seen

Section 9.6 said the next experiment was a harder case rather than a louder
prompt, and named the property it needed: a batch that goes red in a way the
model must react to. `add-a-field` has it. A struct gains a `String` field, which
forces `Copy` off it, which breaks every implicit copy. Nothing in
`readings[index - 1]` says a copy happens there, and fixing the first error
reveals a second the first was masking.

It passed twice, in eight and ten turns, for about ten pence a run, and both
runs produced `gate: regressing, green`, the same tool histogram, and correct
code. The `regressing` verdict is the first live sighting of anything other than
`green` or `unchanged`, and the model responded to it by reading the two files
it had not yet opened.

Two lessons about writing cases, which is what this section is really about.

**Difficulty is not the variable; predictability is.** `thread-a-result`
(section 9.5) is harder in every ordinary sense — more files, more signatures,
a `Result` threaded through call sites — and the model dispatched it in one pass
because the whole cascade is visible by reading. `add-a-field` is a smaller
change that cannot be read ahead, and it forced the loop. Three cases were spent
learning that.

**The corpus is now discriminating between designs rather than only between
models.** RFC-0011 section 12.1 doubted the gate's premise; section 12.1.3
substantially withdraws that doubt on the strength of these two runs, and
replaces it with a sharper question the corpus can also answer. That is the
first time a case has been written to settle an internal argument and has
settled it.

### 9.8 Ablation, and writing the prediction down first

The harness gained `--without <tool>`, which hides a tool as completely as if it
had never been built: absent from the definitions, and an unknown-tool error if
named. Anything less would measure a tool that is present but discouraged, which
is a different experiment.

`add-a-field` was then run twice with the gate hidden, against the two runs with
it. The prediction was written into the terminal before the first run rather
than after the last, which is the only reason the result can be read as evidence
rather than as a story. It held exactly, and RFC-0011 section 12.1.4 has the
table: `cargo check` fills the gate's slot, and every difference is inside the
noise floor.

Two things this section is really recording.

**A null result is a result, and this one costs something.** The gate is
correct, used as designed, and has not been shown to change an outcome. Writing
that down is the whole reason the corpus exists; a repository that only ever
found its own designs vindicated would not be measuring anything.

**Ablation is the cheapest lever the harness has.** Twenty pence answered a
question that three RFC sections had been arguing about, and it works for any
tool. The obvious next uses are `search` and `list`, both of which have been
assumed useful and neither of which has been withheld to find out.

### 9.9 Withholding `search` and `list`

**Measured on 2026-08-27**, three cases, three arms, about fifty pence.

| case | control | `--without list` | `--without search` |
|---|---|---|---|
| `rename-across-files` | patch x3, cargo, **list**, read x3 | patch x3, gate x2, read x3, **search** | patch x3, cargo, **list**, read x3 |
| `count-without-editing` | **search x2** | read, **search** | **read** only |
| `fix-failing-test` | patch, cargo, **exec**, gate, list, read | patch, cargo x3, read | patch, cargo, **list**, read |
| input tokens | 61,874 | 57,966 | 45,801 |

All nine runs passed.

**The prediction about `list` was refuted, and that is the useful part.** It was
written down before the runs: withhold `list` and `exec` comes back, as it did
before `list` existed. It did not. The model used `search` and `read` and never
reached for a shell. So `list` is a preference rather than a necessity, at least
on fixtures small enough to read whole.

**`search` is not load-bearing on this corpus, which says more about the corpus
than about `search`.** `count-without-editing` exists to count occurrences of
`.unwrap()`, and without `search` the model read the file and counted three,
naming the lines. Every fixture here is under ten files. A finding that search
is redundant on a ten-file crate is not a finding about search.

**The one `exec` call was in the control arm**, where `list` was available and
had already been used: `find . -maxdepth 3 -name .dancho`. `list` cannot answer
that, because it respects ignore rules. RFC-0005 section 6.1 has the fix and the
argument for building it on a single observation.

**Fewer tools cost fewer tokens**, by more than the noise floor: 45,801 against
61,874 for the same three cases and the same outcomes, a 26% difference against
a measured 6.6% run-to-run variance. Confounded, because the leaner arm also
made fewer calls, but suggestive that a tool surface has a price and that adding
one is not free.

### 9.12 The first time the corpus compared two models

Nine cases against `claude-sonnet-5` and `claude-opus-5`, same build, same day.
Until now the harness had been used entirely to debug DanchoAI and never once
for the thing it is named after.

| | Sonnet | Opus |
|---|---:|---:|
| passed | 9/9 | 9/9 |
| model turns | 56 | 57 |
| input tokens | 242,282 | 324,589 |
| output tokens | 10,039 | 16,721 |
| cost at list price | $0.58 | $2.04 |

**The assertions do not discriminate.** Both models pass every case. Section 8
said a judge model becomes worth designing "when the checkable assertions have
stopped discriminating between models"; on this corpus, for these two models,
they have. That is a statement about the corpus being too easy, not about the
models being equal.

**The metrics discriminate sharply.** On `wide-cascade`, the case built so that
reading everything is expensive, **Opus read all thirty-one files and Sonnet read
five** — the four that broke plus the type's own module. Same correct result,
111,301 input tokens against 42,393. Opus also reached for `list` in eight cases
of nine where Sonnet used it in two, and for `exec` three times where Sonnet used
it none.

So the two models solve this corpus differently and the difference is invisible
to every assertion in it. **That vindicates section 2's split more sharply than
anything before it:** had turn counts or token counts been assertions, both
models would still have passed and the difference would have been thrown away.
Assertions decided nothing here; metrics carried the entire finding.

**What this does not say.** One run each. Cost is list price, not measured
billing. And "Sonnet is better for this" would be a claim about nine small Rust
fixtures, which is not a claim about anything else.

**A miss worth recording.** The run was estimated at about seventy pence for
Opus and cost $2.04. The estimate assumed comparable token counts and applied
the price ratio; Opus used 34% more input at 2.5x the price, and reading
thirty-one files rather than five is where that went. Estimating a model's cost
from another model's token counts is the error, and the harness now prints
totals precisely so that it is checkable afterwards.

## 10. Open questions

- Whether a case should be able to assert on the gate's final verdict, which is
  tempting and is prescribing method by the back door.
- What a corpus looks like that discriminates between Opus and Sonnet on
  assertions rather than only on metrics. Section 9.12 says the current one does
  not, and harder cases are the cheaper answer to try before a judge model.
- ~~What `--without search` and `--without list` do to the corpus.~~ Answered in
  section 9.9. The follow-on question is whether either earns its place on a
  workspace large enough that reading everything is not an option, which this
  corpus cannot ask.
- Whether a case needs a per-case timeout distinct from its turn limit, since a
  model can spend a great deal of wall time inside one turn.
- How many runs of a live case are needed before a number means anything. First
  evidence, section 9.6: two identical runs of four cases agreed exactly on tool
  usage and varied by ±1 turn and 6.6% in tokens. That is enough to read a tool
  histogram from one run and not enough to read a token count.
- ~~Whether the missing capability behind the corpus's one `exec` call is a
  `list` tool.~~ Answered in section 9.4: it was.
- ~~Whether the system prompt should ask for a mid-batch gate check.~~ Set aside
  in section 9.6: it would measure the wrong thing. The open question underneath
  it is whether a gate verdict ever changes a decision, which needs a case that
  goes red rather than a prompt that asks more often.
