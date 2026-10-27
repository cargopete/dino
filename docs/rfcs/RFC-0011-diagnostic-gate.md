# RFC-0011: The Rust diagnostic gate

| | |
|---|---|
| Status | implemented M5; withdrawn from the default tool set M11 |
| Date | 2026-08-27 |
| Depends on | RFC-0001, RFC-0005, RFC-0008 |
| Scope | diagnostic identity, batch progress, the definition of done |

## 1. Purpose

RFC-0001 section 7 describes the piece that makes this a Rust agent rather than
a general one:

> An edit batch does not have to compile after every individual change.
> Multi-site Rust changes often pass through a red state. The gate tracks the
> diagnostic set and permits accumulated edits when they make measurable
> progress. It rolls back or refuses a batch that stops improving. A green build
> with no material diff is not task success.

Every clause there is a design decision. This RFC makes them.

## 2. It refuses; it does not roll back

RFC-0001 said "rolls back or refuses". This RFC answers with **refuses**, and
the reason is worth putting on the record rather than leaving as an omission.

Rolling back arbitrary workspace edits means the gate keeps a shadow copy of the
workspace and restores from it. That is a worse version of git that will drift
from the real one, and it would be the second thing in this repository claiming
to know what the files used to say. Version control already does this correctly
and every Rust workspace has it.

So the gate measures and reports, and its teeth are that **it will not report
success**. A batch that has stopped improving gets a verdict saying so, and a
green build that changed nothing gets a verdict saying that too.

**This is a quality signal, not a safety boundary, and the distinction is not a
weasel.** Safety is about what a model can do to a machine, is enforced in code
below the model, and is RFC-0006 and RFC-0009's business. The gate is about
whether work is progressing. Enforcing it by refusing further edits would trap a
model halfway through a legitimate multi-site rename, which is the exact case
section 7 exists to permit.

## 3. Diagnostic identity

Comparing two diagnostic sets needs a stable identity for a diagnostic, and the
obvious one is wrong. `(level, code, file, line, column, message)` changes when
an edit above it shifts a line, so a batch that fixed nothing would appear to
have resolved eleven diagnostics and introduced eleven others.

Identity is therefore `(level, code, file, message)` with a count, and line and
column are carried for display only. Two identical errors in one file collapse
into one identity with a count of two, which is the right granularity for
"is this getting better" even though it is the wrong one for "show me each one".

## 4. Verdicts

The gate holds a baseline from `start` and the previous set from the last
`check`. Progress is measured **since the last check**, because "stops
improving" is a statement about the most recent edits rather than about the
batch as a whole.

Let `resolved` be identities present at the previous check and absent now, and
`introduced` be identities absent then and present now, counting errors only.
Warnings are reported and do not decide a verdict; a batch that trades an error
for a warning has made progress.

| Verdict | When | Meaning |
|---|---|---|
| `green` | no errors, and files changed since `start` | done |
| `unchanged` | no errors, and no file changed since `start` | **not** done |
| `improving` | `resolved > introduced` | keep going |
| `regressing` | `introduced > resolved` | the last edits made it worse |
| `stalled` | `resolved == introduced` | the last edits changed nothing measurable |
| `exhausted` | the second consecutive `stalled` or `regressing` | stop and reconsider |

`unchanged` is section 7's "a green build with no material diff is not task
success", made into a value a model has to look at. A model asked to fix
something, which then runs the gate and gets `green` without having touched a
file, has reported someone else's success as its own.

`exhausted` is where "refuses a batch that stops improving" lives. Two
consecutive non-improving checks, not one, because a single stalled check is a
normal step in a multi-site change where the next edit unblocks a cascade.

## 5. Material diff

Deciding `green` from `unchanged` needs to know whether anything changed, which
means the gate has to look at the files rather than trust that an edit tool ran.

At `start` it walks the workspace with the same ignore rules `search` uses,
fingerprints every `.rs` file with SHA-256, and stores the map. At `check` it
walks again and reports the number of files added, removed and modified, and the
net byte change.

Fingerprinting rather than trusting an edit count matters: an `apply_patch` that
wrote a file and a later one that wrote it back are two edits and no change, and
the gate should say no change.

The walk is capped at 20,000 files, matching RFC-0005's search cap. Exceeding it
sets `truncated` and the `unchanged` verdict is withheld rather than guessed,
because a gate that cannot see the whole workspace must not claim nothing in it
moved.

## 6. Command surface

One tool, three commands, no free-form arguments:

