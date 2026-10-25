#!/bin/bash
# Scores a finished ledger run against the brief's checkable requirements.
# Usage: scripts/ledger/score.sh <directory>
set -uo pipefail
cd "$1"
yes_no() { if eval "$1" >/dev/null 2>&1; then echo yes; else echo no; fi; }
locked() { awk -v n="$1" '$0=="name = \""n"\"" {getline; gsub(/version = |"/,""); print; exit}' Cargo.lock 2>/dev/null; }
tests=$(cargo test 2>&1 | awk '/^test result:/ {passed+=$4; failed+=$6} END {printf "%d passed, %d failed", passed, failed}')
log=$(ls -t .gyr/sessions/*.jsonl 2>/dev/null | head -1)
turns=$(grep -o '"model_turns":[0-9]*' "$log" 2>/dev/null | tail -1 | cut -d: -f2)
echo "model        $(grep -o '"model_key":"[^"]*"' "$log" 2>/dev/null | head -1 | cut -d'"' -f4)"
echo "run          $(cat run.meta 2>/dev/null) turns=${turns:-?}"
echo "builds       $(yes_no 'cargo build')"
echo "clippy clean $(yes_no 'cargo clippy --all-targets -- -D warnings')"
echo "formatted    $(yes_no 'cargo fmt --check')"
echo "tests        $tests"
echo "proto+build  $(yes_no 'test -f proto/ledger.proto && grep -q tonic build.rs')"
echo "shutdown     $(yes_no 'grep -rqE "ctrl_c|signal::unix|SignalKind" src')"
echo "real ports   $(yes_no 'grep -rqE "TcpListener::bind|127\.0\.0\.1:0|0\.0\.0\.0:0" tests')"
echo "grpc client  $(yes_no 'grep -rqE "Client::connect|ServiceClient" tests')"
echo "versions     axum $(locked axum), tonic $(locked tonic), thiserror $(locked thiserror)"
