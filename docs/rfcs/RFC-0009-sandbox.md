# RFC-0009: Operating-system sandbox

| | |
|---|---|
| Status | implemented M3 (macOS) and M6 (Linux) |
| Date | 2026-08-27 |
| Depends on | RFC-0001, RFC-0006, RFC-0008 |
| Scope | the sandbox seam, macOS Seatbelt, honest unavailability elsewhere |

## 1. Decision

Every process Gyrfalcon starts runs inside an operating-system sandbox that
confines writes to the workspace and denies the network. On macOS this is
Seatbelt. Everywhere else the sandbox is unavailable, and Gyrfalcon refuses to
run processes at all unless a person explicitly says `--sandbox none`, which is
named in the approval prompt and recorded in the session log.

RFC-0001 section 8 has claimed this since the beginning. Until now the enforced
boundary was a filesystem fence in the tool layer plus an approval decision,
which stops a tool from writing outside the workspace and does nothing whatever
about the build script that tool invites in.

`exec` is still not shipped. It is now unblocked rather than delivered, and it
gets its own RFC once the sandbox has been exercised against real work.

## 2. What is contained, and what is not

**Contained.** Writes outside the workspace, and the network.

**Not contained.** Reads. The profile allows `file-read*` everywhere, so a build
script can read `~/.ssh`, the shell history and any credential file on the
machine. With the network denied it cannot send what it reads anywhere, and the
combination is the actual guarantee: **a sandboxed process may learn a secret
but cannot write it down outside the workspace or transmit it.** A person who
then runs `--sandbox none`, or who grants network access when that mode exists,
has taken that protection off.

A narrower read profile was considered and rejected for this slice. Rust builds
read the toolchain, the registry cache, the system frameworks and a long tail of
configuration; an allow-list would have been guesswork with a failure mode of
mysterious build errors, and guesswork dressed as a security boundary is worse
than an honest wide one.

## 3. The seam

A new `gyr-sandbox` crate owns one trait:

```rust
pub trait Sandbox: Send + Sync + Debug {
    fn label(&self) -> &str;
    fn confines_writes(&self) -> bool;
    fn denies_network(&self) -> bool;
    fn temp_dir(&self) -> Option<&Path>;
    fn wrap(&self, program: &str, arguments: &[String])
        -> Result<WrappedCommand, SandboxError>;
}
```

`wrap` rewrites an argument vector. It does not spawn, so the crate needs no
async runtime and the process module keeps sole responsibility for spawning,
capping and killing. Two implementations ship: `Seatbelt` and `Unconfined`.

## 4. The Seatbelt profile

**Measured on 2026-08-27, macOS 26.5.1 (25F80), `/usr/bin/sandbox-exec`
present.** The profile is passed inline with `-p`, so no file is left behind:

```text
(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow file-read*)
(allow sysctl-read)
(allow mach-lookup)
(allow signal (target self))
(allow file-write* (literal "/dev/null"))
(allow file-write* (subpath "<canonical workspace root>"))
```

`sandbox-exec` is deprecated by Apple and still present and functional. When it
disappears, this crate is the single place that has to change, which is most of
the argument for the seam.

### 4.1 Path escaping

A workspace path is interpolated into an SBPL string literal, which makes it an
injection surface. **Measured:** a path containing an unescaped `"` produced a
malformed profile, and `sandbox-exec` refused to start rather than starting
unconfined. Refusing is the good outcome and is not something to rely on, so
backslashes and quotes are escaped and control characters are rejected before a
profile is built, with a test that says so.

### 4.2 The temporary directory

**Measured:** `cargo test` fails under this profile with
`failed to create temporary directory` from rustdoc, which builds doctests in
`TMPDIR`. Widening the writable set to the per-user temporary directory would
have worked and would also have handed every build script a writable staging
area outside the workspace.

Instead the child's `TMPDIR` is set to `<workspace>/.gyr/tmp`, created before the
run. Doctests then pass and the writable set remains exactly the workspace.
`.gyr` is already ignored by the repository, since the session log lives there.

### 4.3 Offline

