import { afterEach, beforeEach, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import coach, { MAX_QUEUE, type Transport } from "./index.ts";

type Handler = (event: unknown, ctx: unknown) => unknown;

function fakePi() {
  const handlers = new Map<string, Handler[]>();
  const commands = new Map<string, unknown>();
  const api = {
    on(name: string, h: Handler) {
      handlers.set(name, [...(handlers.get(name) ?? []), h]);
      return () => {};
    },
    registerCommand(name: string, options: unknown) {
      commands.set(name, options);
    },
  };
  const emit = (name: string, event: object, ctx: object = headless) =>
    (handlers.get(name) ?? []).map((h) => h({ type: name, ...event }, ctx));
  return { api, handlers, commands, emit };
}

const headless = { mode: "json", hasUI: false, cwd: "/tmp/demo", sessionManager: { getSessionId: () => "sess-pi-1" } };

let dir: string;
let server: Server;
let lines: Record<string, any>[];
let connections: number;
const open: Socket[] = [];

beforeEach(async () => {
  dir = mkdtempSync(join(tmpdir(), "coach-pi-"));
  process.env.COACH_STATE_DIR = dir;
  process.env.COACH_BIN = "/nonexistent/coach"; // never spawn a real daemon from tests
  delete process.env.COACH_PARENT_AGENT_ID;
  delete process.env.COACH_RUN_ID;
  delete process.env.COACH_AGENT_ROLE;
  lines = [];
  connections = 0;
  server = createServer((sock) => {
    connections++;
    open.push(sock);
    let buf = "";
    sock.on("data", (d) => {
      buf += d.toString();
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        lines.push(JSON.parse(buf.slice(0, i)));
        buf = buf.slice(i + 1);
      }
    });
  });
  await new Promise<void>((r) => server.listen(join(dir, "coach.sock"), r));
});

afterEach(async () => {
  for (const s of open.splice(0)) s.destroy();
  await new Promise<void>((r) => server.close(() => r()));
  rmSync(dir, { recursive: true, force: true });
});

const settle = (ms = 80) => new Promise((r) => setTimeout(r, ms));
const events = () => lines.filter((l) => l.type === "event").map((l) => l.event);

async function started() {
  const pi = fakePi();
  coach(pi.api as never);
  pi.emit("session_start", { reason: "startup" });
  await settle();
  return pi;
}

test("factory opens no resources", async () => {
  const pi = fakePi();
  coach(pi.api as never);
  await settle();
  expect(connections).toBe(0);
  expect(pi.handlers.has("session_start")).toBe(true);
  expect(pi.commands.has("coach-debrief")).toBe(true);
});

test("observers return undefined", async () => {
  const pi = await started();
  for (const [name, hs] of pi.handlers) {
    for (const h of hs) expect(h({ type: name, text: "x", toolCallId: "t", toolName: "bash", isError: false, kind: "confirm", reason: "quit", aborted: false }, headless)).toBeUndefined();
  }
});

test("session start registers by peer and is collected headless", async () => {
  await started();
  expect(lines[0]).toEqual({ type: "register_peer", source: "pi" });
  const [e] = events();
  expect(e.kind).toBe("session_start");
  expect(e.source).toBe("pi");
  expect(e.process_instance_id).toBe("peer");
  expect(e.session_id).toBe("sess-pi-1");
  expect(e.payload.raw.reason).toBe("startup");
  expect(e.payload.mode).toBe("json");
});

test("tool outcome comes from isError and nested tools are not subagents", async () => {
  const pi = await started();
  pi.emit("tool_execution_start", { toolCallId: "t1", toolName: "bash", args: { command: "ls" }, parentToolCallId: "outer" });
  pi.emit("tool_execution_end", { toolCallId: "t1", toolName: "bash", result: {}, isError: true, durationMs: 3, parentToolCallId: "outer" });
  pi.emit("tool_execution_end", { toolCallId: "t2", toolName: "read", result: {}, isError: false });
  await settle();
  const [, start, fail, ok] = events();
  expect(start.kind).toBe("tool_start");
  expect(start.tool_call_id).toBe("t1");
  expect(start.payload.parent_tool_call_id).toBe("outer");
  expect(start.parent_agent_id).toBeNull();
  expect(fail.kind).toBe("tool_failure");
  expect(fail.payload.outcome).toBe("error");
  expect(ok.kind).toBe("tool_result");
  expect(ok.payload.outcome).toBe("success");
});

