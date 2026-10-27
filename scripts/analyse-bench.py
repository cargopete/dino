"""Mine a bench's session logs for what dancho made hard, and re-score runs that did not complete.

usage: python3.12 -I scripts/analyse-bench.py <out_dir> [--rescore] [--context N]

Reads what scripts/bench.sh wrote under <out_dir> and writes report.md and
runs.json beside it. --rescore copies the workspace each failed run left,
overlays the case's hidden tests and counts how many pass, because a run cut off
while re-checking finished work is not the same as one that failed, and a run
that passes 40 of 44 is not the same as one that passes none. --context is the
served context length the peak prompt is measured against (default 65536).
"""
import collections
import glob
import json
import os
import re
import shutil
import subprocess
import sys

B = os.path.abspath(sys.argv[1])
CORPUS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "evals")
CTX = int(sys.argv[sys.argv.index("--context") + 1]) if "--context" in sys.argv else 65536

ERROR_KINDS = [
    ("stale sha", r"sha|changed since|stale"),
    ("old_text not found", r"not found|does not occur|no match|did not match"),
    ("old_text ambiguous", r"more than once|ambiguous|occurs \d+ times"),
    ("file missing / create", r"no such file|does not exist|not a file|cannot create"),
    ("not read first", r"read .* first|has not been read|before patching"),
    ("sandbox denied", r"denied|operation not permitted|sandbox"),
    ("timeout", r"timed out"),
    ("bad arguments", r"missing field|invalid type|unknown field|expected"),
]


def kind(text):
    for name, pat in ERROR_KINDS:
        if re.search(pat, text, re.I):
            return name
    return "other"


def mine(log):
    turns = collections.defaultdict(lambda: {"tools": [], "text": 0})
    calls, out = {}, {"errors": [], "exec": [], "repeats": 0, "finish": None, "retried": 0, "told": 0}
    first = last = None
    real_shas = set()
    seen = collections.Counter()
    for line in open(log):
        e = json.loads(line)
        ms = e.get("unix_millis")
        first = first or ms
        last = ms or last
        if e.get("record") == "finished":
            out["finish"] = e.get("outcome")
            continue
        ev = e.get("event") or {}
        t = ev.get("model_turn")
        if ev.get("type") == "model":
            inner = ev["event"]
            it = inner.get("type")
            if it == "started":
                turns[t]["start"] = ms
            elif it == "finished":
                turns[t]["end"] = ms
                turns[t]["reason"] = inner.get("reason")
            elif it == "usage":
                turns[t]["usage"] = inner["usage"]
            elif it == "text_delta":
                turns[t]["text"] += len(inner.get("text", ""))
            elif it == "tool_call_completed":
                c = inner["call"]
                calls[c["id"]] = (t, c["name"], c["arguments"])
                key = json.dumps([c["name"], c["arguments"]], sort_keys=True)
                seen[key] += 1
                if seen[key] > 1 and c["name"] != "cargo":
                    out["repeats"] += 1
                if c["name"] == "exec":
                    a = c["arguments"]
                    cmd = a.get("command") or []
                    out["exec"].append((" ".join(cmd) if isinstance(cmd, list) else str(cmd))[:120])
        elif ev.get("type") == "turn_retried":
            out["retried"] += 1
        elif ev.get("type") == "turns_running_out":
            out["told"] += 1
        elif ev.get("type") == "tool_finished":
            r = ev["result"]
            o = r.get("output") or {}
            if isinstance(o, dict):
                real_shas.update(re.findall(r"[0-9a-f]{64}", o.get("content", "")))
            tt, name, args = calls.get(r["call_id"], (t, "?", {}))
            turns[tt]["tools"].append(name)
            if isinstance(o, dict) and o.get("is_error"):
                body = o.get("content", "")
                k = kind(body)
                if k == "stale sha":
                    k = "stale sha (truly stale)" if args.get("expected_sha256") in real_shas else "stale sha (hash mis-copied)"
                out["errors"].append({"turn": tt, "tool": name, "kind": k, "msg": body[:300],
                                      "args": json.dumps(args)[:200]})
    us = [v.get("usage", {}) for v in turns.values()]
    gen = [(v["end"] - v["start"]) / 1000 for v in turns.values() if v.get("start") and v.get("end")]
    out.update({
        "turns": len(turns),
        "wall_s": round((last - first) / 1000) if first and last else None,
        "model_s": round(sum(gen)),
        "max_prompt": max((u.get("input_tokens", 0) for u in us), default=0),
        "output_tokens": sum(u.get("output_tokens", 0) for u in us),
        "reasoning_tokens": sum(u.get("reasoning_tokens", 0) for u in us),
        "text_chars": sum(v["text"] for v in turns.values()),
        "parallel_turns": sum(1 for v in turns.values() if len(v["tools"]) > 1),
        "tools": collections.Counter(n for v in turns.values() for n in v["tools"]),
    })
    return out