**Measured:** with the network denied and no `--offline`, Cargo fails with
`Couldn't resolve host: index.crates.io`, which is honest but reads like a
network fault rather than a policy. The `cargo` tool therefore passes `--offline`
whenever the sandbox denies the network, and the failure becomes a clear
statement that a dependency is not in the local cache.

The consequence is real and worth stating: **a sandboxed agent cannot fetch a
new dependency.** Adding a crate needs a person to run Cargo themselves, or to
run with `--sandbox none`. A network-enabled sandbox mode would need writes to
`CARGO_HOME` so the registry cache could be updated, which hands a build script
the ability to poison that cache for everything else on the machine. That
trade-off deserves its own design rather than a flag added in passing.

## 5. Elsewhere

Until Linux is built, `Sandbox::detect` reports unavailability on every platform
that is not macOS, and a `Process` tool call is refused with a message naming
the reason. `--sandbox none` remains available, is never the default, appears in
the approval prompt as `unconfined`, and is written into the session log's
opening record. A person running unconfined should be able to tell from the log
that they did.

Windows is not a target, per RFC-0001 section 8. That is a decision rather than
a backlog entry, so nothing here is written to leave room for it.

### 5.1 Linux: three decisions, taken 2026-08-27

These were open questions until now. They are recorded here rather than in a
future session's head.

**Mechanism: a `gyr-confine` helper binary. Not `bubblewrap`, and not an unsafe
exemption.**

Landlock applied to a *child* needs `Command::pre_exec`, which is `unsafe` and
the workspace forbids it. That constraint evaporates one level down. A small
second binary can apply a Landlock ruleset **to itself**, apply a seccomp filter
to itself, then `exec` the real program: Landlock restrictions are inherited
across `exec`, and `CommandExt::exec` is safe — it is only `pre_exec` that is
not. So the helper needs no unsafe at all, and it slots into the existing
`Sandbox` trait exactly as `sandbox-exec` does, because `wrap` already returns a
rewritten argument vector.

`bubblewrap` was the obvious alternative and loses on two counts: it is not
installed everywhere, and it needs user namespaces that hardened kernels and
some container runtimes restrict, which would make the boundary depend on the
host's configuration. An unsafe exemption for one file was the third option and
erodes a stated workspace invariant to save shipping a binary.

**Kernel floor: Landlock ABI 4, kernel 6.7 or newer. Nothing older.**

Landlock filesystem confinement arrived in 5.13; network confinement in 6.7.
Supporting 5.13 means blocking `AF_INET` and `AF_INET6` socket creation with
seccomp instead, which is a second mechanism, coarser, and easier to get subtly
wrong around the unix sockets a build legitimately uses.

**One mechanism that can be verified beats two that can each be half-verified**,
and that is the whole argument. The cost is real and worth stating: this
excludes long-term-support server kernels, notably anything on 5.15 or 5.10. On
those, Gyrfalcon refuses to run processes and `--sandbox none` is the recorded
escape, exactly as on any other platform without an implementation.

**Testing: no Linux sandbox code lands until it is exercised on Linux, and the
vehicle is CI rather than hardware.**

This is the actual blocker and it comes first. Writing an unverifiable security
boundary is the thing this repository keeps refusing to do, and Linux is not
where it should start making exceptions. A GitHub Actions `ubuntu-latest` runner
costs nothing, needs no hardware, and runs a recent kernel; whether it runs a
kernel with Landlock in its LSM list is a fact rather than an assumption, so the
first step is a job that reports it.

"Done" for the Linux sandbox requires, on a real kernel, the two tests that
carry the whole claim: a build script writing outside the workspace and being
refused, and a network call returning nothing. Neither can be faked, and passing
unit tests without them would be a green light for the wrong reason.

### 5.2 The order of work

1. CI on both platforms, running what already exists, plus a step that reports
   the runner's kernel version and LSM list. This lands before any sandbox code
   and answers whether the floor above is reachable in CI at all.

   **Answered, 2026-08-27.** The `ubuntu-latest` runner reports kernel
   `6.17.0-1022-azure`, `landlock` in `lockdown,capability,landlock,yama,
   apparmor,ima,evm`, and **Landlock ABI 7** against a floor of 4. The kernel
   floor in section 5.1 is reachable on a hosted runner with no hardware and no
   configuration.

   The same run also caught a genuine cross-platform defect on its first Linux
   build: a test helper gated to macOS by its callers but not by its own
   definition, which is dead code and therefore an error under `-D warnings`
   anywhere the sandbox is unimplemented. That is CI earning its keep before it
   has been asked to do the thing it was added for.
