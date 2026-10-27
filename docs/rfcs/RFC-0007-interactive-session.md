# RFC-0007: The interactive session

| | |
|---|---|
| Status | implemented M4 |
| Date | 2026-08-27 |
| Depends on | RFC-0001, RFC-0006, RFC-0009, RFC-0010 |
| Scope | the session loop, line rendering, the palette, input, cancellation |

## 1. Decision

`dancho` with no subcommand opens a persistent session. `dancho run` stays exactly as
it is: one submission, one exit code, no terminal required.

Two entry points rather than one, because they serve different things. A session
is where a person and a model actually work; a one-shot is what an eval harness,
a CI job and a shell pipeline can call. Collapsing them would mean either a
scriptable thing wearing a prompt or an interactive thing that cannot be
scripted.

The session is **line-based, not an alternate-screen interface.** RFC-0001
section 4.1 said a line UI comes before a full TUI, and having now looked at the
alternative I would keep that order even without the instruction. An
alternate-screen TUI takes the transcript out of the terminal's scrollback,
which is where a person goes to find what the agent did an hour ago, and it
breaks selection and copying in most terminals. The transcript is the artefact.
It stays where the shell puts it.

## 2. The palette

The colours are the house palette, unchanged, and are not open for redesign
here. Their provenance and their rules live in the `cargopete-style` skill.

| Token | Value | Job in a terminal |
|---|---|---|
| `--accent` | `#8bb8dc` | the machine's voice |
| `--rust` | `#cd8560` | the person's voice |
| `--text` | `#ece9e3` | model prose |
| `--text-muted` | `#9c978c` | chrome that carries meaning |
| `--text-faint` | `#6e6a61` | labels a reader can lose |
| `--ok` | `#86b592` | a thing that worked |
| `--warn` | `#cdb06a` | a thing that stopped early |

The load-bearing rule survives the translation intact:

> If a shell printed it, it is slate. If a person wrote it, it is terracotta.

So tool invocation lines, streamed model text and the status line are slate and
ink. The approval prompt, the echo of what a person typed, and section kickers
are terracotta, because those are the moments a person is being addressed or
quoted.

**One thing does not translate, and pretending otherwise would be the kind of
lie this style exists to avoid.** `--bg: #171614` is a page canvas. A line-based
terminal program does not own its background; the terminal does. DanchoAI
therefore ships the ink and the accents and leaves the ground alone. The palette
assumes a warm dark terminal, which is what it was drawn for. On a light
terminal it will read badly, and the honest answer to that is `--plain`, not a
guess at what the ground is.

`--text-faint` measures 3.36:1 on the intended ground and fails AA. It is used
exactly as the style permits: for the window-chrome equivalents, never for
anything a reader would miss.

### 2.1 Colour depth

Truecolor when `COLORTERM` says `truecolor` or `24bit`, so the palette is exact.
Otherwise the nearest xterm-256 approximations, which are close enough that the
difference is hard to see: 110 for slate, 173 for terracotta, 179 for warn, 108
for ok. Otherwise nothing at all, which is also what `NO_COLOR` and `--plain`
select, and what a non-terminal destination gets.

## 3. The loop

```text
prompt  →  submission  →  model turn(s) and tools  →  summary  →  prompt
```

The provider session already retains native history, so a session is the same
`Agent` driven repeatedly rather than a new mechanism. Nothing in `dancho-core`
changes.

The session log gains a shape rather than a feature: one `started` record for
the session, then per submission a run of events and one `finished` record. A
`finished` record therefore means "this submission ended", not "the process
exited", and that is written down here because a reader of the log would
otherwise be entitled to assume the second.

## 4. Input

`rustyline` supplies line editing, history and the usual key bindings, because
hand-rolling a line editor is a week of work to arrive somewhere worse. History
persists to `.dancho/history` beside the session logs.

A line beginning with `/` is a command rather than a submission. The set is
deliberately small:

| | |
|---|---|
| `/help` | what these are |
| `/status` | model, workspace, containment, approval mode, tokens so far |
| `/log` | the path of this session's log |
| `/exit` | leave |

