"""Prints the comparison tables from the dino eval records in one directory.

usage: python3.12 -I scripts/compare/table.py evals/comparison/<date>

The first table is one row per model, with cost worked out from each run's own
tokens at list price. The second is the mean score per case, so a model that
wins overall but loses a kind of task shows it. Last comes the most
cost-effective model by the rule in evals/MODEL-CHOICE.md.
"""
import json
import os
import sys

# USD per million tokens: uncached input, cached input, output. List prices as
# checked on 2026-10-30; MODEL-CHOICE.md gives the sources.
PRICES = {
    "qwen3.7-max": (2.50, 0.50, 7.50),
    "kimi-k3.1-pro": (2.00, 0.20, 10.00),
    "grok-4.7": (2.00, 0.50, 6.00),
    "terra-6.1": (1.00, 0.05, 5.00),
    "sonnet-5.5": (2.00, 0.20, 10.00),
    "opus-5.5": (4.00, 0.20, 20.00),
}
# xAI reports reasoning beside completion tokens; the others count it inside.
REASONING_BILLED_SEPARATELY = {"grok-4.7"}
CEILING = {"opus-5.5"}


def cost(name, tokens):
    price = PRICES.get(name)
    if price is None:
        return None
    cached = tokens["cached_input_tokens"]
    output = tokens["output_tokens"]
    if name in REASONING_BILLED_SEPARATELY:
        output += tokens["reasoning_tokens"]
    return ((tokens["input_tokens"] - cached) * price[0] + cached * price[1] + output * price[2]) / 1e6


d = sys.argv[1]
runs = {}
for f in sorted(os.listdir(d)):
    if f.endswith(".json"):
        try:
            runs[f[:-5]] = json.load(open(os.path.join(d, f)))
        except ValueError:
            print(f"{f}: not a finished record, skipped", file=sys.stderr)

rows = {}
print("| Model | Score (90% interval) | Passed | Mean turns | Tool errors | Min per run | Cost per run | Cost per pass | Total cost |")
print("|---|---|---|---|---|---|---|---|---|")
for name, r in runs.items():
    s, o = r["summary"], r["outcomes"]
    n = len(o) or 1
    turns = sum(x["metrics"]["model_turns"] for x in o) / n
    errors = sum(x["metrics"]["tool_errors"] for x in o)
    minutes = sum(x["duration_ms"] for x in o) / n / 60000
    passed = sum(x["passed"] for x in o)
    costs = [cost(name, x["metrics"]["tokens"]) for x in o]
    total = None if None in costs else sum(costs)
    per_pass = total / passed if total is not None and passed else None
    rows[name] = (s, total, per_pass)
    money = lambda v: "-" if v is None else f"${v:.3f}" if v < 1 else f"${v:.2f}"
    print(f"| {name} | {s['score']} ({s['low']}-{s['high']}) | {passed}/{len(o)} | {turns:.1f} | {errors} "
          f"| {minutes:.1f} | {money(None if total is None else total / n)} | {money(per_pass)} | {money(total)} |")

cases = sorted({c for r in runs.values() for c in r["summary"]["cases"]})
print()
print("| Case | " + " | ".join(runs) + " |")
print("|---|" + "---|" * len(runs))
for c in cases:
    cells = []
    for r in runs.values():
        x = r["summary"]["cases"].get(c)
        cells.append(f"{x['mean']:.0f}" if x else "-")
    print(f"| {c} | " + " | ".join(cells) + " |")

# The cheapest per pass among candidates whose interval reaches the best candidate's.
candidates = {k: v for k, v in rows.items() if k not in CEILING and v[2] is not None}
if candidates:
    best = max(candidates.values(), key=lambda v: v[0]["score"])[0]
    level = [k for k, v in candidates.items() if v[0]["high"] >= best["low"]]
    pick = min(level, key=lambda k: candidates[k][2])
    print()
    print(f"Statistically level with the best candidate ({best['score']}): {', '.join(sorted(level))}.")
    print(f"Most cost-effective of those: {pick}, ${candidates[pick][2]:.3f} per passed run.")