2. `gyr-confine`, with the two escape tests gated to Linux.
3. `Seatbelt` and the new implementation behind the same `detect`, with the
   kernel-floor refusal message naming 6.7.

### 5.3 What was written, 2026-08-27

Steps 2 and 3, on a machine with no Linux on it. This section was written before
CI had returned a verdict, deliberately, so that what follows could not be
tidied up afterwards to match the result.

**The verdict, appended after the fact: all six confinement tests ran on
`ubuntu-latest` and passed.** Not only the escape tests. The pair that matters
is `a_confined_command_cannot_write_outside_the_workspace` passing alongside
`a_confined_command_may_still_write_inside_the_workspace`, because the first
alone would also pass against a sandbox that refused everything. Cargo, rustc
and a build script writing to `OUT_DIR` all run under the ruleset, doctests get
their workspace-local `TMPDIR`, and `curl` returns nothing.

So the Linux boundary is verified rather than asserted, on a real kernel, by a
job that costs nothing and needed no hardware. That was the whole argument for
putting CI before the code.

`gyr-confine` takes `--allow-write <path>...` and `-- <program> [args]`, builds a
Landlock ruleset at ABI 4, grants read on `/` and full access under each
writable path, adds no network rule, restricts itself, and `exec`s. Two details
carry the weight:

- **`CompatLevel::HardRequirement`.** The `landlock` crate defaults to best
  effort, which on an older kernel enforces whatever it can and says nothing.
  The whole point of a floor is that falling below it is an error, so the
  compatibility level is set explicitly and `restrict_self` must report
  `FullyEnforced` or the helper refuses to exec.
- **No unsafe anywhere.** Restricting oneself and then calling
  `CommandExt::exec` are both safe. That was the argument for a helper in
  section 5.1 and it survived contact with the code.

**Amended 2026-08-27.** The helper ships as a second binary target of
`gyr-cli` rather than from its own crate. It had its own crate first, and
`cargo install --path crates/gyr-cli` delivered `gyr` alone: an installed agent
on Linux would have found no helper and refused every process. The argument in
this section was for a helper *process* over in-process `unsafe`, and which
crate builds it is packaging rather than design. CI now installs and runs a
confined write, because that defect was invisible to every test that ran from
`target/`.

`Landlock` in `gyr-sandbox` locates the helper beside the running executable,
then one directory up because a test binary lives in `deps/`, then on `PATH`,
with `GYR_CONFINE` overriding for packaging. A helper that cannot be found is
`Unavailable`; it never degrades into running the command unwrapped.

**The Linux guarantee is weaker than the macOS one, and is labelled as such.**
Landlock ABI 4 restricts TCP bind and connect. It does not restrict UDP, so a
confined child can still reach a DNS resolver and, with effort, talk through it.
The Seatbelt profile denies everything. So the two labels differ:
`workspace (seatbelt: writes confined, network denied)` against
`workspace (landlock: writes confined, TCP denied)`. Making the weaker one wear
the stronger one's words would be the exact failure this document exists to
avoid. Closing the gap means seccomp filtering on `socket(AF_INET, SOCK_DGRAM)`,
which is the second mechanism section 5.1 declined, and it is recorded as a gap
rather than smuggled in.

The three escape tests now run on both platforms rather than only on macOS,
asking for a refusal rather than for one platform's wording, because Seatbelt
refuses with `EPERM` and Landlock with `EACCES`.

If step 1 reports a runner without Landlock, the floor decision stands and the
vehicle changes; the decision that must not change is that the code waits for
the test.

## 6. Command surface

```console
gyr run --sandbox workspace   # the default: writes confined, no network
gyr run --sandbox none        # nothing confined; explicit, prompted, recorded
```

`--read-only` still refuses every `Process` call before the sandbox is
consulted. The sandbox is a second boundary, not a replacement for the first.

## 7. Verification

**Measured on 2026-08-27.** The workspace passes 77 tests and Clippy with
`-D warnings`.

