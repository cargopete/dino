# RFC-0018: Handing a long task to a fresh session

| | |
|---|---|
| Status | built 2026-10-24 as `gyr run --handover`, after §7 step 2 failed (benches v5 to v8); steps 3 and 4 not yet run |
| Date | 2026-10-20 |
| Depends on | RFC-0013, RFC-0014, RFC-0017 |
| Scope | an opt-in mode that splits one task across several short sessions, carried by a progress record |

## 1. The idea

Issue #17. For a small-context model, practitioners report better long tasks
from several short sessions over durable state than from one long transcript.
Each session takes the next item from a plan, verifies it, records what it did
and ends. The next one starts from the record and the workspace, not from the
old conversation.

gyr's default target makes the idea attractive. Qwen3.8-27B is served with a
64k window (RFC-0017 §8.5), and the hard cases take 20 to 40 turns.

## 2. What the bench says today

The bench does not yet show the failure this would fix. Across the three
benches since the harness changes (2026-10-20, 36 Qwen runs):

- **Turn limits:** 3 runs hit the turn limit. Two had already finished and
  their code passes the hidden tests. Only one, a pratt run in v2, ended
  unfinished. The turn countdown (#5) did most of that work.
- **Window:** peak prompts reached 80 to 93% of the window. The one run that
  failed for lack of room did so through a bug (#24), now fixed, not because
  the task outgrew the window.
- **Elision:** one run of the 18 in v3 and v4 needed it, the same pratt run.

So the corpus has no task longer than one session. That is the first thing
to fix (§7), and the reason this RFC is a design to measure rather than a
plan to ship.

## 3. The progress record

A tool, `progress`, rather than a file the model edits with `write_file`:

```json
{"op": "plan", "items": ["parse integers", "precedence", "error offsets"]}
{"op": "done", "item": 2, "note": "precedence climbing; tests in tests/prec.rs"}
{"op": "note", "text": "tried a recursive descent first; it could not handle right associativity"}
```

There are two reasons for a tool:

- **The harness reads it.** When to hand over, and whether anything is left,
  are decisions the core makes. A markdown file would mean parsing prose the
  model was free to reshape.
- **It cannot be broken by a bad patch.** In v1, 25 of Qwen's 29 tool errors
  were `apply_patch` failures.

The record lives at `.gyr/sessions/<chain>/progress.json`, beside the
RFC-0014 state files and ignored by git. Items are pending, done or blocked.
Notes are kept in order. Recording what was tried and why it failed is the
point of notes: a fresh session loses exactly that knowledge, and it is how a
chain repeats its own mistakes.

A `done` is the model's claim, so it is not taken as evidence. The seed in §4
carries the harness's own view of the workspace alongside it.

## 4. Handing over

A session hands over when one of these is reached:

1. **The window:** the point where RFC-0013 would elide, as long as the record
   has a plan. Without a plan there is nothing to hand over, so elision runs
   as it does now.
2. **A per-session turn limit:** default 16.

In the plain loop, an end of turn is the end of the run. In handover mode, an
end of turn with items still pending starts one more session. A second end of
turn without new progress ends the chain, so a chain cannot loop on a model
that keeps stopping early.

The new session gets the same system prompt and tools. Its first message is
built by the harness, not written by the model:

- the original task, verbatim;
- the progress record, rendered;
- why the last session ended (window, turns, or ended early);
- the files changed so far, as `git status --short` would list them;
- the current `cargo check` diagnostics, from the diagnostic gate (RFC-0011).

**The prefix cache survives.** The system prompt and tool definitions are
byte-identical across sessions, so vLLM's automatic prefix caching still
serves them. Only the conversation is new, and that is the part the handover
exists to drop.

## 5. Budget and records

- **Turns** count across the whole chain against `max_model_turns`, so the
  mode cannot buy extra turns. The countdown is chain-wide.
- **Sessions** are capped at 4 per chain.
- **One log per chain.** A new event marks each boundary:

  ```rust
  AgentEvent::HandedOver { model_turn, session: u32, reason: HandoverReason, pending: usize }
  ```

  A reader walking the log meets the boundary in its place, as with
  `Elided` and `TurnCutOff`.
- **RFC-0014 state** is written per session. `--resume` resumes the latest
  session in the chain.
- **Opt-in only.** The mode is `--handover` on `gyr run` and `gyr eval`.
  Interactive sessions stay out of scope until the one-shot form has numbers.

## 6. Not summarisation

RFC-0013 §3 rejected summarisation: an extra model request, not
deterministic, and a rewrite of what the model believes happened. This mode
is different in the way that matters. The harness never summarises. The model
writes its notes during the work, as tool calls in the log, and the seed is
built deterministically from those notes and the workspace. Replaying a chain
reproduces it.

## 7. Measuring before building

1. **Add one or two long cases to the corpus.** Each should need more than one
   64k window or more than 40 turns for Qwen: a feature with several
   independent parts and hidden tests per part. The candidate is a small TOML
   subset parser (tables, arrays, inline tables, string escapes, error
   positions).
2. **Run the plain loop on them, three trials.** If it passes them, stop here
   and close #17 with the numbers.
3. **If it fails, build this and run both arms.** Each arm gets three trials,
   on the long cases and on the six hard cases. Use a 32k window as a second
   condition, where the plain loop is forced to elide.
4. **Adopt it as a default only if** pass rate rises on the long cases, falls
   on none of the hard ones, and total wall time grows by no more than 20%.

Step 2 costs one GPU-hour or two, about $3. Steps 3 and 4 cost about $15.

## 8. Out of scope

- Interactive sessions.
- Harness-written summaries.
- Splitting a task the model has not planned.
- Parallel sessions.
- Any handover for the OpenAI adapter, whose continuation (RFC-0013 §4) keeps
  no local history to hand over from.

## 9. Risks

- **Planning is a new skill to get wrong.** A 27B model may plan badly: too
  coarse, too fine, or in an order that does not build. The seed's
  diagnostics catch a broken build, but they do not catch a bad plan.
- **Notes cost turns.** If the model writes notes instead of code, the chain
  is slower than the loop it replaces. §7 step 4's wall-time bound is there
  for that.
- **The record is model-written state the next session trusts.** A `done` the
  model never verified becomes a fact in the seed. Putting the diagnostics
  beside it is the only check, and a passing build does not mean a finished
  item.
