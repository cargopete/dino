#!/bin/zsh
# Runs the hard corpus through dancho (a self-hosted model) and Claude Code, for
# comparison. RFC-0017 section 8.5 reports what it found.
#
# usage: scripts/bench.sh <out_dir> <trials> <case> [case ...]
#
# Needs QWEN_API_BASE and QWEN_API_KEY for the dancho runs, `claude` on PATH for the
# Claude runs (skip them with SKIP_CLAUDE=1), and python3.12 for the baseline
# script. DANCHO overrides the dancho binary (default: target/release/dancho), and
# DANCHO_EVAL_ARGS passes extra flags to every dancho run, such as an ablation.
#
# Each (model, trial, case) writes its own result, so a crash loses one case
# at most, and a rerun skips what already finished. Trials of the self-hosted
# model run side by side, so its wall times are under load.
set -u
out=${1:?out_dir}; trials=${2:?trials}; shift 2
repo=${0:A:h:h}
dancho=${DANCHO:-$repo/target/release/dancho}
model=${DANCHO_MODEL:-qwen3.8-27b}
python=${PYTHON:-python3.12}
: ${QWEN_API_BASE:?set QWEN_API_BASE} ${QWEN_API_KEY:?set QWEN_API_KEY}
mkdir -p $out/results $out/work

dancho_trial() { local t=$1; shift; for c in "$@"; do
  local f=$out/results/$model-$t-$c.json; [ -s $f ] && continue
  # A failing case exits non-zero; its result is still a result.
  $dancho eval --model $model --corpus $repo/evals --scratch $out/work/$model-$t \
    --case $c --json ${=DANCHO_EVAL_ARGS:-} > $f.tmp 2> $out/results/$model-$t-$c.err
  mv $f.tmp $f
done; }

claude_runs() { local name=$1 claude_model=$2; shift 2; for t in $(seq 1 $trials); do for c in "$@"; do
  local f=$out/results/$name-$t-$c.json; [ -s $f ] && continue
  $python -I $repo/scripts/claude-code-eval.py $repo/evals $out/work/$name-$t $claude_model $c \
    > $f.tmp 2> $out/results/$name-$t-$c.err && mv $f.tmp $f
done; done; }

for t in $(seq 1 $trials); do dancho_trial $t "$@" & done
if [ -z "${SKIP_CLAUDE:-}" ]; then
  claude_runs sonnet claude-sonnet-5-5 "$@" &
  claude_runs opus claude-opus-5-5 "$@" &
fi
wait
echo done