| `command` | |
|---|---|
| `start` | record the baseline diagnostics and fingerprints |
| `check` | re-run, compare, return a verdict |
| `status` | the last verdict, without re-running anything |

`check` before `start` is an error rather than an implicit baseline of zero,
because a gate that invents its own baseline will always report improvement.

The underlying invocation is `cargo check --workspace --all-targets
--message-format=json`, plus `--offline` under confinement. Clippy is not run:
its warnings are noise for a question about errors, and doubling the wall clock
of the one tool a model is meant to call repeatedly is a poor trade.

## 7. Classification

`start` and `check` run Cargo, so they are `ToolClass::Process` and no policy
auto-allows them. `status` runs nothing and is `ReadOnly`.

The approval subject names both the gate command and the Cargo invocation it
will run, so a session rule is narrow and a person is shown the actual call
rather than a friendly summary of it.

## 8. Out of scope

Rollback, clippy in the gate, test results as a progress signal, selecting tests
from the changed package graph, per-package baselines, and any use of the gate's
verdict to block another tool.

Tests deserve a note rather than silence. "Does it compile" and "does it pass"
are different questions, and a gate that ran the test suite on every check would
be too slow to call often, which would defeat the point of a tool designed to be
called after every few edits. A test-aware second gate is a later RFC.

## 9. Verification

**Measured on 2026-08-27.** The workspace passes 111 tests and Clippy with
`-D warnings`. Every gate test runs a real `cargo check` over a
dependency-free fixture, so the verdicts are measured against a compiler rather
than against a mock of one.

- Two errors, one fixed, reporting `improving`; the second fixed, reporting
  `green`.
- Two errors of *different kinds*, one fixed, naming the resolved `E0425` in the
  report, so the distinct-identity path is covered as well as the multiplicity
  one.
- A `check` with no edits between reporting `stalled`, and a second consecutive
  one reporting `exhausted`.
- An edit that adds an error reporting `regressing`, with a message that says to
  revert.
- A green fixture never edited reporting `unchanged`, not `green`.
- An edit and its exact reversal reporting no files changed.
- A diagnostic whose line moved reporting neither resolved nor introduced.
- `check` before `start` refused; `status` reporting the last verdict; `start`
  and `check` classified `Process` and `status` `ReadOnly`.

### 9.1 A design error the tests caught

The first implementation compared identity *sets* rather than counts. Two
mismatched-types errors in one file share an identity, so fixing one of them
produced no set difference and the gate reported `stalled` for real progress.
The delta is now computed per identity over counts: multiplicity is part of the
measurement, and line numbers still are not. Section 3 always said identity was
`(level, code, file, message)`; what it did not say, and now does by way of this
note, is that an identity carries a count and the count is load-bearing.

**Observed on 2026-08-27**, driving the built `dancho` binary against a local fake
endpoint over a two-error fixture, with the sandbox in force: `gate start`,
`apply_patch`, `gate check`. The model received `verdict: improving`, two errors
down to one, one file changed by minus four bytes, and "Fewer distinct errors
than at the last check. Keep going."

## 10. Withdrawn, 2026-08-27

**The gate is no longer offered.** The code stays, the tests stay, this document
stays. It is not in the tool set a session is given and it is not in the system
prompt.

Section 12.1.4 measured it against `cargo check` on a four-file case and found
no difference. The objection was that four files is not where a progress tracker
earns its keep, so RFC-0012 built `wide-cascade`: twenty-nine modules, a `Copy`
removal cascading into four of them, invisible in the source, too many files to
read whole.

| arm | turns | input tokens |
|---|---:|---:|
| with the gate | 10 | 52,271 |
| without it | **8** | **43,711** |

Identical reads, identical patches, identical result. `cargo check` filled the
gate's slot exactly, and the run was two turns and 8,560 tokens shorter without
it.

**The mechanism is understood, which is why this is a conclusion rather than a
data point.** A compiler error names its file. `regressing` tells a model
nothing about *where* that `cargo check` has not already said, and locating the
damage was the entire job in this case. The gate's distinctive information is a
comparison against a baseline, and a model about to read four files does not
need to be told the count first.

What is still untested is `stalled` and `exhausted` on a batch that genuinely
fails to converge. Both live sightings of `stalled` were artefacts of a baseline
taken after the work rather than before it, which is a mis-ordering and not a
stall. Constructing a real one means finding a task a competent model fails at
repeatedly, and after nine cases I have not managed it.

