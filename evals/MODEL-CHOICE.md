# Choosing Dino's default model

The question: which model should Dino use by default for Chief's own work? It
is `qwen3.7-max` on the Alibaba Token Plan, chosen on 2026-10-25 from one
ledger run. This file holds the evidence, how each piece was gathered, what it
can and cannot say, and the decision. Raw records sit in
`evals/comparison/<date>/`, per model: the `dino eval` record (`.json`), a
`.manifest` (commit, binary hash, flags, start and end) and, when the tree was
not committed, a `.patch` of the code that ran.

## How to decide

In order:

1. **Corpus score in Dino** (RFC-0019, version 2). Treat a gap as real only if
   the 90% intervals do not overlap.
2. **The six hard-bench cases** (`interval-set`, `diagnose-the-panic`,
   `make-it-thread-safe`, `large-workspace-find-the-cause`,
   `pratt-expression-parser`, `zero-copy-tokenizer`), read from the per-case
   table, where the corpus is too easy to separate strong models.
3. **Cost per passed run**, from each run's own tokens at list price.
4. **Speed**, mean minutes per run.

Dino's prompts and tools were tuned on Qwen sessions, so a narrow Qwen win is
a draw.

**Most cost-effective**, as `scripts/compare/table.py` names it: among the
candidates whose score interval reaches the best candidate's, the one with the
lowest cost per passed run. Opus is the ceiling and is not a candidate.

### Prices

USD per million tokens, checked 2026-10-30. Cost per run is uncached input,
cached input and output, each at its rate, from the tokens the run reported.

