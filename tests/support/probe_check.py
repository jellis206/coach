"""Check Task 1 integration-probe records against the PID the launcher started.

Usage: probe_check.py LOG.jsonl --pid PID [--start LSTART]
Prints failures and the per-event identity matrix. Exit 1 on any failure.
"""
import argparse
import json
import os
import subprocess
import sys

# Processes between a harness and its hook. Anything else is the owner candidate.
WRAPPERS = {"sh", "bash", "zsh", "dash", "fish", "env", "timeout"}
IDENTITY = ("session_id", "turn_id", "tool_use_id", "agent_id", "agent_type", "transcript_path")
LIFECYCLE = {
    "Stop": "idle", "SubagentStop": "idle", "agent_settled": "idle",
    "SessionEnd": "shutdown", "session_shutdown": "shutdown",
}


def _name(comm):
    return os.path.basename(comm.lstrip("-"))


def discover_owner(chain):
    """chain[0] is the hook process; return the nearest non-wrapper ancestor or None."""
    for proc in chain[1:]:
        if _name(proc["comm"]) not in WRAPPERS:
            return proc
    return None


def lifecycle(event):
    return LIFECYCLE.get(event)


def check(records, pid, start):
    if not records:
        return ["no probe records"]
    failures = []
    start = start.strip()
    for i, rec in enumerate(records):
        in_process = rec.get("in_process", False)
        owner = rec["chain"][0] if in_process else discover_owner(rec["chain"])
        where = f"record {i} ({rec['event']})"
        if owner is None:
            failures.append(f"{where}: no owner found")
        elif owner["pid"] == rec["pid"] and not in_process:
            failures.append(f"{where}: owner is the hook process")
        elif owner["pid"] != pid or owner["start"] != start:
            failures.append(f"{where}: owner {owner['pid']}@{owner['start']} != launched {pid}@{start}")
    return failures


def matrix(records):
    rows = {}
    for rec in records:
        rows.setdefault(rec["event"], []).append(set(rec["keys"]))
    out = {}
    for event, key_sets in rows.items():
        row = {}
        for field in IDENTITY:
            hits = sum(field in keys for keys in key_sets)
            row[field] = "present" if hits == len(key_sets) else "sometimes" if hits else "unavailable"
        row["deliveries"] = len(key_sets)
        out[event] = row
    return out


def lstart(pid):
    return subprocess.run(["ps", "-o", "lstart=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--pid", type=int, required=True)
    ap.add_argument("--start", help="ps lstart of the launched process (default: read it now)")
    args = ap.parse_args()
    with open(args.log) as f:
        records = [json.loads(line) for line in f if line.strip()]
    failures = check(records, args.pid, args.start or lstart(args.pid))
    print(json.dumps({"failures": failures, "matrix": matrix(records)}, indent=2))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
