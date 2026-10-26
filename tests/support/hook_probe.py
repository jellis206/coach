#!/usr/bin/env python3
"""Fail-open Task 1 hook probe for Claude Code and Codex command hooks.

Usage (as a hook command): hook_probe.py <harness>
Appends one JSONL record to $COACH_PROBE_DIR/<harness>.jsonl containing payload
key names and value types, the process ancestry (pid/ppid/start/executable) and
the names (never values) of harness environment variables. No payload values,
argv or environment values are stored. Always exits 0 with empty stdout.
"""
import json
import os
import re
import subprocess
import sys
import time

MAX_STDIN = 1 << 20
# Low-cardinality fields whose values are vocabulary, not user content.
ENUM_FIELDS = ("reason", "source", "tool_name", "agent_type", "permission_mode", "is_interrupt",
               "stop_hook_active", "notification_type", "trigger", "model")


def shape(value, depth=0):
    if isinstance(value, dict):
        return {k: shape(v, depth + 1) for k, v in value.items()} if depth < 2 else "object"
    if isinstance(value, list):
        return "array"
    return type(value).__name__


def ancestry(pid):
    chain = []
    while pid > 1 and len(chain) < 32:
        out = subprocess.run(["ps", "-o", "ppid=,lstart=,comm=", "-p", str(pid)],
                             capture_output=True, text=True).stdout.strip()
        if not out:
            break
        ppid, rest = out.split(None, 1)
        start, comm = rest[:24], rest[24:].strip()
        chain.append({"pid": pid, "ppid": int(ppid), "start": start, "comm": comm})
        pid = int(ppid)
    return chain


def main():
    out_dir = os.environ.get("COACH_PROBE_DIR")
    if not out_dir or len(sys.argv) < 2:
        return
    raw = sys.stdin.buffer.read(MAX_STDIN + 1)
    try:
        payload = json.loads(raw[:MAX_STDIN])
    except ValueError:
        payload = {}
    payload = payload if isinstance(payload, dict) else {}
    rec = {
        "harness": sys.argv[1],
        "event": payload.get("hook_event_name", "unknown"),
        "received_ms": int(time.time() * 1000),
        "pid": os.getpid(),
        "chain": ancestry(os.getpid()),
        "keys": sorted(payload),
        "shape": shape(payload),
        "enums": {k: payload[k] for k in ENUM_FIELDS if isinstance(payload.get(k), (str, bool))},
        "stdin_bytes": len(raw),
        "env_names": sorted(k for k in os.environ if re.match(r"(CLAUDE|CODEX|PI)_", k)),
        "env_pids": {k: v for k, v in os.environ.items()
                     if re.fullmatch(r"(CLAUDE|CODEX|PI)_\w*PID", k) and v.isdigit()},
    }
    os.makedirs(out_dir, mode=0o700, exist_ok=True)
    fd = os.open(os.path.join(out_dir, f"{sys.argv[1]}.jsonl"), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    with os.fdopen(fd, "a") as f:
        f.write(json.dumps(rec) + "\n")


if __name__ == "__main__":
    try:
        main()
    except Exception:
        pass
    sys.exit(0)
