# RFC-0003: Provider protocol and model matrix

| | |
|---|---|
| Status | accepted for M0 |
| Date | 2026-08-23 |
| Depends on | RFC-0001 |

## 1. Decision

Gyrfalcon defines a normalised stream of model events but does not define a
normalised stored model conversation. Each provider adapter owns its native
session history and continuation handles.

This boundary shares what the agent core genuinely needs:

- streamed user-visible text;
- streamed reasoning summaries when the provider exposes them;
- complete typed tool calls and their call IDs;
- token usage;
- stop reason;
- cancellation and errors.

It does not pretend these provider concepts are identical:

- role and content-block ordering;
- opaque or encrypted reasoning items;
- server-side conversation and previous-response identifiers;
- tool argument delta formats;
- compaction protocols;
- reasoning controls;
- prompt caching;
- authentication and retry semantics.

## 2. Core contract

`ModelSession::next` accepts either a new user message or the results of the
preceding tool calls. It returns a cancellable stream of `ModelEvent` values.
The session mutates its private native history only when it has enough provider
data to do so correctly.

The initial event vocabulary is:

- `Started`;
- `TextDelta`;
- `ReasoningDelta`;
- `ToolCallStarted`;
- `ToolCallArgumentsDelta`;
- `ToolCallCompleted`;
- `Usage`;
- `Finished`.

Adapters may buffer provider deltas until a complete JSON argument value can be
validated. The core dispatches only `ToolCallCompleted`. Streaming argument
deltas are presentation data, never executable authority.

## 3. Model catalogue

| Model | Protocol | Reasoning | Native context | Required parser |
|---|---|---:|---:|---|
| `gpt-5.6-terra` | OpenAI Responses | configurable | 1,050,000 | provider-native |
| `claude-opus-5` | Anthropic Messages | low through max | 1,000,000 | provider-native |
| `claude-sonnet-5` | Anthropic Messages | low through max | 1,000,000 | provider-native |
| `Qwen3-Coder-480B-A35B-Instruct` | Qwen Chat Completions | no | 262,144 | `qwen3_coder` |
| `Qwen3-Coder-Next` | Qwen Chat Completions | no | 262,144 | `qwen3_coder` |
| `Qwen3-Coder-30B-A3B-Instruct` | Qwen Chat Completions | no | 262,144 | `qwen3_coder` |
| `Qwen3.6-27B` | Qwen Chat Completions | yes | 262,144 | `qwen3` + `qwen3_coder` |
| `Qwen3.6-35B-A3B` | Qwen Chat Completions | yes | 262,144 | `qwen3` + `qwen3_coder` |

The built-in Anthropic profile follows the current `claude-opus-5` alias. A
session still carries the provider model identifier explicitly so deployments
can pin a snapshot or test another compatible model without changing the
transport.

### 3.1 Development targets

**Amended 2026-08-27.** Section 7 ends by saying a profile is not marked
supported until a real model run has completed the behavioural evals. That is
now a field rather than a sentence: every `ModelProfile` carries a
`ProfileStatus` of `supported` or `development`, and `supported_profiles()`
returns only the former. The catalogue test above pins the supported set, so a
development entry cannot quietly join it.

**Amended 2026-10-19** by RFC-0017. The amendment above was applied to one
profile and not to the rest: `terra` and the five Qwen coding profiles were
marked `supported` without a live run. They are now `development`, and only the
two Claude profiles, which have passed the corpus, are `supported`.

| Model | Protocol | Reasoning | Native context | Status |
|---|---|---:|---:|---|
| `qwen3:8b` | Qwen Chat Completions | toggle | 32,768 | development |

`qwen3:8b` exists so the loop, the tool round trip and the approval path can be
driven by real inference on a laptop. It is not a coding target, it has not been
through the conformance suite, and `gyr models` says "development" beside
it.

**Not measured.** The identifier is Ollama's tag format, and Ollama is not a
supported serving stack: RFC-0001 admits local runtimes only once they pass the
same conformance suite. Two consequences are worth writing down before somebody
rediscovers them at a keyboard. Ollama serves a default context far below the
model's native window unless `num_ctx` is raised, which silently truncates the
system prompt and tool schemas rather than failing. And Ollama's
OpenAI-compatible surface is not known to honour `chat_template_kwargs`, so
`--no-thinking` may be sent and ignored. The request Gyrfalcon sends has been
verified locally; what Ollama does with it has not.

## 4. OpenAI Responses

**Provider-documented on 2026-08-23:** `gpt-5.6-terra` supports the Responses
endpoint, streaming, function calling, structured outputs and reasoning effort
from `none` through `max`. The Responses API can continue with
`previous_response_id`, permits parallel tool calls, and returns ordered output
items rather than promising that output index zero is assistant text.

The adapter therefore:

- parses the complete response-item lifecycle;
- retains every continuation item needed by the selected storage mode;
- correlates function call outputs by `call_id`;
- never assumes the first output item is text;
- repeats instructions on continued responses because previous instructions
  are not automatically carried with `previous_response_id`;
