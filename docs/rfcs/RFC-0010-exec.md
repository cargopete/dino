# RFC-0010: Process execution

| | |
|---|---|
| Status | implemented M3 |
| Date | 2026-08-27 |
| Depends on | RFC-0001, RFC-0006, RFC-0008, RFC-0009 |
| Scope | the `exec` tool, argument vectors, the shared workspace fence |

## 1. Decision

`exec` runs one program with one argument vector inside the sandbox. There is no
shell, no allow-list of permitted binaries and no deny-list of forbidden ones.

RFC-0008 deferred this until containment existed. RFC-0009 built the
containment and exercised it against real Rust builds, so the deferral has been
paid off rather than merely waited out.

## 2. No shell

The tool takes `["git", "log", "--oneline", "-20"]`, never
`"git log --oneline -20"`.

RFC-0001 section 8 asks for a sandbox that does not delegate the security
boundary to shell quoting. With a real operating-system sandbox the boundary is
the sandbox, so a shell would not breach it. The argument against one is
narrower and still holds: an argument vector is exactly what is approved, what
is recorded and what is executed, with no parsing step in between where those
three could differ. A person reading `sh -c "…"` in an approval prompt is being
asked to audit a string; a person reading an argument vector is being shown the
call.

The cost is real. No pipes, no redirection, no globbing, no `&&`. Most of what
an agent wants from a shell has a flag instead: `-20` rather than `| head -20`,
and `read` and `search` already exist for the cases that would otherwise be
`cat` and `grep`.

**Open, and deliberately not answered here.** If pipelines turn out to be
necessary rather than merely familiar, the answer is a designed one, and the
evidence for it should come from an eval corpus rather than from an afternoon's
irritation.

**Amended 2026-10-23.** Answered by a change of scope rather than by the
corpus. DanchoAI is now also a daily agent for its author's own repositories,
not all of them Rust, where `npm run`, `&&` chains and `| tail` are the normal
idiom and have no flag to stand in for them. A `shell` tool now runs one line
through `sh -c` beside `exec`. The audit concern is met as far as it can be: the
line is shown whole in the approval prompt, a session rule matches that exact
line and nothing broader, and the sandbox is the boundary, as above. The system
prompt still prefers `exec`.

## 3. No list of blessed binaries

An allow-list of permitted programs is the obvious safety feature and it is not
being built. It would be brittle, it would break on every project with its own
scripts, and it would create the impression of a boundary that a single
`./configure` walks straight through.

The boundary is the sandbox and the approval decision. Every `exec` call is
classified `Process`, which no policy auto-allows, and every one runs confined.

This has a consequence worth stating positively. RFC-0001 section 3 lists
automatic commits, pushes, pull requests, deployments and purchases as
non-goals. Under a confining sandbox the network is denied, so `git push`, `gh`,
`curl` and `cargo publish` **fail because they cannot reach anything**, not
because their names appear on a list. That is a stronger property than a
blacklist and it degrades honestly: run with `--sandbox none` and it is gone,
which is exactly why that flag is named in the prompt and written to the log.

A local `git commit` does work, one approval at a time. The non-goal is
automatic commits, and an approval a person gave is not automatic.

## 4. Command surface

```json
{"command": ["cargo", "tree", "--depth", "1"], "directory": "crates/dancho-core"}
```

`command` is a non-empty array of strings; the first is the program. `directory`
is optional, relative to the workspace root, and must resolve inside it.

A bare program name is passed to the operating system, which searches `PATH`
from the filtered child environment. A *relative* program path is resolved
against the workspace fence, so `./scripts/build.sh` works and
`../../elsewhere/script` does not.

An absolute program path is passed through unchanged. The first draft of this
refused them, which broke `/usr/bin/curl` while leaving `curl` by way of `PATH`
working perfectly: the same binary reached two ways, one of them forbidden for
the look of the thing. Which programs exist at all is the allow-list section 3
declined to build, and what any of them may do is the sandbox's business.

The environment is the same filtered set RFC-0008 defined: `PATH`, `HOME`,
`CARGO_HOME`, `RUSTUP_HOME`, `CARGO_TERM_COLOR=never`, `TERM=dumb`, and a
`TMPDIR` inside the workspace. The agent's provider credentials are not among
them.

## 5. Output

