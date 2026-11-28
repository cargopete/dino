# REVIEW.md

This file is the review guide for our coding workers and human reviewers: what to flag, at what severity, and what to skip. Claude's managed Code Review and Approvals are not installed (the owner's plan is Max, not Team or Enterprise), so nothing here is enforced by a GitHub check. The local `/code-review` command does not read this file; it reads `CLAUDE.md`. Put rules you want in both places in `CLAUDE.md`.

## What matters most here
dino is a terminal coding agent written in Rust, designed for Rust work, with an open-weight-first stance. Design lives in `docs/rfcs/`.

Flag these as Important:
- **Sandbox, exec and the network allowlist** (`dino-sandbox`, `dino-exec`; RFC-0009, RFC-0010, RFC-0021). Any change that widens what the agent can read, write, run or reach is a finding unless the RFC says so. The CI confine checks must still pass.
- **Provider and model code** (`dino-model`, the provider protocol in RFC-0003). Keys come from env only, are never logged, and tests use fake values (MOONSHOT_API_KEY and friends).
- **Numbers and claims.** Results in the README and `evals/` must be traceable to a run in `evals/` or `scripts/ledger`. Flag a figure with no source, rounded or invented figures, and a claim that is stronger than the data (one run is one run).
- **Eval integrity.** Changes to `dino-eval` or the corpus must not let the model see hidden tests or change the scoring (RFC-0019).
- **Behaviour change vs RFC.** A behaviour change that contradicts an accepted RFC needs the RFC updated in the same PR.
- **Replay and session resumption** (RFC-0014, RFC-0016) must stay deterministic.
- **`extension/`** is a public browser extension; flag new permissions in `manifest.json`.
- Clippy runs with warnings denied (`clippy.toml`). `yatr fmt-check`, `lint`, `build` and `test` are CI's job.

## Calibration
- Style and naming: Nit, at most 3 per review.

## Skip
`Cargo.lock`, files under `evals/` that are recorded run output.
