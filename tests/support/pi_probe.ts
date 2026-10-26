// Task 1 Pi probe: `pi --extension tests/support/pi_probe.ts` with COACH_PROBE_DIR set.
// Appends event names, payload key shapes and process identity to $COACH_PROBE_DIR/pi.jsonl.
// Stores no payload values. Observers return undefined (no mutations).
import { execFileSync } from "node:child_process";
import { appendFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const EVENTS = [
  "session_start", "session_shutdown", "session_before_switch", "session_before_fork",
  "input", "before_agent_start", "agent_start", "agent_end", "agent_settled",
  "turn_start", "turn_end", "tool_call", "tool_result", "tool_execution_start", "tool_execution_end",
  "ui_prompt_start", "ui_prompt_end", "user_bash",
] as const;

function shape(value: unknown, depth = 0): unknown {
  if (Array.isArray(value)) return "array";
  if (value && typeof value === "object") {
    if (depth >= 2) return "object";
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, shape(v, depth + 1)]));
  }
  return typeof value;
}

function lstart(pid: number): string {
  try {
    return execFileSync("ps", ["-o", "lstart=", "-p", String(pid)], { encoding: "utf8" }).trim();
  } catch {
    return "";
  }
}

export default function (pi: ExtensionAPI) {
  const dir = process.env.COACH_PROBE_DIR;
  if (!dir) return;
  let start = "";
  for (const name of EVENTS) {
    pi.on(name as never, ((event: Record<string, unknown>, ctx: { hasUI?: boolean }) => {
      try {
        if (!start) start = lstart(process.pid);
        mkdirSync(dir, { recursive: true, mode: 0o700 });
        const rec = {
          harness: "pi",
          event: name,
          received_ms: Date.now(),
          pid: process.pid,
          in_process: true,
          chain: [{ pid: process.pid, ppid: process.ppid, start, comm: process.argv0 }],
          keys: Object.keys(event ?? {}).sort(),
          shape: shape(event),
          has_ui: ctx?.hasUI,
          env: Object.fromEntries(
            ["COACH_PARENT_AGENT_ID", "COACH_RUN_ID", "COACH_AGENT_ROLE"].map((k) => [k, process.env[k] ? "set" : "unset"]),
          ),
        };
        appendFileSync(join(dir, "pi.jsonl"), JSON.stringify(rec) + "\n", { mode: 0o600 });
      } catch {
        // fail open
      }
      return undefined;
    }) as never);
  }
}