def rescore(case, ws):
    """Run the hidden tests on a copy of the workspace as the model left it."""
    dst = os.path.join(B, "rescore", os.path.basename(os.path.dirname(ws)) + "-" + case)
    shutil.rmtree(dst, ignore_errors=True)
    shutil.copytree(ws, dst, ignore=shutil.ignore_patterns(".dancho"))
    shutil.copytree(os.path.join(CORPUS, case, "hidden"), dst, dirs_exist_ok=True)
    r = subprocess.run(["cargo", "test", "--offline", "--workspace", "--no-fail-fast"], cwd=dst, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=900)
    # Only the case's own test files count, not tests the model wrote.
    ours = {os.path.basename(f) for d in ("hidden", "workspace")
            for f in glob.glob(os.path.join(CORPUS, case, d, "tests", "*.rs"))}
    passed = total = 0
    target = None
    for line in r.stdout.splitlines():
        if m := re.match(r"\s+Running (\S+)", line):
            target = os.path.basename(m[1])
        elif (m := re.match(r"test result: \w+\. (\d+) passed; (\d+) failed", line)) and target in ours:
            passed += int(m[1])
            total += int(m[1]) + int(m[2])
    return {"passes_hidden": r.returncode == 0, "tests": f"{passed}/{total}"}


def split_name(base):
    """Splits <model>-<trial>-<case>.json, matching the case against the corpus,
    since case names hold hyphens and digits too."""
    stem = re.sub(r"\.json(\.tmp)?$", "", base)
    for case in sorted(os.listdir(CORPUS), key=len, reverse=True):
        if stem.endswith("-" + case):
            m = re.match(r"(.+)-(\d+)$", stem[: -len(case) - 1])
            if m:
                return m[1], m[2], case
    return None


def main():
    runs = []
    for f in sorted(glob.glob(os.path.join(B, "results", "*.json*"))):
        base = os.path.basename(f)
        if base.endswith(".err"):
            continue
        parts = split_name(base)
        if not parts:
            continue
        model, trial, case = parts
        text = open(f).read().strip()
        if not text:
            continue  # still running
        try:
            d = json.loads(text)
            d = d[0] if isinstance(d, list) else d
        except ValueError:
            d = json.loads(text.splitlines()[-1])
        row = {"model": model, "trial": int(trial), "case": case,
               "pass": d.get("passed", d.get("pass")), "fails": d.get("failures", d.get("fails"))}
        # A run through dancho leaves a session log beside its workspace; a Claude
        # Code baseline does not.
        log = os.path.join(B, "work", f"{model}-{trial}", case + ".jsonl")
        if os.path.exists(log):
            row["dancho"] = True
            row.update(mine(log))
            if "--rescore" in sys.argv and not row["pass"]:
                row["rescore"] = rescore(case, os.path.join(B, "work", f"{model}-{trial}", case))
        else:
            row.update({"turns": d.get("turns"), "wall_s": d.get("wall_s"), "cost": d.get("cost_usd")})
        runs.append(row)
    json.dump(runs, open(os.path.join(B, "runs.json"), "w"), indent=1, default=dict)

    L = ["# Bench report", "", "| model | trial | case | pass | turns | wall s | notes |", "|---|---|---|---|---|---|---|"]
    for r in runs:
        note = "; ".join(r["fails"] or [])[:90]
        if r.get("rescore"):
            rs = r["rescore"]
            note = f"final code {'PASSES' if rs['passes_hidden'] else 'fails'} hidden, tests {rs['tests']}; " + note
        L.append(f"| {r['model']} | {r['trial']} | {r['case']} | {r['pass']} | {r.get('turns')} | {r.get('wall_s')} | {note} |")
    q = [r for r in runs if r.get("dancho") and "errors" in r]
    errs = collections.Counter((e["tool"], e["kind"]) for r in q for e in r["errors"])
    L += ["", "## dancho tool errors by kind", ""] + [f"- {n} x {t}: {k}" for (t, k), n in errs.most_common()]
    L += ["", "## dancho runs", ""]
    for r in q:
        L.append(f"- {r['model']} {r['trial']}/{r['case']}: turns {r['turns']}, model {r['model_s']}s of {r['wall_s']}s, "
                 f"max prompt {r['max_prompt']} ({100 * r['max_prompt'] // CTX}% of ctx), out {r['output_tokens']} "
                 f"(reasoning {r['reasoning_tokens']}), repeats {r['repeats']}, parallel turns {r['parallel_turns']}, "
                 f"retried {r['retried']}, told turns left {r['told']}, tools {dict(r['tools'])}")
    L += ["", "## exec commands reached for", ""]
    L += [f"- {n} x `{c}`" for c, n in collections.Counter(c for r in q for c in r["exec"]).most_common(40)]
    L += ["", "## 'other' errors (uncategorised, read these)", ""]
    L += [f"- {r['trial']}/{r['case']} t{e['turn']} {e['tool']}: {e['msg'][:200]!r}" for r in q for e in r["errors"] if e["kind"] == "other"]
    open(os.path.join(B, "report.md"), "w").write("\n".join(L) + "\n")
    print("\n".join(L))


main()