- reports server token usage separately from locally estimated context size.

The terminal authority is `response.completed`, `response.incomplete` or the
corresponding failure/cancellation event, not the transport connection closing
and not an assumed `[DONE]` sentinel. Malformed SSE JSON is a hard protocol
error. Silently discarding one malformed delta can produce a plausible but
corrupted answer, then leave the consumer waiting for state which was thrown
away. The public Codex parser and its reported malformed-chunk failure mode
made this an explicit conformance requirement rather than a logging detail.

Authentication has two planned sources: API key first, then the public Codex
ChatGPT browser/device flow. Authentication is independent of this transport.

Sources:

- <https://developers.openai.com/api/docs/models/gpt-5.6-terra>
- <https://developers.openai.com/api/reference/cli/resources/responses/methods/create>
- <https://github.com/openai/codex>
- <https://github.com/openai/codex/blob/main/codex-rs/codex-api/src/sse/responses.rs>
- <https://github.com/openai/codex/issues/31148>

## 5. Anthropic Messages

**Provider-documented on 2026-08-23:** Claude Opus 5 has a 1,000,000-token
context window and a 128,000-token maximum output. Current Claude models use
adaptive thinking and `output_config.effort`. Opus 5 accepts `low`, `medium`,
`high`, `xhigh` and `max`; thinking cannot be disabled at `xhigh` or `max`.

The adapter uses Messages API version `2023-06-01`, retains native assistant
content blocks and sends client tool results as the immediately following user
message. That message contains only `tool_result` blocks. Opaque thinking
signatures are accumulated from `signature_delta` events. Ordinary `thinking`
and safety-redacted `redacted_thinking` blocks are replayed unchanged on the
next turn. Direct tool caller metadata is retained as well. Losing or altering
these values is a protocol error, not an invitation to make up a replacement.

Streamed tool input arrives as fragments in `input_json_delta.partial_json`.
The adapter emits presentation deltas as they arrive, then parses the complete
value at `content_block_stop` before authorising a tool call. A malformed or
unfinished value is rejected. History is committed only after `message_stop`.

`tool_use` requests another client turn. `end_turn` and `stop_sequence` map to
normal completion; `max_tokens` and `model_context_window_exceeded` map to the
bounded-output outcome; `refusal` remains distinct. `pause_turn` is rejected in
the MVP because it requires continuation of provider server tools, which the
client-only tool surface does not enable.

Anthropic reports ordinary, cache-creation and cache-read input tokens
separately. Gyrfalcon's normalised input total is their sum, while cache-read
tokens are also retained in `cached_input_tokens`. Reasoning token usage is not
reported separately by this protocol and remains zero rather than being
estimated.

The transport has parser fixtures and an in-process TCP conformance test which
checks the request path, `anthropic-version` and `x-api-key` headers, request
body, SSE lifecycle and final stop reason. This is transport verification, not
a claim that the repository has exercised the adapter against Anthropic's live
API.

Reasoning and text deltas remain different model events. Invalid partial tool
JSON is never repaired silently. If fine-grained tool streaming is later
enabled, the adapter accumulates and validates it before emitting a completed
call.

Sources:

- <https://platform.claude.com/docs/en/agents-and-tools/tool-use/how-tool-use-works>
- <https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls>
- <https://platform.claude.com/docs/en/build-with-claude/streaming>
- <https://platform.claude.com/docs/en/about-claude/models/overview>
- <https://platform.claude.com/docs/en/about-claude/models/whats-new-opus-5>
- <https://platform.claude.com/docs/en/build-with-claude/effort>
- <https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons>
- <https://platform.claude.com/docs/en/about-claude/models/extended-thinking-models>

## 6. Qwen Chat Completions

Qwen's HTTP envelope is OpenAI-compatible Chat Completions. Its behavioural
contract is not the OpenAI Responses contract.

The reference servers are vLLM and SGLang. Startup configuration is part of
conformance: an endpoint which omits the required tool parser is not considered
a supported Qwen endpoint merely because `/v1/chat/completions` answers 200.

Two initial profiles are required:

### 6.1 Coder non-thinking profile

Applies to the 480B Coder, Coder Next and 30B Coder. It enables automatic tool
choice and `qwen3_coder` parsing, does not request or expect reasoning content,
and uses each model card's sampling defaults.

### 6.2 Qwen3.6 reasoning profile

Applies to 27B and 35B-A3B. It enables the `qwen3` reasoning parser and
`qwen3_coder` tool parser. Thinking is an explicit request setting. The adapter
handles `reasoning_content` as a provider extension and never folds it into
assistant text.

The first product surface is text and code. Qwen3.6 can be served with its
vision encoder disabled to reserve memory for KV cache; multimodal support is a
separate capability and eval decision.