`/model` is absent on purpose. Switching model mid-session means discarding the
provider's native history, and a command that silently throws away the
conversation is worse than not having the command. When there is a designed
answer for carrying context across providers it can have one.

## 5. Cancellation

Ctrl-C during a turn cancels that turn and returns to the prompt. The run ends
with `StopReason::Cancelled`, no terminal event is fabricated, and the provider's
history is untouched because every adapter commits only on a terminal event.
The next submission continues from the last completed turn.

Ctrl-C at an idle prompt clears the line, which is what a shell does and what a
person's fingers expect. Ctrl-D, and `/exit`, leave.

Each turn gets its own cancellation token and its own signal task, and the task
is aborted when the turn ends so a stale handler cannot cancel the next one.

## 6. Out of scope

An alternate-screen interface, a persistent bottom status bar, mouse support,
syntax highlighting of model output, rendered diffs, image display, conversation
persistence across process restarts, and `/model`.

A persistent status bar deserves a sentence rather than silence: it needs
raw-mode cursor management that fights the scrollback this RFC just chose to
keep. `/status` prints the same information on request, which costs a person one
command and costs the design nothing.

## 7. Verification

**Measured on 2026-08-27.** The workspace passes 99 tests and Clippy with
`-D warnings`.

- Command parsing, including the case that caught a real bug. The first rule was
  "a leading slash followed by a letter", and it ate
  `/usr/bin/env is on PATH, is that a problem?`, which is a perfectly reasonable
  thing to ask an agent. A first word containing a second slash is now a path and
  therefore a submission. A lone unknown word is still a mistyped command and
  gets a correction rather than being quietly sent to a model, and a known
  command word wins over whatever follows it, as in any shell.
- The palette pinned to its mandated values, so the implementation cannot drift
  from the style it implements, and rendered at all three depths through a
  function that takes the depth rather than reading a global. The first version
  of that test passed because the global happened to default to off, which is a
  green light for the wrong reason.
- `--plain` emitting nothing at all. A smoke run caught it emitting a bold
  sequence: colour was suppressed and the attribute was not, which is not plain
  but untidy.

**Observed on 2026-08-27**, driving the built `dancho` binary against a local fake
endpoint with four lines piped at it:

- Two submissions and a `/status` between them. The second request carried
  `[system, user, assistant, user]`, so the conversation continued rather than
  restarting.
- `/status` reported `100 in, 5 out` after the first turn and `300 in, 10 out`
  after the second, which is the same running total the turn summary prints
  because they read one shared tally rather than keeping two that could
  disagree.
- Truecolor sequences emitted for the exact palette, terracotta on the kickers
  and slate on the machine's lines.

### 7.1 Multi-line input, answered 2026-08-27

The open question below asked what a pasted stack trace does today. The answer
was measured before anything was built, and it turned out to be "the right
thing".

**Pasting already worked.** rustyline enables bracketed paste by default and
collects a whole pasted block, normalising `\r\n` to `\n`, into one buffer.
Verified through a real pseudo-terminal rather than by reading: a three-line
panic pasted into a session reached the provider as **one** request whose user
message held three lines. Not three requests.

That is worth recording because the alternative was building a fix for a problem
that did not exist, and it was one afternoon away from happening.

**Typing several lines did not work**, and now does: a line ending in a
backslash continues, which is the convention fingers already have. The check is
for an *odd* number of trailing backslashes, so a Windows path ending in an
escaped one is not a continuation that silently swallows the next line.

Verified the same way: three typed lines, two continuations, one request holding
three lines with the backslashes gone.

## 8. Open questions

- ~~Whether a submission spanning several lines wants an explicit terminator or
  bracketed paste, and what a pasted stack trace should do today.~~ Answered in
  section 7.1: paste already did the right thing, and typed lines continue on a
  trailing backslash.
- Whether `/status` should show a context-window estimate, which needs a
  tokeniser DanchoAI does not currently have and should not guess at.
- Whether the session should offer to resume the previous session's transcript,
  which needs the log to be a replay source and it is not one yet.
