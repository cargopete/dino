# Ablations

Each row is the full corpus, scored (RFC-0019, version 2), three trials per
case, qwen3.7-max on the Token Plan, run in parallel by `scripts/ablate.sh`.
A tool is kept only if withholding it does not improve the score.

## 2026-10-26, main at a26991f

| Run | Score (90% interval) | Passed | Mean turns | Tool errors |
|---|---|---|---|---|
| baseline, all tools | 93 (92-95) | 54/63 | 11.4 | 7 |
| without `search` | 93 (91-94) | 56/63 | 12.1 | 13 |
| without `shell` | 92 (90-94) | 54/63 | 12.7 | 9 |
| without `background`, `packages` | 92 (90-94) | 53/63 | 11.9 | 11 |
| without `todo` | 91 (88-94) | 51/63 | 11.4 | 11 |

No difference is significant at three trials; every interval overlaps the
baseline's. No withheld tool improved the score, three of four raised the
mean turn count, and all raised tool errors, so all are kept. `todo` comes
closest to showing its worth: three fewer passes, and make-it-thread-safe fell
33 points without it.

The baseline scored 95 (95-96) on the same corpus that morning, before
`background`, `/init` and regex search landed. Withholding `background` and
`packages` did not recover it, so the drop is more likely noise or five runs
sharing one plan's capacity than the added tools.

## 2026-10-26, the requirement check (#89)

Not an ablation but the same method: main against main plus the instruction to
check each requirement before finishing. 95 (95-96) against 93 (92-94), 57 against
52 of 63 passed, honesty 0.85 against 0.77. Not adopted.
