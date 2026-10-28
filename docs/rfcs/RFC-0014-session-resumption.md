# RFC-0014: Resuming a session

| | |
|---|---|
| Status | implemented M8 |
| Date | 2026-08-27 |
| Depends on | RFC-0001, RFC-0003, RFC-0006, RFC-0013 |
| Scope | persisting native provider state, and continuing from it |

## 1. The failure this fixes

Closing the terminal loses the conversation. There is no way back to a session
from an hour ago, so any interruption means starting again and re-establishing
context the model already had.

RFC-0001 section 9 promised the recovery path — "provider-side continuation IDs
are an optimisation and may expire; replayable local history remains the
recovery path" — and never built it.

## 2. The session log is the wrong thing to restore from

The obvious move is to replay the session log, and it does not work.

RFC-0003 section 1 puts native conversation history inside the adapter on
purpose, so continuation does not lose reasoning items or provider-specific
content ordering. The log holds **normalised** events, which is the lossy view.
Rebuilding an Anthropic content-block sequence or a Qwen tool-call shape from
`AgentEvent` values would reconstruct something plausible rather than something
identical, and RFC-0003 exists to refuse exactly that.

So state is persisted separately, by the adapter that owns it, exactly as
RFC-0001 section 9 says: "native provider continuation state is stored
separately and versioned".

## 3. The interface

```rust
pub struct SessionState {
    pub provider: ProviderKind,
    pub model_key: String,
    pub version: u32,
    pub payload: Value,
}
```

`ModelSession` gains an export and a restore. The adapter serialises its own
history into `payload` and never explains it to the core, which stores and
returns bytes it does not interpret.

Restore is a method rather than a constructor, so it works through a trait
object: build the session as usual, then hand it its past.

**Three refusals, and all three are the point:**

- A `provider` that does not match refuses. Anthropic history in a Qwen session
  is not a degraded conversation, it is a corrupt one.
- A `model_key` that does not match refuses. Continuing an Opus conversation on
  Sonnet is a decision a person should make deliberately, and silently is not
  deliberately.
- A `version` that does not match refuses. When a payload shape changes, an old
  file must fail loudly rather than deserialise into something almost right.

## 4. What this costs in honesty

**The state file holds the conversation, and the conversation holds your
source.** Tool results are in there: file contents, search hits, compiler
output. So is whatever reasoning the provider returned and the adapter retained.
It is written to `.dino/sessions/<id>.state.json`, which is inside the workspace
and already ignored by git, with owner-only permissions.

That is a real disclosure rather than a footnote. A person who would not commit
their tool output to a repository should know it is on disk beside it.

**OpenAI can export and may fail to restore.** Its state is a
`previous_response_id`, which is trivially serialisable and which the provider
may have expired by the time it is used. That failure surfaces on the next
request, in the provider's words, and it is the case RFC-0001 section 9
anticipated when it called continuation IDs an optimisation.

## 5. What is not restored

The transcript is not reprinted. The token tally starts from zero. Neither is
hard to add and both are presentation, and a resumed session that scrolled an
hour of old output past you before accepting input would be worse than one that
did not.

The log is appended to rather than replaced, with a record saying the session
was resumed, so one file remains one conversation.

## 6. Command surface

```console
dino --resume            # the most recent session in this workspace
dino --resume <id>       # a named one
```

`dino run` writes state too, so a one-shot can be picked up interactively. State
is written after each completed submission rather than continuously, because a
submission is the unit that either happened or did not.

## 7. Out of scope

Restoring the transcript to the screen, cross-workspace resumption, resuming
into a different model, pruning old state files, and any attempt to reconstruct
state from the session log, which section 2 argues is not possible honestly.

## 8. Verification

**Measured on 2026-08-27.** The workspace passes 137 tests and Clippy with
`-D warnings`.

- Anthropic and Qwen round trips assert the **next request body is identical**
  to the one the original session would have sent, not merely that the payload
  survived. Anything weaker proves serialisation rather than continuation.
- A mismatched provider, model and version each refused, each naming why.
- OpenAI's continuation handle round-tripping.

**Observed**, driving the built binary against a local fake endpoint: one
process asked a question and exited; a second process started with `--resume`
sent `[system, user, assistant, user]`. The conversation crossed a process
boundary. The state file is mode `0600`.

The three refusals, each in one line:

```text
dino: model configuration is invalid: this state belongs to a Qwen session, not Anthropic
dino: no session "nonsense" in /…/ws7
dino: no session to resume in /…/ws8; run without --resume to start one
```

### 8.1 A round-trip bug the tests caught

`ChatMessage` skipped its empty fields on the way out, which is correct for the
wire, and had no defaults on the way back in. A message that legitimately had no
tool calls therefore serialised cleanly and then failed to read. It would have
failed the first time anyone resumed a Qwen session and never once before.

That is the argument for asserting on the reconstructed request rather than on
the payload: the payload was fine.
