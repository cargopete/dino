# RFC-0005: Workspace filesystem tools

| | |
|---|---|
| Status | implemented M0 subset; wired in M1; `list` added M6 |
| Date | 2026-08-23 |
| Depends on | RFC-0001 |

## 1. Decision

The first tool slice is a separate `gyr-tools` crate with three operations:

- `read`, for bounded numbered ranges from UTF-8 files;
- `search`, for bounded literal search through ignored-aware workspace files;
- `apply_patch`, for one exact replacement in an existing UTF-8 file.

The name `apply_patch` describes the model-facing edit operation, but the M0
schema is deliberately exact search and replacement rather than unified diff.
This resolves RFC-0001's initial format question for the first eval corpus. A
unified-diff variant may be evaluated later against the same stale-content and
path rules.

The crate implements `gyr-core::ToolRuntime`. It was not wired into a live CLI
agent until the approval layer existed; RFC-0006 supplied that layer, and
`WorkspaceTools` now also classifies each call so a policy can decide it.

## 2. Root confinement

`WorkspaceTools` canonicalises one existing directory at construction. Every
model path must be relative, non-empty and free of parent, root or platform
prefix components. Existing targets are canonicalised and required to remain
beneath the canonical root.

**Amended 2026-08-27.** This rule now lives in `gyr-core::workspace` because
`exec` needs the same one for its working directory, and two implementations of
one security check is how they come to disagree. The tests below did not move;
they now cover the shared implementation.

Directory walking never follows symbolic links. Direct read and edit calls do
canonicalise symbolic links, then reject a target outside the root. Tests cover
both `..` traversal and an in-root symlink to an outside file.

This is a filesystem fence, not a process sandbox. There remains a time-of-check
to time-of-use window if another process replaces a directory component during
an operation. The later sandbox layer must enforce an operating-system boundary
as well. Calling the present crate a sandbox would only make the race condition
feel more official.

## 3. Read contract

`read` accepts a path and optional one-based start and end lines. Default hard
limits are 200 lines and 32 KiB of returned content. Output is JSON containing:

- the requested path and actual line bounds;
- total line count;
- numbered content;
- explicit truncation state;
- SHA-256 of the complete file bytes.

The fingerprint covers the whole file even when returned content is truncated.
Invalid UTF-8 is rejected rather than transformed. Byte truncation stops at a
valid UTF-8 boundary.

## 4. Search contract

`search` is literal rather than regular-expression search. It returns path,
line, byte column and complete matching line. Default limits are 200 returned
matches, 64 KiB of encoded matches, 20,000 scanned files and 2 MiB per file.
The scan continues after the result cap where possible so `total_matches`
describes the scanned corpus rather than merely the displayed prefix. Hitting
the file cap, match cap or byte cap sets `truncated`.

The implementation uses `ignore::WalkBuilder` with standard `.gitignore`,
`.ignore`, repository exclude and hidden-file filters. Parent ignore discovery
is disabled so a developer's unrelated parent directory cannot quietly change
the repository tool contract. Git ignore rules apply even in a temporary
workspace without a `.git` directory. Symlink traversal is disabled and paths
are sorted for deterministic output. Non-UTF-8 files are skipped.

**Dependency documentation inspected on 2026-08-23:** `ignore` 0.4.33 documents
that standard filters include hidden files, `.ignore`, `.gitignore`, global Git
ignores and `.git/info/exclude`; symbolic-link following is disabled by default.

Source:

- <https://docs.rs/ignore/0.4.33/ignore/struct.WalkBuilder.html>

## 5. Exact patch contract

The model must provide `path`, `expected_sha256`, non-empty `old_text` and
`new_text`. The operation proceeds only when:

1. the current complete file hash matches the value returned by a prior read;
2. old and new text differ;
3. `old_text` occurs exactly once.

The replacement is written to a uniquely created sibling temporary file with
the original permissions, flushed with `sync_all`, then renamed over the
target. A failed replacement attempts to remove its temporary file. The parent
directory is not yet synchronised, so the current guarantee is atomic visibility
on supported local filesystems, not crash durability across sudden power loss.
Windows replacement semantics are not claimed, and Windows is not a target.

The result reports before and after SHA-256 values and final byte length. It
does not manufacture a successful edit when the replacement is absent,
ambiguous or byte-identical.

**Dependency documentation inspected on 2026-08-23:** SHA-256 is supplied by
RustCrypto `sha2` 0.11.0. The digest is a freshness token, not an authentication
or signature scheme.

Source:

