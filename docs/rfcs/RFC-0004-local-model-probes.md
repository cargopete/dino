# RFC-0004: Local subscription model probes

| | |
|---|---|
| Status | observed baseline |
| Date | 2026-08-23 |
| Depends on | RFC-0003 |

## 1. Purpose

The development machine has active Claude and ChatGPT subscriptions. These are
useful for cheap behavioural smoke tests before direct API credentials are
configured. They do not provide a licence to extract either application's
tokens or treat a consumer login as a generic provider API key.

This distinction produces two test layers:

1. Official vendor CLIs verify that a named model is available to the local
   subscription and follows a bounded instruction.
2. DanchoAI's provider transports are verified by recorded streams, local mock
   servers and, when separately supplied, vendor API credentials.

Passing the first layer does not imply that the second has reached a live
vendor endpoint.

## 2. Authentication boundary

**Provider-documented and locally observed on 2026-08-23:** Codex CLI is logged
in using the ChatGPT subscription flow and Claude Code is logged in using a
first-party Claude subscription. Neither `OPENAI_API_KEY` nor
`ANTHROPIC_API_KEY` is present in the process environment.

OpenAI documents ChatGPT sign-in and API-key sign-in as supported Codex CLI
authentication methods. It also states that general OpenAI API usage continues
to use Platform API keys. DanchoAI must not read, copy or replay the private
tokens held by Codex CLI or Claude Code. A future subscription-backed adapter
must use an explicitly documented integration boundary, such as invoking an
official CLI under a constrained protocol, rather than borrowing credentials
from another application's cupboard.

Source:

- <https://developers.openai.com/codex/auth>

## 3. Probe method

Each CLI received a single request to return an exact sentinel. Model tools
were disabled, repository access was read-only or unavailable, and session
persistence was disabled. The probes were not coding evaluations and incurred
no file changes.

The Claude probe selected the `sonnet` alias with safe mode, no tools, plan
permissions and JSON output. The Codex probe selected `gpt-5.6-terra` with a
read-only sandbox, ephemeral execution, ignored user configuration and JSONL
output.

The first Codex attempt failed before contacting the model because the managed
sandbox prohibited initialising Codex's in-process application server. The
identical read-only command succeeded outside that sandbox. This failure is
recorded because a test harness which reports it as a model failure would be
lying with admirable confidence.

## 4. Observations

| Model | Result | Wall time | Reported usage |
|---|---|---:|---|
| Claude Sonnet 5 | exact `DANCHOAI_SONNET_OK` | 3.58 s | 2 ordinary input, 3,505 cache creation, 3,289 cache read, 17 output |
| GPT-5.6 Terra | exact `DANCHOAI_TERRA_OK` | 4.34 s | 16,171 input, 9,984 cached input, 11 output, 0 reasoning output |

Claude Code reported canonical model `claude-sonnet-5`, a 1,000,000-token
context window and a 64,000-token maximum output for this subscription run. The
wrapper also used a small Haiku request, so its total reported cost was not
solely the Sonnet generation. Codex reported the requested Terra model only
through the command selection; the successful event stream contained the
sentinel and token usage but did not echo a model identifier.

The large input counts demonstrate that official coding-agent CLIs add their
own system context even for a trivial prompt. These figures therefore describe
the vendor CLI envelopes, not raw model API overhead and not DanchoAI's future
prompt budget.

## 5. Decision

Keep these probes as a manually invoked development check, not a default unit
test. They consume subscription capacity, depend on external services and can
change with vendor CLI releases. Automated conformance remains deterministic
and local; live transport tests will be opt-in and require explicit API keys.