So: kept, because that case may yet be written and the code is correct. Not
offered, because every measurement so far says it costs turns and tokens and
changes nothing. Putting it back is one line in `dancho_eval::session_tools`.

Building something, arguing for it, measuring it twice and then withdrawing it
is the outcome this repository's method is supposed to produce. It is not a
pleasant one.

## 10.1 A broken build at the end of a turn, 2026-10-21

The gate came back in one narrow form (#31). In bench v5, Qwen ended its turn
after 9 of 80 turns with 19 compile errors and no message, and the run ended
there.

In `dancho run` and `dancho eval` only, when a model ends its turn in a Cargo
workspace, the harness runs `cargo check`. If there are errors, the model is
sent back with the count and the first three, and told to fix them or call
`report_blocker`. This happens once per submission, never on the last turn,
and never in an interactive session, where ending a turn hands control to a
person. A check that cannot run lets the run end. It is recorded as
`AgentEvent::SentBack`.

This is the "definition of done" of §1 at its smallest: not a verdict on
progress, only a refusal to call a broken build finished.

The eval harness now ignores a root `Cargo.lock` when deciding whether the
workspace changed. That check runs Cargo and may create the lockfile, and a
run that did nothing must still fail.

A run that hits its turn limit is never sent back, and in bench v8 one ended
that way with two borrow errors, scoring nothing on tests it had mostly
passed. So the same check also runs with each turns-running-out notice, and
its findings go into the notice: the model is told the build is broken while
it still has turns to fix or undo the change in progress.

## 11. Open questions

- Whether `exhausted` should be two consecutive checks or a ratio over the
  batch. RFC-0012 records every gate verdict per case, so this is now a query
  against a corpus rather than an opinion. It is also the wrong question to ask
  next; see below.

### 12.1 The premise is in doubt

**Two live findings, recorded on 2026-08-27, and they point the same way.**

Across six cases (RFC-0012 section 9.3) the gate was called in one. The case
built specifically to force a red state passed through one without the model
ever looking: it read three files, applied three patches, then ran `cargo check`
once.

A harder case was then written to see whether difficulty was the missing
ingredient: a `u32` return threaded into a `Result` across three files, where
which call sites can propagate and which cannot is a thing the compiler knows
and grep does not. The model used the gate. It used it **after finishing**: five
reads, three patches, three verifying searches, then `gate start`, then
`gate check`, then `cargo check`.

The baseline was therefore taken from the finished code, the verdict was
`unchanged`, and the model ran `cargo check` immediately afterwards anyway.

So the shape of the evidence is not "the gate is unused". It is that **this
model reaches for the gate as a terminal verifier, and `cargo check` is already
one.** RFC-0001 section 7 designed a progress tracker for a model that checks
mid-batch. Twice now the observed behaviour has been to read everything, edit
everything, and check once at the end, which is a strategy the gate adds nothing
to.

That is a doubt about the premise rather than about the implementation.

### 12.1.1 Half of it turned out to be wording

**Measured on 2026-08-27.** Section 12.2's fix changed the tool description from
"Call start before editing to record a baseline" to "Call start BEFORE your
first edit ... a baseline taken after the work cannot see that work and will
report unchanged". The four multi-edit cases were then run twice on the fixed
description, and compared against the single run on the old one:

| case | before, 1 run | after, 2 runs |
|---|---:|---:|
| `rename-across-files` | 0 | 2, 2 |
| `thread-a-result` | 2, misused | 0, 0 |
| `fix-warnings-at-the-cause` | 2 | 2, 2 |
| `fix-failing-test` | 0 | 0, 0 |

Both cases that moved, moved the way the wording pushed, and both reproduced.
`rename-across-files` now takes a baseline, applies three patches, and checks:
exactly the usage section 4 designed. `thread-a-result` stopped taking a
baseline after finishing, which was the misuse the old wording invited.

One run on the old description is not a control group, so this is suggestive
rather than established. What it does establish is that **the gate's usage is
sensitive to how the tool describes itself, and measurably so**, which is a more
tractable lever than a system prompt and was found by accident while fixing a
message.

Run-to-run variance on the two identical runs was ±1 turn per case and 6.6% in
input tokens, with gate usage identical. So the metric is stable enough to read,
which is the other thing those two runs bought.

### 12.1.2 What is still in doubt

Not whether the gate is called. Whether its answer ever changes anything.

Every verdict a live model has received has been `green` or `unchanged`.
`improving`, `regressing`, `stalled` and `exhausted` exist, are unit-tested
against a real compiler, and have never been seen in a live run. The gate is
being called at the right moments and finding nothing to report, because the
batch was correct by the time it was asked.

So the premise's second half is untested: a batch that goes red in a way the
model has to react to. `thread-a-result` was written to be that case and was
handled in a single pass. The next experiment is a harder case rather than a
louder prompt, because a prompt that asks for more checks produces more calls
and not more informative verdicts.

The honest position in the meantime is that the gate is built, correct, tested,
called as designed about half the time it could be, and has not yet been
observed to change a decision.

### 12.1.3 The premise, largely vindicated

**Measured on 2026-08-27, reproduced twice.** Section 12.1.2 said the next
experiment was a harder case rather than a louder prompt. RFC-0012's
`add-a-field` case is that: a struct gains a `String` field, which forces `Copy`
off it, which breaks every place that relied on an implicit copy. Nothing in
`readings[index - 1]` says a copy happens there, and fixing the first error
reveals a second that was masked by it. The cascade is invisible in the source
and legible only to the compiler.

Both runs produced the same shape:

```text
search  read reading.rs  gate start  apply_patch
gate check -> regressing   (baseline 0 errors -> 3, introduced 3, 1 file changed)
read stats.rs  read window.rs  apply_patch  apply_patch
gate check -> green
```

Three things worth naming.

It is the first live occurrence of any verdict other than `green` or
`unchanged`. `regressing` exists outside the tests.

The model **did not read everything before editing.** It read one file, made the
change that broke the build, was told it had introduced three errors, and only
then went and read the two files it had not seen. That is the mid-batch loop
RFC-0001 section 7 assumed and section 12.1 had begun to doubt.

And it took two things to get here, both of which came from evals rather than
from thinking: a case whose cascade cannot be read ahead of time, and a tool
description that says to call `start` before the first edit.

**What this does not establish.** `cargo check` would have reported the same
three errors. Whether the gate's framing changes what a model does next needed
the case run both ways, which is section 12.1.4.

### 12.1.4 The ablation, and a null result

**Measured on 2026-08-27, two runs each way.** RFC-0012's harness gained a
`--without` flag, which hides a tool as completely as if it had never been
built. `add-a-field` was then run twice with the gate hidden.

The prediction, written down before the runs: `cargo check` would fill the same
slot and reach the same result in a similar number of turns, making the gate's
marginal value nil for this case.

| | with gate | without gate |
|---|---|---|
| turns | 8, 10 | 9, 9 |
| input tokens | 37,053 · 46,369 | 43,403 · 43,263 |
| result | correct, both | correct, both |

```text
with:     search read [gate start] patch [gate check -> regressing] read read patch patch [gate check -> green]
without:  search read              patch [cargo check]              read read patch patch [cargo check]
```

The prediction held exactly. `cargo check` occupies the slot `gate check`
occupied, the same three patches follow the same two reads, and every difference
is inside the ±1 turn and 6.6% token noise floor measured in section 12.1.1.

**So on a batch that converges, the gate is `cargo check` with better wording.**
That is a null result and it is the honest reading of it.

What the gate has that `cargo check` does not is the baseline comparison and the
consecutive-stall counter. Neither mattered here, because the model converged in
one correction. The place the gate could still earn its cost is a batch that
does **not** converge, where `stalled` and `exhausted` would stop a model
repeating itself. That has never been observed, and constructing it deliberately
means finding a task a competent model fails at repeatedly, which is a harder
case to write than any so far.

Until then the position is: the gate is built, correct, tested, used as designed,
and has not been shown to change an outcome. It occupies a tool slot and prompt
tokens for something `cargo` already does. It stays because the stall detection
is the part that was never about verification, and that part is untested rather
than disproven.

### 12.2 A bug the same run found

`Unchanged`'s message read "Nothing was fixed here; a green build with no
material diff is not success." Against a baseline taken after the work, that is
flatly untrue: everything had been fixed, and the gate could not see it.

The gate cannot know what happened before `start`, so the message now says so,
and the tool description now says to call `start` before the first edit and what
happens if you do not. A verdict that overstates what it knows is worse than one
that says less, and this one was overstating in the direction of accusing a
model of doing nothing.
- Whether a warning that was an error a moment ago should count as resolved,
  which it currently does, and whether that lets a batch launder errors into
  allow attributes.
- Whether the fingerprint walk should use git where a repository exists, which
  would be faster and would also make the gate's answer depend on the index.