test("ui prompt waits correlate by nesting", async () => {
  const pi = await started();
  pi.emit("ui_prompt_start", { reason: "ui_prompt", kind: "confirm" });
  pi.emit("ui_prompt_start", { reason: "ui_prompt", kind: "select" });
  pi.emit("ui_prompt_end", { reason: "ui_prompt", kind: "select" });
  pi.emit("ui_prompt_end", { reason: "ui_prompt", kind: "confirm" });
  pi.emit("ui_prompt_end", { reason: "ui_prompt", kind: "confirm" });
  await settle();
  const waits = events().filter((e) => e.kind.startsWith("ui_wait"));
  expect(waits.map((w) => w.kind)).toEqual(["ui_wait_start", "ui_wait_start", "ui_wait_end", "ui_wait_end", "ui_wait_end"]);
  expect(waits[2].payload.wait_id).toBe(waits[1].payload.wait_id);
  expect(waits[3].payload.wait_id).toBe(waits[0].payload.wait_id);
  expect(waits[3].payload.wait_ms).toBeGreaterThanOrEqual(0);
  expect(waits[4].payload.wait_id).toBeNull(); // unmatched end stays unmatched
});

test("explicit launcher lineage is used; malformed lineage stays unknown", async () => {
  process.env.COACH_PARENT_AGENT_ID = "pi:parent-1";
  process.env.COACH_RUN_ID = "run-9";
  process.env.COACH_AGENT_ROLE = "validator";
  let [e] = events();
  await started();
  [e] = events();
  expect(e.parent_agent_id).toBe("pi:parent-1");
  expect(e.payload.run_id).toBe("run-9");
  expect(e.payload.role).toBe("validator");
  expect(process.env.COACH_AGENT_ID).toBe(e.agent_id);

  lines = [];
  process.env.COACH_PARENT_AGENT_ID = "bad\nvalue";
  await started();
  expect(events()[0].parent_agent_id).toBeNull();
});

test("input and settled map to prompt and idle", async () => {
  const pi = await started();
  pi.emit("input", { text: "hello", source: "interactive" });
  pi.emit("agent_settled", { aborted: false });
  await settle();
  const [, prompt, idle] = events();
  expect(prompt.kind).toBe("prompt");
  expect(prompt.payload.raw.text).toBe("hello");
  expect(idle.kind).toBe("idle");
});

test("large values are bounded before framing", async () => {
  const pi = await started();
  pi.emit("tool_execution_end", { toolCallId: "big", toolName: "bash", result: { content: [{ text: "z".repeat(3_000_000) }] }, isError: false });
  await settle(200);
  const e = events().find((x) => x.tool_call_id === "big");
  expect(e).toBeDefined();
  expect(JSON.stringify(e).length).toBeLessThan(512 * 1024);
  expect(e.capture_status.truncated.length).toBeGreaterThan(0);
});

test("reload closes idempotently and reconnects without unregistering", async () => {
  const pi = await started();
  pi.emit("session_shutdown", { reason: "reload" });
  pi.emit("session_shutdown", { reason: "reload" });
  await settle();
  const pi2 = fakePi();
  coach(pi2.api as never);
  pi2.emit("session_start", { reason: "reload" });
  await settle();
  expect(connections).toBe(2);
  expect(lines.some((l) => l.type === "unregister")).toBe(false);
  expect(events().map((e) => e.kind)).toEqual(["session_start", "session_end", "session_start"]);
});

test("every lost working connection may restart the daemon", async () => {
  const bin = join(dir, "fake-coach");
  writeFileSync(bin, `#!/bin/sh\necho "$1" >> "${join(dir, "spawns")}"\n`, { mode: 0o755 });
  process.env.COACH_BIN = bin;
  const pi = await started();
  const spawns = () => (existsSync(join(dir, "spawns")) ? readFileSync(join(dir, "spawns"), "utf-8").split("\n").filter(Boolean) : []);
  for (let crash = 1; crash <= 2; crash++) {
    for (const s of open.splice(0)) s.destroy(); // the daemon died
    const until = Date.now() + 3000;
    while ((spawns().length < crash || connections <= crash) && Date.now() < until) await settle(50);
    expect(spawns()).toEqual(Array(crash).fill("daemon"));
  }
  pi.emit("session_shutdown", { reason: "quit" });
});

test("unreachable daemon bounds the queue and keeps one retry timer", async () => {
  await new Promise<void>((r) => server.close(() => r()));
  rmSync(join(dir, "coach.sock"), { force: true });
  const pi = fakePi();
  const transport: Transport = coach(pi.api as never)!;
  pi.emit("session_start", { reason: "startup" });
  for (let i = 0; i < MAX_QUEUE + 500; i++) pi.emit("input", { text: `m${i}`, source: "rpc" });
  await settle(50);
  expect(transport.queued()).toBeLessThanOrEqual(MAX_QUEUE);
  expect(transport.dropped()).toBeGreaterThanOrEqual(500);
  expect(transport.timers()).toBeLessThanOrEqual(1);
  pi.emit("session_shutdown", { reason: "quit" });
  expect(transport.timers()).toBe(0);
  server = createServer();
  await new Promise<void>((r) => server.listen(join(dir, "coach.sock"), r));
});
