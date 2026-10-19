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

Two rules the harness applies to every case, whether or not the case remembers:

- A case where nothing in the workspace changed fails, even if the code already
  compiled. A corpus that can pass by doing nothing measures nothing.
- Every case runs with the allow-all policy, because nobody is there to approve
  anything, and therefore inside the sandbox. Unattended and unconfined is the
  worst combination in this project.

```console
gyr eval --model claude-opus
gyr eval --case fix-type-errors --json
```

Live runs need a credential, cost money and are not deterministic. They are not
part of `cargo test`. The harness's own correctness is covered by scripted cases
that need no provider at all.
