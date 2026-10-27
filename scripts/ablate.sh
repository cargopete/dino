#!/bin/sh
# Scores the corpus with each listed tool set withheld, beside a baseline, all in
# parallel at three trials, so a change is kept only if the numbers support it.
#
#   scripts/ablate.sh <out-dir> <model> "shell" "todo" "background packages" ...
#
# Each argument after the model is one ablation: the tools it withholds.
set -eu
out=$1 model=$2
shift 2
mkdir -p "$out"
run() {
    name=$1
    shift
    dancho eval --plain --json --model "$model" --score --trials 3 --scratch "$out/$name-work" "$@" \
        > "$out/$name.json" 2> "$out/$name.err"
}
run baseline &
for tools in "$@"; do
    name=$(echo "$tools" | tr ' ' '+')
    flags=""
    for tool in $tools; do flags="$flags --without $tool"; done
    # shellcheck disable=SC2086
    run "without-$name" $flags &
done
wait
for result in "$out"/*.json; do
    python3 -c "
import json, sys
d = json.load(open(sys.argv[1])); s = d['summary']; o = d['outcomes']
print(f\"{sys.argv[1].rsplit('/', 1)[1][:-5]:<32} {s['score']:>3}  ({s['low']}-{s['high']})  passed {sum(x['passed'] for x in o)}/{len(o)}\")
" "$result"
done
