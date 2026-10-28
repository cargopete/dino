# RFC-0016: Replaying a session

| | |
|---|---|
| Status | implemented M10 |
| Date | 2026-08-27 |
| Depends on | RFC-0006, RFC-0007, RFC-0014 |
| Scope | rendering a past session, and the gap that exposed |

## 1. A claim that was never true

RFC-0006 section 5 said the session log's jobs were debugging, approval audit
and the eval corpus, and RFC-0001 section 9 said "presentation is derived from
it". The eval harness made good on the corpus half in RFC-0012. Nothing has ever
derived presentation from the log, and setting out to do so found out why:

**the log has never recorded what the person asked.**

`TurnInput::User` goes to the provider. `AgentEvent` carries model events, tool
decisions, tool calls and tool results. A replay built on that shows a session's
answers with none of its questions, which is not a transcript.

That is a defect in the log rather than in the replay, and it was invisible for
as long as nothing tried to read the log back as a transcript. The eval harness
reads it for metrics and never needed the prompt.

## 2. The fix

`AgentEvent::Submitted { text }`, emitted once at the start of each submission,
before the first request.

It is an agent event rather than a session record because it belongs to the turn
sequence: a reader walking events in order should see the question in its place,
not have to correlate a separate stream by timestamp.

**This changes what the log holds, and that is worth saying plainly.** The file
already contained your source, because tool results are file contents. It now
also contains what you typed. Same directory, same `.dino/`, same git-ignore, and
one more reason the disclosure in RFC-0014 section 4 is not a footnote.

## 3. Replay

```console
dino replay              # the most recent session in this workspace
dino replay <id>         # a named one
dino replay --last 3     # only the last three submissions
```

Rendering goes through the same `Renderer` the live session uses, fed recorded
events instead of streamed ones. That is the point rather than a convenience: a
second renderer would drift, and the first thing it would drift on is the thing
a person is replaying to check.

`--last` exists because the common case is resuming and wanting the recent
context, not an hour of scrollback.

## 4. What replay is not

It is not resumption. RFC-0014 restores the provider's native history so a
conversation can continue; this restores nothing and changes nothing, it reads.
The two are deliberately separate commands, because "show me what happened" and
"carry on from what happened" are different intentions and one of them costs
money.

It is not a debugger. There is no stepping, no filtering by tool, and no way to
see a tool result in full where the renderer capped it. Those are reasonable and
none of them is needed to answer "what did it do".

## 5. Out of scope

Filtering, stepping, colourless machine output beyond the JSONL that is already
there, replaying into a different renderer, and cross-workspace replay.

## 6. Verification

**Measured on 2026-08-27.** The workspace passes 154 tests and Clippy with
`-D warnings`.

Five tests drive the shipped binary against hand-written logs rather than logs a
run produced, so the reader is tested against a fixed input instead of against
whatever the writer emitted that day: a transcript replaying with its header,
question, tool line and answer; `--last` trimming and saying what it hid; a log
predating the `Submitted` event replaying whole rather than empty; a malformed
line naming the file and the line number; and a named session that does not
exist saying so.

**Observed**, replaying a real two-submission session: the questions and answers
came back in order, and `--last 1` showed the final exchange above
`… 2 earlier submission(s) not shown`.

### 6.1 A test that was right to break

Adding the event broke `cancellation_between_tool_calls_stops_before_the_next_turn`,
which asserted a cancelled-before-anything run recorded nothing at all. It now
records the submission and nothing else, because the person did ask and it was
then cancelled. The provider still received nothing, which the same test still
checks.

Recording nothing would have been tidier and less true.
