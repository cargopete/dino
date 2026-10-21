# RFC-0013: The context budget

| | |
|---|---|
| Status | implemented M7 |
| Date | 2026-08-27 |
| Depends on | RFC-0001, RFC-0003, RFC-0006 |
| Scope | knowing how full the window is, saying so, and elision |

## 1. The failure this fixes

A session grows its history until the provider refuses the request. Every
adapter commits native history only on a terminal event, so a refused request
leaves the same oversized history in place, the next submission resends it, and
it is refused identically. The session does not crash; it stops working, without
saying why, and stays stopped.

Nothing currently tracks how full the window is, although the profile carries
`context_window_tokens` and every turn reports its `input_tokens`. The
information is there and unused.

## 2. Three things, in order of how much they are worth

**Know.** After each turn, compare the reported input tokens against the
model's documented window. That is an estimate of the *last* request, which is a
close enough proxy for the size of the history that produced it.

**Say.** Past a threshold, tell the person. Not the model: injecting a warning
into the conversation would put operator text in the assistant's history, which
RFC-0006 keeps out of prose for the same reason approval does.

**Elide.** Past a higher threshold, reduce the history and record what was
reduced.

The first two are most of the value. A person told "you are at 80% of the
window" can start a new session, which costs them nothing and needs no clever
machinery. Elision is what stops a long session dying mid-task.

## 3. Elision, not summarisation

The history is reduced by **replacing the contents of the oldest tool results
with a marker**, oldest first, until enough has been reclaimed. The tool calls
stay. The assistant's text stays. Only the results are hollowed out.

Summarisation was the obvious alternative and is rejected for this slice. It
needs another model request, which costs money and latency at exactly the moment
a session is already struggling; it is non-deterministic, so a session cannot be
replayed; and it silently rewrites what the model believes happened, which is a
much larger claim than eliding a file listing from forty turns ago.

Tool results are also the right target on the merits: they are usually the
largest thing in a history and the least needed later. A `read` result from
thirty turns back has either been acted on or has not.

The marker names what was removed and how much, so the model can ask again
rather than assume the file was empty. Absent data must not render as healthy
state, and a tool result silently becoming `""` is exactly that.

### 3.1 Amendment, 2026-10-21: long arguments of old calls

A long task disproved "tool results are usually the largest thing". On
`toml-subset-parser` (bench v5, #30), Qwen's history at the end held 156 to
188 KB of its own `write_file` and `apply_patch` arguments against 16 to 19 KB
of elided results. Elision ran nearly every turn, often reclaiming 82 bytes,
and one run ended with nothing left to reclaim.

So elision now also hollows the long strings in the arguments of calls whose
results are being elided: a written file, both sides of a patch. Each string
over the same floor becomes a marker giving its size. The call, its id and
short fields such as the path stay, so the history still says what was done.
The call has run and its effect is on disk, so a `read` recovers anything the
marker removed. `Elided` reports these as `calls_elided`.

The assistant's text is still left alone. One v5 run wrote 140 KB of it, but
text is what the model said, not a copy of something on disk.

## 4. The interface

`ModelSession` gains one method:

```rust
fn elide_tool_results(&mut self, keep_recent: usize) -> Result<Elision, ModelError>;
```

The adapter owns its native history, per RFC-0003, so the adapter does the work.
The core decides when and how much, and receives a report of what happened.

The default implementation returns `ModelError::Configuration`, so a provider
that cannot do this says so rather than silently doing nothing.

**OpenAI Responses cannot do this and will say so.** It continues with
`previous_response_id` and keeps no local history to elide; reducing context
there means abandoning server-side continuation and sending full history
instead, which is a different design with its own costs. Recorded as a gap
rather than papered over: an OpenAI session will still hit the wall, and the
warning in section 2 is what it gets.

## 5. Recording it

Elision is an explicit event, not a silent optimisation:

```rust
AgentEvent::Elided { model_turn, results_elided, bytes_reclaimed }
```

It lands in the session log like everything else, so a transcript that behaves
oddly afterwards can be traced to the moment its history was cut. RFC-0001
section 9 asked for compaction to be an explicit event with the replaced range
recorded; this is that, for the one form of compaction being built.

## 6. Thresholds

Warn at 70% of the documented window, elide at 85%, keeping the most recent
eight tool results intact. All three are `AgentConfig` fields with those
defaults rather than constants, because they are guesses and should be able to
be wrong cheaply.

The two thresholds are whole percentages rather than fractions. That is what a
person setting one would write, and it keeps the arithmetic in integers against
a token count, where a float would need a cast that is either lossy or noisy.

A profile with no documented window disables all of it. Guessing a window and
eliding against the guess would be worse than doing nothing, because the failure
would be a mangled history rather than an honest refusal.

## 7. Out of scope

Summarisation, server-side compaction, OpenAI's continuation problem, eliding
anything other than tool results, a real tokeniser, and any attempt to recover a
session that has already been refused. This prevents the wall; it does not climb
back over it.

The tokeniser deserves a note. Everything here counts tokens the provider
reported for the previous request, which is a lagging measure: a single enormous
tool result can take a session from 60% to over the limit in one turn, and
nothing here will catch that. A local tokeniser would fix it and Gyrfalcon does
not have one, so this is a mitigation rather than a guarantee.

## 8. Verification

**Measured on 2026-08-27.** The workspace passes 133 tests and Clippy with
`-D warnings`.

- A scripted provider crossing 70% warns once and stays quiet on the higher
  turn that follows. A warning repeated every turn is noise, and noise is how a
  real one gets missed.
- Crossing 85% elides, emits the event with what was reclaimed, and asks the
  session to keep exactly `keep_recent_results`.
- A provider that refuses to elide does not fail the run; the warning has
  already been given.
- A profile with no documented window: nothing fires, and the session is never
  asked, even at nine million reported tokens.
- Anthropic and Qwen: the oldest results are hollowed, the newest are untouched,
  an assistant turn between them survives, and every `tool_use_id` and
  `tool_call_id` is preserved, because a provider needs a result for every call
  it issued and an elided result is still a result.
- A result shorter than the marker is left alone: replacing two bytes with a
  sentence reclaims nothing and loses the only thing it had.
- OpenAI reports that it cannot elide rather than returning a successful no-op,
  which would let a caller believe the window had been reclaimed.
