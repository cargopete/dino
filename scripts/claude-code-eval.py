"""Run the eval corpus through headless Claude Code, judged by the corpus's own assertions.

This is a baseline for comparison, not part of the harness: it measures a model
and Claude Code together, the way RFC-0017 section 8.5 reports them. It mirrors
the checks in dino-eval's runner and prints one JSON line per case.

usage: python3.12 -I scripts/claude-code-eval.py <corpus> <out_dir> <model> [case ...]
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
import tomllib

SKIP = {"target", ".dino", ".claude"}


def fingerprint(root):
    out = {}
    for d, dirs, files in os.walk(root):
        dirs[:] = [x for x in dirs if x not in SKIP]
        for f in files:
            p = os.path.join(d, f)
            with open(p, "rb") as fh:
                out[os.path.relpath(p, root)] = hashlib.sha256(fh.read()).hexdigest()
    return out


def cargo_errors(ws):
    r = subprocess.run(["cargo", "check", "--offline", "--all-targets", "--message-format", "json", "-q"],
                       cwd=ws, capture_output=True, text=True)
    n = sum(1 for line in r.stdout.splitlines()
            if line.startswith("{") and (m := json.loads(line)).get("reason") == "compiler-message"
            and m["message"]["level"] == "error")
    return n if n or r.returncode == 0 else 1


def read(ws, rel):
    p = os.path.join(ws, rel)
    return open(p).read() if os.path.exists(p) else None


def run_case(corpus, out, model, name):
    case_dir = os.path.join(corpus, name)
    with open(os.path.join(case_dir, "case.toml"), "rb") as fh:
        case = tomllib.load(fh)
    ws = os.path.join(out, name)
    shutil.rmtree(ws, ignore_errors=True)
    shutil.copytree(os.path.join(case_dir, "workspace"), ws)
    before = fingerprint(ws)
    cmd = ["claude", "-p", case["prompt"], "--model", model, "--output-format", "json",
           "--permission-mode", "acceptEdits",
           "--allowedTools", "Read", "Edit", "Write", "Glob", "Grep", "Bash(cargo:*)",
           "--setting-sources", "project", "--strict-mcp-config", "--no-session-persistence"]
    started = time.time()
    try:
        r = subprocess.run(cmd, cwd=ws, capture_output=True, text=True, timeout=3600)
        res = json.loads(r.stdout)
    except subprocess.TimeoutExpired:
        res = {"error": "timed out after an hour"}
    except ValueError:
        res = {"error": (r.stdout + r.stderr)[-500:]}
    wall = time.time() - started

    after = fingerprint(ws)
    changed = sorted({k for k in after if before.get(k) != after[k]} | (before.keys() - after.keys()))
    exp, answer, fails = case.get("expect", {}), res.get("result") or "", []
    if res.get("is_error") or "error" in res:
        fails.append(f"run did not complete: {res.get('error') or res.get('subtype')}")
    if not changed and not exp.get("answer_contains"):
        fails.append("nothing in the workspace changed")
    fails += [f"answer lacks {w!r}" for w in exp.get("answer_contains", []) if w not in answer]
    fails += [f"{f} unchanged" for f in exp.get("files_changed", []) if f not in changed]
    fails += [f"{f} changed" for f in exp.get("files_unchanged", []) if f in changed]
    for c in exp.get("contains", []):
        if c["text"] not in (read(ws, c["file"]) or ""):
            fails.append(f"{c['file']} lacks {c['text']!r}")
    for c in exp.get("not_contains", []):
        if c["text"] in (read(ws, c["file"]) or ""):
            fails.append(f"{c['file']} contains {c['text']!r}")
    if exp.get("cargo_check"):
        e = cargo_errors(ws)
        if (exp["cargo_check"] == "clean") != (e == 0):
            fails.append(f"cargo check: {e} error(s), expected {exp['cargo_check']}")
    if exp.get("hidden_tests"):
        shutil.copytree(os.path.join(case_dir, "hidden"), ws, dirs_exist_ok=True)
        try:
            t = subprocess.run(["cargo", "test", "--offline", "--workspace"], cwd=ws,
                               capture_output=True, text=True, timeout=600)
            if t.returncode != 0:
                lines = [l for l in (t.stdout + t.stderr).splitlines()
                         if l.startswith("test result:") or "panicked at" in l or l.startswith("error")]
                fails.append("hidden tests failed: " + " | ".join(lines[:6]))
        except subprocess.TimeoutExpired:
            fails.append("hidden tests timed out")

    u = res.get("usage", {})
    row = {"model": model, "case": name, "pass": not fails, "fails": fails,
           "turns": res.get("num_turns"), "wall_s": round(wall, 1), "cost_usd": res.get("total_cost_usd"),
           "input": u.get("input_tokens", 0), "cache_read": u.get("cache_read_input_tokens", 0),
           "cache_write": u.get("cache_creation_input_tokens", 0), "output": u.get("output_tokens", 0)}
    print(json.dumps(row), flush=True)


def main():
    corpus, out, model, *cases = sys.argv[1:]
    corpus = os.path.abspath(corpus)
    os.makedirs(out, exist_ok=True)
    cases = cases or sorted(d for d in os.listdir(corpus)
                            if os.path.isfile(os.path.join(corpus, d, "case.toml")))
    for name in cases:
        run_case(corpus, out, model, name)


main()
