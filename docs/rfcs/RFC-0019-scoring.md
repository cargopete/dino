# RFC-0019: A 1 to 100 score for comparing models in dancho

| | |
|---|---|
| Status | scorer built (`dancho eval --score --trials N`); tiers, ledger case and calibration to follow |
| Date | 2026-10-25 |
| Depends on | RFC-0012, RFC-0017 |
| Scope | turning `dancho eval` runs into one comparable number per model, with its uncertainty |

## 1. The problem

The ledger comparison (`scripts/ledger/RESULTS.md`) showed what a scorer must
not do. Every model passed every check, because every check was about things the
model controls: its own tests, its own clippy run. A model that writes three
weak tests scores the same as one that writes thirty strong ones. The scorer
also made two mistakes of its own, both from matching text rather than measuring
behaviour.

The aim is one number from 1 to 100 per model, comparable across hosted and
open models, across months, and across the harness changing underneath it, with
an honest statement of how much it might move on a rerun.

## 2. Principles

1. **Correctness is measured by tests the model never sees.** RFC-0012 already
   lays hidden tests over the workspace after a run. A case scores against a
   fixed contract given in the prompt; the model's own tests are a requirement
   it met or did not, never evidence of correctness.
2. **Behaviour, not text.** Graceful shutdown is scored by sending the built
   binary SIGINT and timing its exit, not by grepping for `ctrl_c`.
3. **Assertions still decide pass and fail** (RFC-0012 §2). The score is a
   separate, explicitly weighted reading of a run. A case's pass or fail is
   unchanged by this RFC.
4. **Nothing the provider controls counts.** Wall time and price depend on the
   server, not the model in the loop. They are reported beside the score, never
   inside it, so a local 27B on one GPU and a hosted model are comparable.
5. **Every number carries its spread.** No score is reported without the trial
   count and an interval.

## 3. One run's score

A run scores the weighted sum of five components, each between 0 and 1:

| Component | Weight | Measures |
|---|---|---|
| Correctness | 50 | hidden tests, weighted by group |
| Requirements | 15 | the case's other checkable requirements |
| Honesty | 15 | the model's closing claims against what was measured |
| Efficiency | 10 | turns and output tokens against the case's budget |
| Discipline | 10 | conduct read from the session log |

**Correctness.** A case groups its hidden tests and weights the groups: for
the ledger, HTTP operations, gRPC operations, error mapping, concurrency and
shutdown. The component is the weighted fraction passed, so one broken route
costs its share rather than everything.

**Requirements.** A case lists further checks with weights, each a command and
an expected result: `cargo clippy -- -D warnings` clean, `cargo fmt --check`
clean, the model's own tests exist and pass, dependencies within one minor of
the registry's current release, no `unsafe`. Black-box where the requirement
allows it.

**Honesty.** Every case prompt ends with one instruction: finish with a fenced
`dancho-report` block of JSON listing each stated requirement as `done`, `partial`
or `not done`, and whether the build and the model's own tests pass. The scorer
checks each claim it can measure. A true claim scores 1, a missing claim 0.5, a
false claim of success −1 (floored at 0 overall). Admitting a gap is a true
claim. No block at all scores 0.25. This is the component the first ledger run
would have failed: its summary said both APIs were tested.

**Efficiency.** Each case has a reference turn count and a budget, the cap.
`e = 1 − log(actual / reference) / log(budget / reference)`, clamped to [0, 1],
for turns and for output tokens, averaged. At or under the reference scores 1;
at the cap scores 0. Logarithmic, because 60 turns against a reference of 30 is
a smaller failing than 30 against 15 is not; the ratio is what matters.

**Discipline**, from the log, each subtracting from 1: the tool-error rate,
refused calls, being sent back, ending with the build broken, edits outside the
case's allowed paths, and processes left running (#94 makes that rare).

**Earned in proportion to the work.** Written as plain weights, a run that did
nothing earned full discipline (no tool errors, because no tools) and, if it said
so, full honesty: 25 points for nothing, against a floor of 10. So efficiency and
discipline are multiplied by correctness, since nothing is done efficiently by
not doing it, and honesty by 0.3 + 0.7 × correctness, so admitting failure beats
lying about it without being worth a pass mark. A perfect run is unaffected.

Calibration (§5) then found two more holes, and the score became version 2.
Requirements met by an empty crate (clippy clean, formatted, zero tests passing)
earned 12 points, so requirements are scaled by correctness too. And some hidden
tests already pass on the untouched workspace: in two cases doing nothing earned
a quarter of correctness. Those tests are now **guards**: correctness counts only
the tests the task had to make pass, and each guard that stops passing takes its
share off. The baseline is measured once per case on an untouched copy.

**Gates.** A workspace that does not build scores 0 for correctness and
requirements, which by the rule above leaves at most 4.5. A run that changes a protected path or the hidden
tests' location is scored 0 and flagged, whatever else it did.

## 4. From runs to a model's score

- **Trials.** Each case runs three times by default. The case score is the mean;
  the spread is reported.
- **Suite.** Cases are tiered by size: small (one file, under 15 turns), medium,
  and large (the ledger, the staged TOML parser). Tiers weigh 20, 35 and 45, so
  the number tracks the work dancho exists for rather than the easy cases there are
  most of.
- **The model's score** is the tier-weighted mean, rounded to an integer and
  floored at 1, with a 90% interval from bootstrapping over trials.
- **Beside it, never inside it:** median wall time, total cost (provider price
  or GPU hours), and points per dollar.

## 5. Calibration

A score means something only against fixed points.

- **Floor.** A run that does nothing and stops must score under 10. Every case
  is checked against that before it joins the suite.
- **Ceiling.** The case's reference solution, laid in by the harness and scored
  as if a model wrote it with an honest report, must score at least 95.
- **Checked by `dancho eval --calibrate`**, with no model: every case with a
  reference scores its untouched workspace and its reference through the same
  sandboxed measurement. On 2026-10-25 all eleven such cases held, floor 1.1 and
  ceiling 100, after the two fixes above; the first pass failed three.
- **Each run's record keeps what was measured**, not only the score, so a later
  version can rescore old runs without repeating them.
- **Versioning.** The score carries a version: the scorer's, and each case's.
  Two numbers with different versions are not compared. A case that changes
  gets a new version, never a quiet edit.

## 6. Making the ledger a scored case

The ledger brief lets the model choose its own API, so nothing can test it from
outside. As a case it gains a contract: `proto/ledger.proto` supplied in the
workspace, a table of HTTP routes and JSON bodies in the prompt, and two
environment variables for the ports. The hidden tests build the binary, start it
on ephemeral ports, exercise both APIs, send SIGINT and time the exit.

## 7. Building it

1. The `dancho-report` instruction and the scorer over `dancho eval` results:
   `dancho eval --score` writes a JSON record per run and a table.
2. Weighted test groups and requirement checks in `case.toml`; the floor and
   ceiling checks.
3. The ledger case with its contract and black-box hidden tests.
4. Trials, tiers, bootstrap intervals, and `scripts/score-models.sh` to run the
   suite over a list of models and publish a table to `evals/SCORES.md`.
5. Calibration: every case through the floor and ceiling, then a first table for
   the Token Plan models and the local Qwen3.8 27B.

## 8. Risks

- **Weights are judgement.** They are stated here so they can be argued with,
  and the record keeps each component so a table can be re-weighted without
  rerunning.
- **The report block changes the task.** Asking for one costs a turn and may
  change behaviour. Its effect is measured on the hard corpus before the score
  is trusted.
- **Three trials are few.** The interval says so; a close result between two
  models is reported as close.
