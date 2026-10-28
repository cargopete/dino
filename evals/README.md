# The eval corpus

Each directory is one case: a `case.toml` and a `workspace/` tree that is copied
somewhere temporary before every run, so the corpus is never the thing being
edited.

Assertions in `case.toml` decide pass or fail and are about the outcome. What a
model did on the way is a metric, and metrics decide nothing. RFC-0012 section 2
explains why that line matters.

A case may also set `hidden_tests = true` and ship a `hidden/` tree. After the
run, and after every other assertion has been checked, the harness copies it over
the workspace and requires `cargo test` to pass. The model never sees those
tests, which is what lets a case check behaviour instead of only that the code
compiles. A `reference/` tree beside them, never copied, holds a solution that
proves the case can be passed.

`toml-subset-parser` is too long for an open-weight model to finish in its turn
limit, so it also comes as three stages, `toml-1-values`, `toml-2-tables` and
`toml-3-arrays`. Each stage starts from the previous stage's reference and keeps
its tests, so a stage passes or fails on its own work.

Two cases were added on 2026-10-27 because the rest of the corpus scored 93 to 95
and could no longer show a change: `semver-ranges` (a long written spec, with hidden
tests generated from node-semver by the script in its `oracle/`) and
`text-buffer-undo` (undo merging and groups, checked by random sequences against a
second implementation that keeps snapshots). On qwen3.7-max at three trials they
scored 94 (92-97) and 83 (68-98). A third, making sliding-window statistics fast,
scored 99 and was left out. The first runs scored 88 and 55 before two ambiguous
sentences in the prompts were fixed, so read a low score on a new case against its
prompt before trusting it.

`slugify-unprompted` (2026-10-28) asks for a small function and never mentions
tests. It passes only if the hidden tests pass and at least one new `#[test]`
was written (`new_tests = 1`), and every run reports `untested_at_end`, whether a
Rust file was edited after the last passing `cargo test`. It measures whether a
model verifies its work without being told to (#129).

Two rules the harness applies to every case, whether or not the case remembers:

- A case where nothing in the workspace changed fails, even if the code already
  compiled. A corpus that can pass by doing nothing measures nothing.
- Every case runs with the allow-all policy, because nobody is there to approve
  anything, and therefore inside the sandbox. Unattended and unconfined is the
  worst combination in this project.

```console
dino eval --model claude-opus
dino eval --case fix-type-errors --json
```

Live runs need a credential, cost money and are not deterministic. They are not
part of `cargo test`. The harness's own correctness is covered by scripted cases
that need no provider at all.

## Benchmarking against Claude Code

`scripts/bench.sh <out> <trials> <case>...` runs cases through `dino` against a
self-hosted model (`QWEN_API_BASE`, `QWEN_API_KEY`) and through Claude Code as a
baseline. It writes one result per model, trial and case, so an interrupted run
can be restarted where it stopped. Pass an ablation to every `dino` run with
`DINO_EVAL_ARGS`, for example `--max-diagnostics 3`.

`scripts/analyse-bench.py <out> --rescore` mines the session logs: tool errors
by kind, peak prompt against the served context, reasoning share, repeated
calls, and the `exec` commands a model used where a tool was missing. With
`--rescore`, a run cut off by the turn limit has the case's hidden tests run on
the code it left, because a run stopped while re-checking finished work is not a
failed run.

Change one thing at a time between benches, so a difference can be attributed
(issue #18). RFC-0017 section 8.5 records the results.
