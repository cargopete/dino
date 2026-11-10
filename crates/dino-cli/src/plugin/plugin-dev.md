---
name: plugin-dev
description: Write, check and install a Dino plugin (a bundle of skills, hooks, MCP servers and instructions). Use when asked to make a plugin, package a skill or hook for reuse, or extend Dino itself.
---

# Writing a Dino plugin

A plugin is a directory with a `dino-plugin.toml` at its root. Work in the
workspace; nothing is installed until the last step, and the person approves it.

## 1. Scaffold

Run `dino plugin new <name>` in the workspace. The name is lower-case letters,
digits and hyphens. It writes `<name>/dino-plugin.toml` and one skill at
`<name>/skills/<name>/SKILL.md`, and passes validation as written.

## 2. Write the contributions

The manifest:

```toml
name = "cargo-tidy"
version = "0.1.0"                 # semver; raise it for every change you install
description = "One line."
engines = ">=0.1"                 # the Dino versions it works with, a semver range
instructions = "INSTRUCTIONS.md"  # optional; read into every session's prompt

[[hooks]]                         # same fields as [[hooks]] in config.toml
event = "after_edit"              # after_edit, before_end_turn, session_start, task_end,
                                  # before_call, after_call, prompt_submit, awaiting_approval
extension = "rs"
command = ["{plugin}/bin/tidy.sh", "{path}"]

[mcp.docs]                        # same fields as [mcp.<name>] in config.toml
command = ["{plugin}/bin/server"]
sandbox = true                    # the default; say why if you turn it off
```

- Skills are folders under `skills/`, each with a `SKILL.md` whose front matter
  has `name:` and `description:`. The description is all the model sees until
  it loads the skill, so say when to use it.
- Slash commands are `commands/<name>.md`: a prompt the person sends with
  `/<name> args`, `$ARGUMENTS` and `$1`, `$2` filled in, and `description:` in
  front matter for `/help`.
- `{plugin}` is the installed bundle's directory. A command that starts with it
  must name a file in the bundle. Make scripts executable.
- Hooks and MCP servers run as the person, in every session. Contribute one
  only when a skill or instructions cannot do the job.
- No symbolic links, nothing outside the bundle.

## 3. Validate

`dino plugin validate <dir>` lists every problem at once, or prints what the
bundle contributes and its `digest`. Fix and repeat until it says `valid`.
Show the person the output.

## 4. Install

Installing writes outside the workspace, so the sandbox refuses it. Run
`dino plugin install <absolute dir> --checksum <digest>` with `unconfined`
true and a justification naming the plugin. The digest must be the one
`validate` just printed: it ties the person's approval to exactly those files.
Never install without the person seeing the validate output first.

## 5. Verify it is live

A plugin loads when a session starts, so this session does not have it.
Run `dino plugin list` (unconfined again; it reads the user's config) and check
the plugin says `loads:` with the counts you expect. `dino prompt` prints the
system prompt a new session would get; its skills list should name the new
skills. Tell the person to start a new session to use it.

To change an installed plugin, edit the workspace copy, raise `version`,
validate and install again. `dino plugin disable <name>` and
`dino plugin uninstall <name>` undo it.
