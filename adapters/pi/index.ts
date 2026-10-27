// Coach observer for Pi. Forwards bounded observations to the local Coach daemon
// over its Unix socket. Never blocks, mutates, or answers for the agent.
//
// Launcher lineage contract (environment, all optional):
//   COACH_PARENT_AGENT_ID  the launching agent's id or session id
//   COACH_RUN_ID           dispatcher run/batch id
//   COACH_AGENT_ROLE       e.g. worker, validator
// This extension sets COACH_AGENT_ID for its own process at session start so an
// in-process launcher (another extension) can pass it to children. Pi's bash tool
// snapshots its environment earlier, so shell launches use $PI_SESSION_ID instead.
import { execFile, spawn } from "node:child_process";
import { createConnection, type Socket } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";
import type {
  ExtensionAPI,
  ExtensionContext,
  InputEvent,
  SessionShutdownEvent,
  SessionStartEvent,
  ToolExecutionEndEvent,
  ToolExecutionStartEvent,
  UIPromptEndEvent,
  UIPromptStartEvent,
} from "@earendil-works/pi-coding-agent";

export const MAX_QUEUE = 1024;
const MAX_FRAME_BYTES = 512 * 1024;
const MAX_STRING_CHARS = 16 * 1024; // <= 64 KiB of UTF-8; the daemon enforces the byte cap
const MAX_ARRAY = 200;
const MAX_DEPTH = 8;
const MAX_BACKLOG_BYTES = 1024 * 1024;
const LINEAGE = /^[\w.:@/-]{1,200}$/;

// Mirrors src/paths.rs default_state_dir().
function stateDir(): string {
  const env = process.env;
  if (env.COACH_STATE_DIR) return env.COACH_STATE_DIR;
  if (process.platform === "darwin") return join(homedir(), "Library", "Application Support", "Coach");
  if (env.XDG_STATE_HOME?.startsWith("/")) return join(env.XDG_STATE_HOME, "coach");
  return join(homedir(), ".local", "state", "coach");
}

const coachBin = () => process.env.COACH_BIN || "coach";

export class Transport {
  private queue: string[] = [];
  private sock: Socket | undefined;
  private ready = false;
  private closed = false;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private attempt = 0;
  private spawned = false;
  private droppedCount = 0;

  constructor(private readonly socketPath: string) {}

  send(message: object): void {
    if (this.closed) return;
    if (this.queue.length >= MAX_QUEUE) {
      this.droppedCount++;
      return;
    }
    this.queue.push(JSON.stringify(message) + "\n");
    if (this.ready) this.flush();
    else this.connect();
  }

  close(): void {
    this.closed = true;
    clearTimeout(this.timer);
    this.timer = undefined;
    this.sock?.end();
    this.sock = undefined;
    this.ready = false;
  }

  queued = () => this.queue.length;
  dropped = () => this.droppedCount;
  timers = () => (this.timer ? 1 : 0);

  private connect(): void {
    if (this.sock || this.timer || this.closed) return;
    const sock = createConnection(this.socketPath);
    this.sock = sock;
    sock.unref();
    sock.on("connect", () => {
      this.ready = true;
      this.attempt = 0;
      sock.write(JSON.stringify({ type: "register_peer", source: "pi" }) + "\n");
      this.flush();
    });
    sock.on("drain", () => this.flush());
    sock.on("error", () => {});
    sock.on("close", () => {
      if (this.sock === sock) {
        // A working connection was lost (daemon crash): allow one more restart.
        if (this.ready) this.spawned = false;
        this.sock = undefined;
        this.ready = false;
      }
      this.retry();
    });
  }

  private retry(): void {
    if (this.timer || this.closed) return;
    if (!this.spawned) {
      this.spawned = true;
      try {
        const child = spawn(coachBin(), ["daemon"], { detached: true, stdio: "ignore" });
        child.on("error", () => {});
        child.unref();
      } catch {
        // no coach binary: stay best-effort
      }
    }
    const delay = Math.min(5000, 100 * 2 ** this.attempt++);
    this.timer = setTimeout(() => {
      this.timer = undefined;
      this.connect();
    }, delay);
    this.timer.unref?.();
  }

  private flush(): void {
    const sock = this.sock;
    while (sock && this.ready && this.queue.length && sock.writableLength < MAX_BACKLOG_BYTES) {
      sock.write(this.queue.shift()!);
    }
  }
}

type Truncation = { path: string; original_bytes: number };

function bound(value: unknown, path: string, notes: Truncation[], depth = 0): unknown {
  if (typeof value === "string") {
    if (value.length <= MAX_STRING_CHARS) return value;
    notes.push({ path, original_bytes: Buffer.byteLength(value) });
    return value.slice(0, MAX_STRING_CHARS);
  }
  if (value === null || typeof value !== "object") return typeof value === "function" ? undefined : value;
  if (depth >= MAX_DEPTH) {
    notes.push({ path, original_bytes: 0 });
    return "[depth limit]";
  }
  if (Array.isArray(value)) {
    if (value.length > MAX_ARRAY) notes.push({ path, original_bytes: 0 });
    return value.slice(0, MAX_ARRAY).map((v, i) => bound(v, `${path}/${i}`, notes, depth + 1));
  }
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(value)) {
    out[k] = bound(v, `${path}/${k.replace(/~/g, "~0").replace(/\//g, "~1")}`, notes, depth + 1);
  }
  return out;
}