**Provider-documented on 2026-08-23:** the Qwen3.6 model cards recommend
`temperature=0.6`, `top_p=0.95`, `top_k=20`, `min_p=0.0`,
`presence_penalty=0.0` and `repetition_penalty=1.0` for precise coding in
thinking mode. Non-thinking mode has its own `0.7/0.8/20` set with presence
penalty `1.5`. Gyrfalcon records both and permits an explicit session override
rather than quietly using one set for both modes.

Qwen3.6 normally retains only the latest thinking block. Its documented
`preserve_thinking` chat-template option retains historical reasoning for agent
work and may improve prefix-cache reuse. The adapter enables this by default,
keeps `reasoning_content` distinct from visible text, and stores the native
assistant message only after a complete `[DONE]`-terminated stream.

The Coder 480B and 30B profiles use their documented `0.7/0.8/20` sampling
set, including repetition penalty `1.05`. Coder Next uses its distinct
`1.0/0.95/40` set. These values belong to model profiles, not to the shared
transport.

Sources:

- <https://github.com/QwenLM/Qwen3-Coder>
- <https://huggingface.co/Qwen/Qwen3-Coder-480B-A35B-Instruct>
- <https://huggingface.co/Qwen/Qwen3-Coder-Next>
- <https://huggingface.co/Qwen/Qwen3-Coder-30B-A3B-Instruct>
- <https://huggingface.co/Qwen/Qwen3.6-27B>
- <https://huggingface.co/Qwen/Qwen3.6-35B-A3B>

## 7. Conformance suite

Every adapter must pass the same semantic cases using recorded or local mock
streams:

1. Plain streamed answer.
2. One tool call followed by a final answer.
3. Several tool calls in one model turn.
4. Tool execution error returned to the same call ID.
5. Text and tool calls interleaved in one response.
6. Malformed and truncated tool arguments.
7. Duplicate call ID.
8. Stream ends without a terminal event.
9. Cancellation during text and tool-argument streaming.
10. Context continuation after compaction or provider-handle loss.
11. Usage and cache accounting.
12. A long Rust task with repeated compiler feedback.

Passing the HTTP schema is necessary and insufficient. A real model run must
also complete the common behavioural evals before a model profile is marked
supported.

### 7.1 The suite, built 2026-08-27

`gyr-model/tests/conformance.rs` drives all three adapters over a real socket
with provider-shaped SSE and asserts the same **normalised** events from each.
Three wires, one semantics, which is the whole claim the `ModelSession` boundary
makes and which nothing checked until now. Each adapter had tested whatever its
author thought of, and **`OpenAiSession` had no wire test at all** — the
transport with the least coverage being the one with no credential to verify it.

Seven of the twelve scenarios are covered here, and the honest accounting of the
rest matters more than the count:

| # | Scenario | Where |
|---|---|---|
| 1 | Plain streamed answer | conformance suite |
| 2 | Tool call then answer, result returned | conformance suite |
| 3 | Several tool calls in one turn | conformance suite |
| 4 | Tool error returned to the same call ID | `gyr-core` agent tests |
| 5 | Text and tool calls interleaved | conformance suite |
| 6 | Malformed or truncated arguments | conformance suite |
| 7 | Duplicate call ID | `gyr-core` agent tests |
| 8 | Stream ends without a terminal event | conformance suite |
| 9 | Cancellation mid-stream | `gyr-core` agent tests |
| 10 | Continuation after handle loss | RFC-0014 round trips |
| 11 | Usage and cache accounting | conformance suite |
| 12 | A long Rust task with compiler feedback | the eval corpus |

Scenarios 4, 7 and 9 are properties of the loop rather than of a transport and
are tested where they live. Scenario 12 is an eval and cannot be a unit test.

Scenario 2 is the two-turn one, and the only one that inspects what the adapter
**sends** rather than what it parses. The three wires carry a tool result three
different ways: a `tool_result` content block, a `function_call_output` item, and
a message with `role: "tool"`. What must be true of all three is that the next
request carries the output tied to the ID the provider issued, and that is what
is asserted, one property across three shapes.

**A convention worth writing down, discovered by getting a fixture wrong.**
Anthropic's `input_tokens` **excludes** cached tokens, so its adapter sums
`input + cache_creation + cache_read`. OpenAI's and Qwen's **include** them, so
theirs pass the value through. Fifteen-plus-five on one wire is
twenty-including-five on the others, and all three normalise to twenty. The
first draft of the fixture used twenty on all three, and the suite reported
Anthropic as twenty-five. Both adapters were right; the fixture was not.

That is the kind of thing a cross-provider suite exists to surface, and it would
have surfaced as a real accounting error the first time anyone compared token
counts between providers.

## 8. Raw capture and privacy

Conformance fixtures need raw provider events, but real sessions may contain
source code and secrets. Raw capture is opt-in, stored locally, permissioned to
the current user, and scrubbed before any fixture is committed. The normal
session log contains normalised events and redacted request metadata by default.