| Model | Input | Cached input | Output | Source |
|---|---|---|---|---|
| qwen3.7-max | 2.50 | 0.50 | 7.50 | [Model Studio](http://docs.modelstudio.console.alibabacloud.com/en/model-studio/qwen3-7-max), international list price |
| kimi-k3.1-pro | 2.00 | 0.20 | 10.00 | [Yotta Labs](https://www.yottalabs.ai/post/kimi-k3-1-pro-pricing-api-cost) |
| grok-4.7 | 2.00 | 0.50 | 6.00 | [eesel](https://www.eesel.ai/blog/grok-4-7-pricing), prompts under 200K |
| terra-6.1 | 1.00 | 0.05 | 5.00 | [eesel](https://www.eesel.ai/blog/gpt-6-1-terra-pricing), prompts under 272K |
| sonnet-5.5 | 2.00 | 0.20 | 10.00 | Anthropic |
| opus-5.5 | 4.00 | 0.20 | 20.00 | Anthropic |

Where the arithmetic is approximate:

- Grok bills reasoning beside its completion tokens (total = prompt +
  completion + reasoning), so its reasoning is added to output. Kimi, Qwen,
  OpenAI and Anthropic count reasoning inside output, checked against each
  API's totals on 2026-10-30.
- Anthropic charges 1.25 times input for cache writes; Dino counts those as
  plain input, so Claude's cost is slightly understated.
- Long-prompt surcharges (Grok over 200K, Terra over 272K) are ignored.
- Qwen on the Token Plan costs a monthly fee until the quota runs out, not per
  token. The list price is used so it compares like for like; a promotional
  Model Studio rate ($1.65, $0.33, $4.95) would cut its figure by a third.

## Evidence before 2026-10-30

| Source | What ran | Result |
|---|---|---|
| `scripts/ledger/RESULTS.md`, 10-25 | One service brief, one run per Token Plan model, in Dino | All five met the brief. qwen3.7-max was the only finisher on current crate versions, in 50 turns and 10 minutes. |
| RFC-0017 §8.6, 10-19 and 10-20 | Six hard cases with hidden tests, three trials each | Opus 5.5 18/18 and Sonnet 5.5 17/18 in Claude Code; Qwen3.8 27B (self-hosted, not the Token Plan model) 11/18, then 15/18 in Dino after harness fixes, at about 30 times Opus's wall time. |
| `evals/ABLATIONS.md`, 10-26 to 10-28 | The full corpus, three trials, qwen3.7-max in Dino | 91 to 95 across baselines. At `0da5149`: 91 (89-93) with 62/75 passed. |

Only totals were kept for those qwen3.7-max runs. Every run from here keeps
its record.

## 2026-10-30: the deciding run

Six models, all in Dino, one binary (sha256 `9ccd292afc7344d6...`, `main` at
`2e054e2` plus the patch beside each record), started together on one machine,
five trials of the full corpus of 26 cases: 130 runs per model.

| Model | Profile | Endpoint | Reasoning setting |
|---|---|---|---|
| Qwen3.7-Max | `qwen3.7-max` | Alibaba Token Plan | 4,096-token thinking budget |
| Kimi K3.1 Pro | `kimi-k3.1-pro` | `api.moonshot.ai/v1` | 4,096-token thinking budget sent; Moonshot may ignore it |
| Grok 4.7 | `grok-4.7` | `api.x.ai/v1` | as Kimi |
| GPT-6.1 Terra | `terra-6.1` | OpenAI Responses API | effort `high` |
| Claude Sonnet 5.5 | `claude-sonnet-5.5` | Anthropic | effort `high` |
| Claude Opus 5.5 | `claude-opus-5.5` | Anthropic | effort `high`; the ceiling, not a candidate |

Kimi and Grok go through Dino's OpenAI-compatible (Qwen) adapter. Each
profile's window is the provider's: 262K for Qwen, 1M for Kimi, Terra and
Claude, 500K for Grok. Run with `~/dino-bench-results/decisive.sh`; session
logs are in `~/dino-bench-results/decisive/`.

Estimated cost from Sonnet's measured tokens (92K input, 6K output per run),
before caching: Opus about $65, Sonnet, Kimi and Grok about $30 each, Terra
about $16. No spend cap; Kimi had $55 at the start.

### Results

Five of six models finished; Grok was stopped (see the log). From
`scripts/compare/table.py evals/comparison/2026-10-30`, cost at list price:

| Model | Score (90%) | Passed | Min per run | Turns | Tool errors | Cost per pass | Total |
|---|---|---|---|---|---|---|---|
| Opus 5.5 (ceiling) | 99 (97-100) | 128/130 | 1.4 | 6.4 | 9 | $0.292 | $37.32 |
| Sonnet 5.5 | 96 (94-97) | 128/130 | 0.8 | 5.5 | 36 | $0.120 | $15.39 |
| Kimi K3.1 Pro | 95 (94-96) | 114/130 | 7.3 | 14.6 | 87 | $0.240 | $27.40 |
| Qwen3.7-Max | 90 (87-92) | 109/130 | 2.9 | 11.4 | 50 | $0.337 | $36.68 |
| GPT-6.1 Terra | 74 (68-80) | 96/130 | 1.3 | 9.1 | 13 | $0.056 | $5.40 |

- **Kimi beats Qwen**: the intervals do not overlap, on prompts tuned for Qwen.
  Qwen falls to 71-80 on the long builds (`ledger`, `semver-ranges`,
  `text-buffer-undo`, `toml-subset-parser`, `zero-copy-tokenizer`); Kimi
  stays at 88-99. On the six hard cases they split: Kimi wins
  `zero-copy-tokenizer` (94 to 78), Qwen `make-it-thread-safe` (99 to 89).
- **Sonnet and Kimi are level on score**; Sonnet passed 14 more runs, nine
  times faster, at half the cost per pass. By the rule above Sonnet is the
  most cost-effective candidate.
- Kimi's lost passes are mostly not its code: 21 runs edited test files the
  case protects (#166), and 37 of its tool errors were an empty `list` path
  (#164). Sonnet's 33 on `toml-2-tables` is a scorer fault (#165), so its
  score is slightly understated.
- Terra is cheap and well behaved but well short.

### Rescored, 2026-10-31

The scorer fault in #165 was not the scorer's rule: hidden test files copied
over a workspace kept their old modification times, so Cargo reran the model's
own stale test binary instead of the hidden tests. Fixed in #178, which also
added `dino eval --rescore`; the records were rescored without calling any
model, as `<model>.rescored.json` beside the originals.

| Model | Score (90%) | Passed | Cost per pass |
|---|---|---|---|
| Opus 5.5 (ceiling) | 100 (99-100) | 130/130 | $0.287 |
| Sonnet 5.5 | 99 (99-99) | 128/130 | $0.120 |
| Kimi K3.1 Pro | 95 (94-96) | 113/130 | $0.243 |
| Qwen3.7-Max | 90 (87-92) | 109/130 | $0.337 |
| GPT-6.1 Terra | 74 (68-80) | 96/130 | $0.056 |

Sonnet and Kimi are no longer level: Sonnet's interval is clear of Kimi's, and
Sonnet is the only candidate level with the best. Kimi's one fewer pass is the
ledger acceptance test's port race flipping a trial on the rerun, not a change
in its work. Kimi still clearly beats Qwen.

### Decision

Kimi K3.1 Pro replaces qwen3.7-max as the default (#167), with Qwen on the
flat-rate plan as the first fallback. Sonnet 5.5 is the better model on this
evidence and the choice whenever per-token Anthropic cost is acceptable.
Rerun Kimi once #164, #166 and #163 land.

### What changed in Dino for this run

- **Anthropic prompt caching was never requested.** The adapter read cache
  usage but sent no `cache_control`, so every Claude turn, the Opus fallback
  included, paid full price for its whole history. Now on; a probe read 12.9K
  of 20.5K input tokens from cache.
- **Terra could not run at all.** The OpenAI adapter declared tools `strict`,
  which requires every property to be required; `read` has an optional one.
  Now `strict: false`, as no other provider gets strict schemas either.
- Profiles added: `terra-6.1`, `kimi-k3.1-pro`, `grok-4.7`.
- `dino eval --harness` gained `kimi` and `grok`, for runs through those
  vendors' own CLIs, and a model name with a `/` no longer loses its session
  log.

### Log

- Sonnet 5.5 in Dino at `0da5149`, and Terra 5.6, Kimi and Grok through their
  own CLIs on Chief's subscriptions, were started first, then stopped
  unfinished once API keys made the all-in-Dino run possible. None of them is
  used. The automatic permission check refused to launch Kimi Code unattended,
  because its prompt mode runs every call unasked with no sandbox.
- Keys checked 2026-10-30: Anthropic, OpenAI, xAI and the Token Plan
  generated; Moonshot was suspended at a zero balance until Chief topped it up.
  No provider reports a balance to an ordinary API key except Moonshot.
- One probe case per model passed before the run (Terra after the strict fix).
- The first launch failed on every model: `run.sh` resolved the corpus
  against the caller's directory. It now runs from the repo root.
- 2026-10-31: Grok 4.7 stopped at Chief's request at about 119 of 130 runs. `dino eval`
  writes its record only at the end, so `grok-4.7.json` is empty and Grok is not in the
  results. The finished records show a scorer fault (#165) and two Kimi-specific faults
  (#164, #166) that cost Kimi passes; see those issues before reading the TOML rows.