- <https://docs.rs/sha2/0.11.0/sha2/>

## 6. The `list` tool

**Added 2026-08-27, on evidence rather than on a hunch.** RFC-0012's first
six-case corpus run caught the model reaching for
`exec find . -name "*.rs"`. `search` finds text and `read` reads a known path;
neither answers "what files are here", so a shell was being used to get a
capability the tool surface lacked rather than a syntax it lacked.

`list` takes an optional `path` and an optional `depth`, walks with the same
`ignore` rules `search` uses, and returns each entry's workspace-relative path,
whether it is a file or a directory, and a file's byte length. It is
`ToolClass::ReadOnly`.

Using the search walk rather than a plain directory read is the point. A listing
that included `target/` would be mostly build output and would cost more context
than it returned, which is why this is not `exec ls` with a nicer name.

Default limits are 500 entries and 32 KiB. `total_entries` counts everything the
walk saw, so a capped listing never reads as a complete one, which is the same
rule the other three tools follow.

**Measured on 2026-08-27**, by re-running the same six-case corpus against the
same model with only this tool added:

| | before | after |
|---|---:|---:|
| `exec` calls across the corpus | 1 | **0** |
| `list` calls across the corpus | 0 | **2** |
| cases passing | 6/6 | 6/6 |
| input tokens | 112,942 | 112,003 |

`list` was called in two cases rather than the one that had reached for `exec`.
In `fix-failing-test` the model previously began with `cargo test` and worked
backwards; it now begins with `list`, and drops one of its three `cargo test`
runs. So the missing capability was costing something in a case where nobody had
noticed it missing.

The listing it received in `rename-across-files` was five entries with byte
lengths and no `target/`, from a `depth: 3` call on `.`. That is the argument for
the ignore-aware walk in one line: `exec ls -R` would have returned the build
directory and cost more context than it returned.

### 6.1 What the listing hides, and how to ask

**Measured on 2026-08-27.** RFC-0012's ablation runs caught the model calling
`exec find . -maxdepth 3 -name .gyr` in a run where `list` was available and had
already been used.

That is not the model preferring a shell. `list` walks with the search rules, so
it cannot show `.gyr`, `target/`, `.gitignore` or anything else hidden or
ignored, and there was no way to ask it to. The property that keeps a listing
from being mostly build output also makes it blind, and a model that wants to
know about those paths has no tool but `exec`.

`list` therefore takes `all`, which turns the ignore filters off, and reports
the flag in its output so an absence in an ordinary listing reads as "not shown"
rather than "not there".

**This was built on one observation, where RFC-0012 declined to build `list`
itself on one.** The standard differs because the questions differ. "Does a
model want this capability" is behavioural and needs evidence. "Can `list`
express this" is structural, is answered by reading the code, and the answer was
no.

## 7. Verification and remaining work

Fifteen deterministic temporary-workspace tests cover bounded reading, hashing,
ignore-aware search, ambiguous edits, stale edits, parent traversal, symlink
escape, the classification rules RFC-0006 added, and `list`: its ignore
awareness, its path and depth narrowing, its entry cap, and its refusal to
escape the workspace.

Approvals arrived with RFC-0006. This slice still does not include OS
sandboxing, process execution, structured Cargo diagnostics, cancellation of a
long search, binary-file reads, new-file creation or a rendered diff. Those are
missing features rather than properties delegated to the system prompt.

**Amendment, 2026-10-20.** New-file creation arrived as `write_file` (issue #4).
The hard bench showed why: Qwen3.8-27B wrote about twenty files through `exec`
with `python3 -c` and shell heredocs, and once gave up and put its tests in
`src/lib.rs`. The tool creates missing directories, refuses to overwrite a file
without its current SHA-256, and resolves the path through its deepest existing
ancestor, so a symbolic link cannot carry a write outside the workspace.

**Amendment, 2026-10-20: what "stale" means.** Section 5 required the exact
SHA-256 on every patch. In the hard bench that check caused 49 of Qwen's 64 tool
errors:
- 39 named a hash the file really had earlier. These were mostly a second patch
  in one turn, refused because of the first.
- 10 were hashes copied wrongly or invented.

The tools now remember the fingerprint each file had when the model last read
or wrote it. A patch or replacement goes ahead when the file still has that
fingerprint, or when the model names it (a prefix of eight or more characters
will do). `expected_sha256` is therefore optional for a file the model has seen.
What the check exists to catch is unchanged: a file altered by anything other
than the model's own edits, such as a formatter, a build script or a person, is
refused as stale until it is read again (issues #2 and #3).