```json
{
  "command": "git log --oneline -20",
  "exit_code": 0,
  "timed_out": false,
  "stdout": "…",
  "stderr": "…",
  "truncated": false
}
```

Streams stay separate. Merging them loses which one said what, and a tool whose
output cannot distinguish a result from a warning is a tool that teaches a model
to guess.

Limits match RFC-0008: 32 KiB returned per stream, 8 MiB read from the pipes
before the remainder is discarded, and a 600-second wall clock after which the
child is killed and `timed_out` is set rather than a failure being invented.

## 6. One workspace fence, not two

`dancho-tools` resolves a model-supplied path against the workspace root: relative
only, no parent or root components, canonicalised, and required to remain
beneath the root. `exec` needs exactly the same rule for its working directory
and its program path.

Two implementations of one security check is how they drift. The rule moves to
`dancho-core::workspace`, `dancho-tools` is changed to call it, and its existing
traversal and symlink-escape tests now cover the shared implementation rather
than a private copy of it.

## 7. Crates

A new `dancho-exec` crate holds the `exec` tool and the process runner that RFC-0008
put in `dancho-rust`. The runner was never Rust-specific; it spawns, caps, times out
and kills. `dancho-rust` depends on `dancho-exec` for it and keeps the Cargo-specific
argument building and diagnostic parsing, which is what that crate is for.

## 8. Out of scope

Shells and pipelines, interactive processes and pseudo-terminals, stdin for the
child, background or long-lived processes, per-call timeouts, and any relaxation
of the filtered environment.

Interactive processes deserve a note rather than silence: the child's stdin is
`/dev/null`, so a program that waits for input reaches the wall clock and is
killed. That is a poor experience and a correct one, and it is better than a
hung agent.

## 9. Verification

**Measured on 2026-08-27.** The workspace passes 91 tests and Clippy with
`-D warnings`.

- The shared fence: the existing `dancho-tools` traversal and symlink-escape tests
  now exercise the shared implementation rather than a private copy.
- An empty command array, and an unknown argument field, refused before anything
  is spawned.
- A working directory outside the workspace refused at classification, so it
  never reaches a policy. A relative program path that climbs out, likewise.
- An absolute program path passed through, which the first draft got wrong.
- Classification: `exec` is `Process`, its subject is the argument vector as it
  will be run, and `git status` and `git push` produce different rule keys.
- A successful run, a failing run returning exit code 3, and the two streams
  captured separately.
- On macOS, a confined command writing outside the workspace: refused, with the
  test asserting the failure text holds `Operation not permitted` so it cannot
  pass because the fixture broke some other way.
- On macOS, a confined command writing *inside* the workspace: allowed, so the
  previous test is measuring confinement rather than a tool that never works.
- On macOS, a confined `curl` reaching the network: nothing returned.
- The wall clock killing a sleeping child after two seconds.

**Observed on 2026-08-27**, driving the built `dancho` binary against a local fake
endpoint over this repository:

- `exec git log --oneline -3` prompted with the argument vector and the
  containment named, then returned 330 bytes of commit lines.
- `exec git push --dry-run origin HEAD` failed with exit 128 and
  `ssh: connect to host github.com port 22: Operation not permitted`. Section 3
  claims the non-goal is enforced by the sandbox rather than by a name list;
  this is that claim measured rather than asserted.

## 10. Open questions

- Whether pipelines earn a designed answer, and how often their absence actually
  blocks a task. **Evidence so far, from RFC-0012 section 9.3:** across six live
  cases `exec` was called once, for `find . -name "*.rs"`. Not a pipeline, and
  not a workaround for one. The model wanted a capability the tool surface lacks
  rather than a syntax it lacks.
- ~~Therefore a newer question: whether `list` should be a tool.~~ **Answered
  and built.** RFC-0005 section 6 added `list`, and re-running the same corpus
  against the same model took `exec` from one call to zero while `list` was
  reached for in two cases. The shell was standing in for a missing capability,
  and with the capability present it was not wanted at all.

  This does not retire the pipeline question, it sharpens it: across six cases
  and two runs, nothing has yet reached for a shell to compose anything. A
  corpus of six is still a corpus of six.
- Whether `directory` should accept a path that does not exist yet, which would
  need the fence to reason about a parent rather than a target.
- Whether the filtered environment should gain a project-declared allow-list, so
  a repository can say which variables its own scripts need.
