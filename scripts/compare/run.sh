#!/bin/sh
# Scores the corpus on one model, through dino or through a vendor's CLI, and
# records what it ran with, so a comparison can be repeated and audited.
#
#   scripts/compare/run.sh <name> <work-dir> --model <model> [--harness codex|kimi|grok|claude-code] [eval flags]
#
# Writes evals/comparison/<date>/<name>.json (the dino eval record) and
# <name>.manifest. Session logs stay under <work-dir>, outside the repo.
set -eu
name=$1 work=$2
shift 2
repo=$(cd "$(dirname "$0")/../.." && pwd)
out=$repo/evals/comparison/$(date +%Y-%m-%d)
mkdir -p "$out" "$work"
trials=${TRIALS:-3}

{
    echo "name: $name"
    echo "started: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "commit: $(git -C "$repo" rev-parse HEAD)$(git -C "$repo" diff --quiet HEAD -- crates || echo ' (dirty)')"
    echo "dino: $(command -v dino) sha256 $(shasum -a 256 "$(command -v dino)" | cut -c1-16)"
    echo "trials: $trials"
    echo "args: $*"
    for cli in codex kimi grok claude; do
        command -v $cli >/dev/null 2>&1 && echo "$cli: $($cli --version 2>&1 | head -1)"
    done
    echo "host: $(uname -sm), $(sysctl -n hw.ncpu 2>/dev/null) cpus"
} > "$out/$name.manifest"
# Uncommitted code is recorded as a patch, so the run can still be rebuilt exactly.
git -C "$repo" diff --quiet HEAD -- crates || git -C "$repo" diff HEAD -- crates > "$out/$name.patch"

cd "$repo"
# The record is written outside the repo and moved in at the end, so git
# operations on the checkout during a long run cannot lose it.
status=0
dino eval --plain --json --score --trials "$trials" --scratch "$work/$name" "$@" \
    > "$work/$name.json" 2> "$work/$name.err" || status=$?
mv "$work/$name.json" "$out/$name.json"
echo "finished: $(date -u +%Y-%m-%dT%H:%M:%SZ), exit $status" >> "$out/$name.manifest"
