# RFC-0015: Configuration files

| | |
|---|---|
| Status | implemented M9 |
| Date | 2026-08-27 |
| Depends on | RFC-0006, RFC-0009 |
| Scope | where settings come from, and which of them a repository may set |

## 1. Why now

RFC-0006 refused a configuration file on the grounds that "inventing one before
the command surface has settled would only produce a format to regret". Ten
flags across four commands have now been stable for several slices, so that
condition has been met, which is the honest trigger rather than the itch.

## 2. A project file is untrusted input

This is the whole RFC and the rest is arrangement.

A user file at `~/.config/dancho/config.toml` was written by the person running the
agent. A project file at `<workspace>/.dancho/config.toml` **arrives with the
repository**, which someone else may have written, and which `git clone` will
happily deliver. Treating the two as one thing would mean a cloned repository
could turn off the sandbox, approve every action in advance, and point the agent
at an endpoint of its choosing.

So the two files have different powers.

| Setting | Project file | User file |
|---|---|---|
| `model`, `max_turns`, `show_reasoning`, `no_thinking`, `plain` | yes | yes |
| `approvals` | **refused** | yes |
| `sandbox` | **refused** | yes |
| `api_base` | **refused** | yes |
| credentials | never | never |

`approvals` and `sandbox` are refused from a project file because they weaken a
boundary. `api_base` is refused because it redirects where a credential is sent,
which is the same problem wearing a different hat: a repository that could set
it could collect your key.

A project file that names a restricted key is an **error naming the key**, not a
warning and not a silent drop. A person who cloned something that tried this
should be told.

**No file may hold a credential.** Not the user's either. An API key in a
configuration file is a key that will be committed, copied into a gist, or
included in a bug report; the environment variable already exists and is where a
secret belongs.

## 3. Precedence

Flag, then environment, then project file, then user file, then the built-in
default. The narrower and more deliberate the source, the more it wins.

`workspace`, `log` and `resume` are not configurable. The first is where you
are, the second is per-session, and the third is per-invocation.

## 4. Making it auditable

`dancho config` prints every setting, its resolved value, and **where that value
came from**. Precedence that cannot be inspected is precedence that gets blamed
for the wrong things, and a person debugging why their agent is unconfined
should be able to see the file that did it.

## 5. Out of scope

Per-directory cascading beyond the workspace root, environment interpolation
inside the file, profiles or named configurations, and any setting that does not
already exist as a flag. A configuration format grows by accident; this one
starts as a mirror of a settled command surface and nothing else.

## 6. Verification

**Measured on 2026-08-27.** The workspace passes 141 tests and Clippy with
`-D warnings`.

- Each restricted key refused from a project file, by name and with the reason.
- The trust split as a lookup rather than only as a check: a `Layers` whose
  project half holds `sandbox` still resolves the user's value, so even a
  restricted setting that somehow reached that struct is never consulted.
- A project file beating a user file for an ordinary setting, and a user file
  read when the project file is silent.
- An unknown key refused, so a typo is an error rather than a setting that
  mysteriously does not apply.
- `api_key` refused, because no such setting exists at any layer.

**Observed**, since flag and environment precedence depend on process-global
state that a parallel test suite should not be writing:

```text
MODEL         claude-sonnet  (project file)
SANDBOX       none  (user file)
MAX TURNS     12  (user file)
PLAIN         true  (project file)
```

then `--model terra` giving `MODEL  terra  (flag)`, and a project file adding
`sandbox = "none"` giving:

```text
dancho: /…/cfg/.dancho/config.toml sets sandbox, which a project file may not:
     it weakens a boundary. Set it in your own config or pass the flag.
```