- Profile generation: denies by default, allows reads everywhere, writes only to
  the canonical root and `/dev/null`. A quote or backslash in the path is
  escaped; a control character is refused rather than escaped.
- `Unconfined` returns the command unchanged and reports itself as confining
  nothing, so nothing has to infer containment from a missing wrapper.
- On macOS, the `cargo` tool run inside the sandbox: a build script compiles,
  runs and writes to `OUT_DIR`, and the recorded command carries `--offline`.
- On macOS, a build script that writes outside the workspace: the run fails,
  the file does not appear, and the test asserts the failure text holds both the
  script's own panic message and `Operation not permitted`. A sandbox test that
  passes because the fixture failed to compile is worse than no test.
- On macOS, `cargo test` inside the sandbox passes including doctests, which is
  the workspace-local `TMPDIR` doing its job.
- Platforms with neither implementation: a `cfg`-gated test asserts detection
  reports unavailability naming the operating system, rather than falling back.
- On Linux, the same six confinement tests as macOS, run on `ubuntu-latest`
  (kernel 6.17, Landlock ABI 7): writes outside the workspace refused, writes
  inside allowed, the network unreachable, a build script confined, Cargo and
  doctests working under the ruleset. A helper that cannot be found reports
  unavailable rather than running the command unwrapped.

**Observed on 2026-08-27**, driving the built `gyr` binary against a local fake
endpoint over a fixture with a type error: the approval prompt names the full
command and the containment, the session log's opening record carries
`workspace (seatbelt: writes confined, network denied)`, and the model receives
576 bytes naming `E0308`.

Every platform that is neither macOS nor Linux refuses, and the test that proves
it refuses is the only one there is. That is the intended end state rather than
a gap.

**Observed on 2026-08-27** while designing the profile, using `sandbox-exec`
directly rather than through Gyrfalcon:

- A fixture with a build script compiled and ran, writing to `OUT_DIR`.
- `thiserror`, `syn`, `quote` and `proc-macro2` built from the local registry
  cache with `--offline`, so proc macros execute correctly inside the profile.
- Gyrfalcon's own workspace passed `cargo check --workspace --all-targets`.
- `cargo test` passed, including doctests, once `TMPDIR` was workspace-local.
- Writing to the workspace's parent, and to `~/.ssh`, was refused.
- `curl https://example.com` returned no response.
- Reading `~/.zshrc` succeeded, which is section 2's limitation demonstrated
  rather than described.

## 8. Open questions

- Whether Landlock's open UDP path is worth a seccomp filter, which would mean
  the second mechanism section 5.1 declined, or whether the honest label is
  enough. Section 5.3.
- Whether a network-enabled mode is worth the `CARGO_HOME` write it requires,
  and whether a per-run registry overlay is a better answer than either.
- ~~Whether the Linux implementation should be an external launcher, a small
  setuid-free helper binary, or a narrowly scoped `unsafe` exemption.~~ Decided
  in section 5.1: a helper binary, kernel 6.7 or newer, and not until CI can run
  the escape tests.
- Whether reads should be narrowed once there is an eval corpus to tell us what
  a Rust build actually touches, rather than what we assume it does.

**Amendment, 2026-10-20: protected paths (issue #14).** A session or an eval
case can name workspace paths that nothing may write, such as the tests a task
is judged by. Published work on test tampering found that models editing tests
to pass stop almost entirely when the tests are read-only, and that prompting
does far less. Two layers enforce it:
- The file tools refuse a protected path when they classify the call, before
  any approval is asked for.
- On macOS, Seatbelt adds a `deny file-write*` rule for each path after the
  workspace allow, since the last matching rule wins. Writing, creating,
  renaming in or out and deleting were all checked by hand, and a test runs a
  script under the sandbox to confirm it.

Landlock cannot express this: it only grants, and a grant on the workspace
covers everything beneath it. On Linux the protected paths are copied before
each process starts and put back once it has finished (#22). Anything created
beneath them is removed, and the tool result names what was undone. This
detects rather than prevents: within one command a process can change a test
and run it before the restore. Granting write access per directory instead
would have left no process able to create anything at the workspace root, so a
fresh checkout's first `cargo build` would fail for want of `target/`.
