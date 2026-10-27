# Integration contracts

What each harness actually emits, verified by live synthetic probes on macOS. Linux behaviour is not yet verified live; see [Linux coverage](#linux-coverage).

Probe tooling lives in `tests/support/`:

- `hook_probe.py <harness>`: a fail-open command hook. It records payload key names and value types, a small allowlist of vocabulary values (`reason`, `source`, `tool_name`, …), the process ancestry (pid, ppid, start time, executable) and the names of harness environment variables. It never records payload values, argv or environment values, and it always exits 0 with empty stdout.
- `pi_probe.ts`: the same probe as a Pi extension, loaded with `pi --extension`.
- `probe_check.py LOG --pid PID --start LSTART`: discovers the owner of every record and compares it with the PID and start time the launcher got from `$!`. A missing or mismatched owner fails. It also prints the identity matrix.

Raw probe output stays in a private directory outside any checkout. `tests/fixtures/<harness>/shapes.json` holds only the sanitized key/type shapes.

## Process ownership

| Harness (version) | Hook process parent | Owner discovery | Live result |
|---|---|---|---|
| Claude Code 2.1.311 | The `claude` process itself. No shell wrapper was observed between harness and hook. | Nearest non-shell ancestor. Claude also exports `CLAUDE_PID` = owner PID to hooks. | 18/18 headless and 10/10 interactive deliveries matched `$!` PID and start time. |
| Pi 1.4.0 | Not applicable: extensions run in the Pi process. | `process.pid`. | 21/21 deliveries in `--mode json -p --no-session` matched `$!`. |
| Codex 0.164.0 | Verified only through Coach's own attribution. | Nearest non-shell ancestor. | 6/6 events from one `codex exec` were attributed to a single owner process, with none unknown. See [Codex](#codex). |

Rules the daemon must follow:

- **Identity is (pid, start time), never an executable name.** `pi` launches as `/usr/bin/env node …`. At `$!` the executable was `/usr/bin/env`; after exec it was `node`. PID and start time stayed the same.
- **Never pick "the nearest harness-looking ancestor".** When a harness runs under another harness, both appear in the chain. In the probe, a child `claude -p` ran under the authoring Claude session. Take the hook's nearest non-shell ancestor. Prefer the harness-provided PID variable when one exists, and check that it is an ancestor.
- **A missing owner is a diagnostic, not a guess.** `probe_check.discover_owner` returns `None` when only shells are found.

## Lifecycle

| Signal | Claude Code | Pi | Meaning for Coach |
|---|---|---|---|
| Session start | `SessionStart` (`source`: `startup`, `clear`) | `session_start` (`reason`) | Conversation began. Not a process start. |
| Idle | `Stop`, `SubagentStop` | `agent_end`, `agent_settled` | Turn idle. **Not** work completion. |
| Session end | `SessionEnd` (`reason`: `clear`, `prompt_input_exit`, `other`) | `session_shutdown` (`reason`) | Conversation ended. **Not** a process exit. |
| Process exit | No event | No event | Only liveness polling detects it. |

Observed facts:

- `/clear` in Claude produced `SessionEnd(reason=clear)` and then `SessionStart(source=clear)` from the **same owner PID**. Session ownership and process ownership are separate.
- `SIGKILL` of `claude -p` mid-tool produced `SessionStart`, `UserPromptSubmit` and `PreToolUse`, then nothing. Crash detection must poll owner liveness.
- With an asynchronous subagent, Claude fired `Stop` before `SubagentStop`. The background result then re-entered as another `UserPromptSubmit` and `Stop`. One task can therefore produce several `Stop` events.
- A Claude `-p` run that failed argument parsing still fired `SessionStart` and `SessionEnd`.

## Identity fields

`present` means the field was in every delivery of that event. `sometimes` means some deliveries had it: Claude adds `agent_id` and `agent_type` only to subagent tool calls. `unavailable` means it was never observed.

### Claude Code 2.1.311

| Event | session_id | tool_use_id | agent_id | transcript_path |
|---|---|---|---|---|
| SessionStart / SessionEnd / UserPromptSubmit / Stop | present | unavailable | unavailable | present |
| PreToolUse / PostToolUse | present | present | sometimes | present |
| PostToolUseFailure | present | present | unavailable in probe | present |
| PermissionRequest | present | **unavailable** | unavailable | present |
| Notification (`permission_prompt`) | present | unavailable | unavailable | present |
| SubagentStart / SubagentStop | present | unavailable | present | present |

- Failure: `PostToolUseFailure` carries `error` (string), `is_interrupt` and `duration_ms`. A failing shell command arrived as a failure event, not as a successful result with a nonzero exit.
- Permission: `PermissionRequest` has `tool_name`, `tool_input` and `permission_suggestions`, but no `tool_use_id`. Correlate it to the preceding `PreToolUse` by session and order, and label the link approximate. When the prompt was denied, no `PostToolUse`/`PostToolUseFailure` and no `Stop` followed. No event reports the human's answer or how long they took. Coach must not infer either.
- Subagents: the `Agent` tool's `PostToolUse.tool_response` has `isAsync` and `agentId`. `SubagentStop` has `agent_transcript_path`.
- Prompt events fire in `-p` mode. `PermissionRequest` did not fire in `-p` mode under the user's `bypassPermissions` default, so the interactive case was tested with a session-only `--permission-mode default`.

### Pi 1.4.0

- Events observed headless: `session_start`, `input`, `before_agent_start`, `agent_start`, `turn_start`/`turn_end`, `tool_call`, `tool_execution_start`/`end`, `tool_result`, `agent_end`, `agent_settled`, `session_shutdown`.
- `tool_execution_end` has `toolCallId`, `toolName`, `isError` and `durationMs`, so failures are explicit.
- `ctx.hasUI` was `false` in JSON print mode. Collection must not depend on the UI.
- No subagent identity is exposed by Pi itself. Workers launched by a dispatcher need explicit `COACH_PARENT_AGENT_ID`, `COACH_RUN_ID` and `COACH_AGENT_ROLE` environment variables (Task 5/8).
- `ui_prompt_start`/`ui_prompt_end` cannot fire headless. They carry no ID, so the adapter pairs them by nesting (LIFO) and labels an unmatched end `wait_id: null`. This is unit-tested only; no live TUI prompt has been triggered yet.

### Pi adapter (`adapters/pi/index.ts`)

- **Registration.** The adapter registers with `register_peer`. The daemon reads the connecting PID from socket credentials (macOS `LOCAL_PEERPID`, Linux `SO_PEERCRED`) and identifies it itself. Events on that connection say `process_instance_id: "peer"`, and the daemon substitutes the verified identity. Live headless and TUI runs matched `$!`.
- **Kinds.** `input` becomes prompt. `tool_execution_end.isError` decides tool_failure or tool_result. `agent_settled` becomes idle. `session_shutdown` becomes session_end with Pi's `reason`.
- **Nested tools.** `parentToolCallId` becomes `payload.parent_tool_call_id`, never `parent_agent_id`.
- **Reload.** `/reload` produced `session_end(reload)` and then `session_start(reload)` from the same PID, with the lease kept (`leases: 1`). The adapter never unregisters; process liveness ends the lease.
- **Launcher lineage.** The adapter sets `COACH_AGENT_ID=pi:<session id>` in its own process. A launcher passes that value to children as `COACH_PARENT_AGENT_ID`, plus `COACH_RUN_ID` and `COACH_AGENT_ROLE`. Values must match `^[\w.:@/-]{1,200}$`; anything else is recorded as unknown. This was verified live in `--mode json -p --no-session`.
- **Transport.** One connection per runtime. The queue is bounded at 1024 frames, with a dropped-frame count. There is a single reconnect timer with backoff capped at 5 s, and at most one asynchronous `coach daemon` spawn. The socket is `unref`'d, so it never delays Pi's exit. Strings are cut at 16 Ki chars and arrays at 200 items before framing, and the daemon applies the byte caps again.
- **Tested floor.** Pi 1.4.0, typechecked against its exported `ExtensionAPI` types.

## Codex

`codex exec` 0.164.0 **silently skips** `.codex/hooks.json` in a project that is not trusted. In the probe, only the already-trusted global `Stop` hook ran, and no error or warning was printed. Consequences:

- Installation must tell users to trust hooks through Codex's own review flow. Coach must never write `trusted_hash` itself or inject hooks with `-c`.
- `coach doctor` must report "registered but no events received" for Codex, so that a silently skipped hook is visible.

The Codex adapter is built from the **documented** input structs in the pinned `rust-v0.164.0` `codex-rs/hooks/src/schema.rs`. Once the user had trusted the hooks, a live `codex exec` delivered `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop` and `SessionEnd` with exactly these keys. None was a parse error or an unknown kind. `PermissionRequest`, the subagent events, `Interrupt` and the compaction events have not been observed live:

- Every event has `session_id`, `cwd`, `hook_event_name` and a nullable `transcript_path`. Turn-scoped events add `turn_id`, `model` and `permission_mode`.
- Tool events (`PreToolUse`, `PostToolUse`) carry `tool_name`, `tool_input` and `tool_use_id`, plus optional `agent_id`/`agent_type`. `PermissionRequest` has `tool_name` and `tool_input` but **no `tool_use_id`**.
- `PostToolUse.tool_response` is an untyped JSON value. There is no failure event.
- `SessionEnd.reason` has the single documented value `other`. `SessionStart.source` is a string.
- Other inputs: `SubagentStart`/`SubagentStop` (required `agent_id`/`agent_type`), `Stop`, `Interrupt`, `PreCompact`/`PostCompact` (`trigger`) and `UserPromptSubmit` (`prompt`).

## Hook delivery mode (Claude Code 2.1.311)

- `"async": true` on a command hook is accepted, and events arrive with the same owner (`$!`) and parent.
- **With `async: true`, `Stop` was never delivered** in two `-p` runs; with sync it was delivered both times. `SessionEnd` was still delivered async.
- Coach therefore registers `Stop` and `SubagentStop` synchronously and every other event async.
- Async hooks take Coach's process startup off the agent's critical path for most events. Measured hook cost: see the ledger and `docs/validation.md`.
- In `-p` mode, a settings file that fails validation is silently ignored. "Zero events" can therefore mean a rejected configuration; `coach doctor` must say so.

## Normalization rules

The envelope's `payload` is `{source_event, raw, …interpretation}`. `raw` is the sanitized source payload. The interpretation fields record what Coach concluded and why.

| Source event | Kind | Notes |
|---|---|---|
| SessionStart / SessionEnd | session_start / session_end | Conversation lifecycle, not process lifecycle. |
| UserPromptSubmit | prompt | Claude `prompt_id` or Codex `turn_id` becomes `turn_id`. |
| PreToolUse | tool_start | `tool_use_id` becomes `tool_call_id`. A shell segment that itself runs `gh pr merge`, `git merge` or `gh pr review --approve` also emits `wrap_up_candidate` with `basis=command_text, remote_state=unverified`. |
| PostToolUse | tool_result | Claude: `outcome=success`, because failures have their own event. Codex: `error`/`success` only from an explicit `exit_code` or `success` field, otherwise `unknown`. |
| PostToolUseFailure (Claude) | tool_failure | `is_interrupt` distinguishes `interrupted` from `error`. |
| PermissionRequest | approval_request | No `tool_call_id` (`tool_call_link=approximate`). No outcome or wait duration is ever recorded. A denied call produces no result event, and none is synthesized. |
| Notification (Claude) | notification | `permission_prompt` is not a wait start or end. |
| Stop | idle | Never completion or wrap-up. |
| SubagentStart / SubagentStop | subagent_start / subagent_stop | `agent_id` from the payload. |
| Interrupt / Pre/PostCompact (Codex) | interrupt / compaction | |
| anything else | unknown | `capture_status.unknown_kind`; source name kept in `source_event`. |

Additional rules:

- **Event IDs and deduplication.** Tool events use `source:session:event:tool_use_id`, so a duplicated hook delivery is stored once. Every other event gets a unique per-delivery ID. Two `Stop`s in one prompt are real (async subagent re-entry) and are never collapsed.
- **Parent agent.** Hook payloads never name a parent. `parent_agent_id` comes only from an explicit launcher's `COACH_PARENT_AGENT_ID`.
- **Bounding.** The hook drains all of stdin (parsing at most 16 MiB) and cuts fields at 64 KiB with truncation notes. If a frame still exceeds 512 KiB, it replaces `raw` with its key names and keeps the identity fields. The event is never dropped.

## Linux coverage

There has been no live Linux harness run. Coverage:

- Process identity from `/proc/<pid>/stat` field 22 (start time in clock ticks since boot), via `sysinfo`, compared with PID.
- CI on Ubuntu runs the owner, liveness and `SO_PEERCRED` tests, including a synthetic hook chain (`sh -c` → probe) that checks shells are skipped.
- README claims for Linux are limited to "unit-tested". See [validation.md](validation.md).
