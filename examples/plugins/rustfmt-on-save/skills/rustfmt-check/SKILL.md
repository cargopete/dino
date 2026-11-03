---
name: rustfmt-check
description: Check formatting across a Rust workspace before a commit. Use before committing Rust changes.
---

# rustfmt-check

Edits are formatted file by file as they happen. Files the model did not edit
are not, so before a commit run `cargo fmt --all --check`. If it reports
anything in files this change touched, run `cargo fmt` on those and include
them; formatting elsewhere is a separate change, so say so and leave it.
