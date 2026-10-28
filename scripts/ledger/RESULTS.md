# Ledger comparison results

`run.sh` sends `brief.md` to one model in an empty crate; `score.sh` checks the
result. One run per model, so a close result settles nothing.

## 2026-10-25, Alibaba Token Plan, dino after the loopback and unparsed-arguments fixes

| Model | Turns | Minutes | Tests | Clippy, fmt | HTTP and gRPC clients | Versions | Notes |
|---|---|---|---|---|---|---|---|
| qwen3.7-max | 50 | 10 | 25 pass | clean | yes | axum 0.8, tonic 0.13 | Only finisher on current versions throughout. |
| qwen3.7-plus | 26 | 7 | 30 pass | clean | yes | axum 0.8 (+0.7 via tonic 0.12) | Fastest; tonic a release behind. |
| deepseek-v4-pro | 56 | 17 | 21 pass | clean | yes | axum 0.7, tonic 0.12 | Older axum throughout. |
| qwen3.8-max | 62 | 31 | 25 pass | clean | yes | axum 0.8 (+0.7 via tonic 0.12) | Most thorough summary; slowest finisher. |
| glm-5.3 | 150 (cap) | 48 | 26 pass | clean | yes | axum 0.8, tonic 0.13 | Ran out of turns testing shutdown by hand; left five servers running (#92). |

Every model met the brief's checkable requirements once loopback was allowed.
The first comparison the same day, before that fix, is not comparable: the
sandbox refused `bind`, so no real-port test could run, and a malformed tool
call ended glm-5.3's run at turn one.
