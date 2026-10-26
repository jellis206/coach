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
| Codex 0.164.0 | Pending live probe. | Pending. | Not verified. See [Codex](#codex). |

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
- `ui_prompt_start`/`ui_prompt_end` cannot fire headless. Their TUI behaviour, `/reload` and session switching are verified in Task 5.

## Codex

`codex exec` 0.164.0 **silently skips** `.codex/hooks.json` in a project that is not trusted. In the probe, only the already-trusted global `Stop` hook ran, and no error or warning was printed. Consequences:

- Installation must tell users to trust hooks through Codex's own review flow. Coach must never write `trusted_hash` itself or inject hooks with `-c`.
- `coach doctor` must report "registered but no events received" for Codex, so that a silently skipped hook is visible.

Live ownership and payload shapes for Codex are pending a user-approved trust of the scratch probe project.

## Linux coverage

There has been no live Linux run yet. Planned coverage:

- Process identity from `/proc/<pid>/stat` field 22 (start time in clock ticks since boot), via `sysinfo`, compared with PID.
- CI on Ubuntu runs the owner/liveness unit tests with a synthetic hook chain (`sh -c` → probe) to check that shells are skipped.
- A live Linux harness probe is a Task 9 item. Until it runs, README claims for Linux are limited to "unit-tested".