function lineage(name: string): string | null {
  const v = process.env[name];
  return v && LINEAGE.test(v) ? v : null;
}

let sequence = 0;

export default function coach(pi: ExtensionAPI): Transport | undefined {
  // No sockets, timers or processes here: the factory can run without a session.
  let transport: Transport | undefined;
  let sessionId: string | null = null;
  let agentId: string | null = null;
  let mode = "unknown";
  const waits: { id: string; started: number }[] = [];

  const emit = (kind: string, raw: object, extra: { tool_call_id?: string; payload?: object } = {}) => {
    if (!transport) return;
    const truncated: Truncation[] = [];
    const seq = sequence++;
    const event = {
      schema_version: 1,
      event_id:
        extra.tool_call_id && kind.startsWith("tool_")
          ? `pi:${sessionId ?? "-"}:${kind}:${extra.tool_call_id}`
          : `pi-${process.pid}-${Date.now().toString(36)}-${seq}`,
      process_instance_id: "peer",
      session_id: sessionId,
      agent_id: agentId,
      parent_agent_id: lineage("COACH_PARENT_AGENT_ID"),
      turn_id: null,
      tool_call_id: extra.tool_call_id ?? null,
      source: "pi",
      source_version: null,
      kind,
      occurred_at_ms: Date.now(),
      sequence: seq,
      payload: {
        source_event: (raw as { type?: string }).type ?? kind,
        mode,
        run_id: lineage("COACH_RUN_ID"),
        role: lineage("COACH_AGENT_ROLE"),
        ...extra.payload,
        raw: bound(raw, "/raw", truncated),
      } as Record<string, unknown>,
      capture_status: { unknown_kind: false, parse_error: false, truncated, redacted: [] as string[] },
    };
    if (Buffer.byteLength(JSON.stringify(event)) > MAX_FRAME_BYTES - 1024) {
      event.payload.raw = { coach_degraded: "frame_too_large", keys: Object.keys(raw) };
      truncated.push({ path: "/raw", original_bytes: 0 });
    }
    transport.send({ type: "event", event });
  };

  // Every observer swallows its own errors and returns undefined (no mutation).
  const observe = <E>(fn: (event: E, ctx: ExtensionContext) => void) => (event: E, ctx: ExtensionContext) => {
    try {
      fn(event, ctx);
    } catch {
      // never disturb the agent
    }
    return undefined;
  };

  pi.on(
    "session_start",
    observe<SessionStartEvent>((event, ctx) => {
      transport?.close();
      transport = new Transport(join(stateDir(), "coach.sock"));
      mode = ctx.mode ?? "unknown";
      sessionId = ctx.sessionManager?.getSessionId?.() ?? null;
      agentId = `pi:${sessionId ?? process.pid}`;
      process.env.COACH_AGENT_ID = agentId;
      emit("session_start", event);
    }),
  );
  pi.on(
    "session_shutdown",
    observe<SessionShutdownEvent>((event) => {
      if (!transport) return;
      emit("session_end", event);
      transport.close();
    }),
  );
  pi.on("input", observe<InputEvent>((event) => emit("prompt", event)));
  pi.on(
    "tool_execution_start",
    observe<ToolExecutionStartEvent>((event) =>
      emit("tool_start", event, {
        tool_call_id: event.toolCallId,
        payload: { parent_tool_call_id: event.parentToolCallId ?? null },
      }),
    ),
  );
  pi.on(
    "tool_execution_end",
    observe<ToolExecutionEndEvent>((event) =>
      emit(event.isError ? "tool_failure" : "tool_result", event, {
        tool_call_id: event.toolCallId,
        payload: {
          outcome: event.isError ? "error" : "success",
          outcome_basis: "is_error",
          parent_tool_call_id: event.parentToolCallId ?? null,
        },
      }),
    ),
  );
  pi.on(
    "ui_prompt_start",
    observe<UIPromptStartEvent>((event) => {
      const id = `w${sequence}-${Date.now().toString(36)}`;
      waits.push({ id, started: Date.now() });
      emit("ui_wait_start", event, { payload: { wait_id: id } });
    }),
  );
  pi.on(
    "ui_prompt_end",
    observe<UIPromptEndEvent>((event) => {
      const w = waits.pop();
      emit("ui_wait_end", event, { payload: { wait_id: w?.id ?? null, wait_ms: w ? Date.now() - w.started : null } });
    }),
  );
  pi.on("agent_settled", observe((event: object) => emit("idle", event)));

  pi.registerCommand("coach-debrief", {
    description: "Show Coach's local debrief for this session",
    handler: async (_args, ctx) => {
      execFile(coachBin(), ["debrief", "--session", sessionId ?? ""], { timeout: 30_000 }, (err, stdout, stderr) => {
        const text = err ? `coach debrief failed: ${stderr || err.message}` : stdout;
        if (ctx.hasUI) ctx.ui.notify(text, err ? "error" : "info");
        else process.stdout.write(text);
      });
    },
  });

  return {
    get transport() {
      return transport;
    },
    queued: () => transport?.queued() ?? 0,
    dropped: () => transport?.dropped() ?? 0,
    timers: () => transport?.timers() ?? 0,
  } as unknown as Transport;
}
