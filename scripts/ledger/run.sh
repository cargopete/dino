#!/bin/bash
# Runs the ledger brief against one model in a fresh crate.
# Usage: scripts/ledger/run.sh <model-key> <directory> [max-turns]
set -euo pipefail
model=$1; dir=$2; turns=${3:-150}
brief="$(cd "$(dirname "$0")" && pwd)/brief.md"
rm -rf "$dir"; mkdir -p "$dir"; cd "$dir"
cargo init -q --name ledger
git init -q && git add -A && git -c user.name=ledger -c user.email=ledger@localhost commit -qm "Empty crate"
start=$(date +%s)
set +e
timeout 5400 gyr run --plain --model "$model" --dangerously-allow-all --max-turns "$turns" "$(cat "$brief")" > run.txt 2>&1
code=$?
set -e
echo "exit=$code seconds=$(( $(date +%s) - start ))" > run.meta
