#!/usr/bin/env python3
"""Asks Dino a read-only question and follows the run as it streams.

    python3 docs/sdk_example.py "What does this crate do?"

Reads the JSON lines `dino run --json` writes, as docs/sdk.md describes them.
Standard library only.
"""

import json
import subprocess
import sys


def ask(question, workspace="."):
    command = ["dino", "run", "--json", "--read-only", "--workspace", workspace, question]
    # stdin closed, so any approval the mode would ask for is refused.
    with subprocess.Popen(
        command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, text=True
    ) as dino:
        outcome = None
        for line in dino.stdout:
            record = json.loads(line)
            if record["record"] == "finished":
                outcome = record["outcome"]
            if record["record"] != "event":
                continue
            event = record["event"]
            if event["type"] == "model" and event["event"]["type"] == "text_delta":
                print(event["event"]["text"], end="", flush=True)
            elif event["type"] == "tool_started":
                call = event["call"]
                print(f"\n[{call['name']} {json.dumps(call['arguments'])}]", file=sys.stderr)
            elif event["type"] == "tool_decided" and event["decision"]["outcome"] == "denied":
                print(f"\n[refused: {event['decision']['reason']}]", file=sys.stderr)
        print()
    return dino.returncode, outcome


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    code, outcome = ask(sys.argv[1])
    print(f"exit {code}: {outcome}", file=sys.stderr)
    sys.exit(code)
