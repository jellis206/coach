# Validation record

Results for the first release (v0.1.0), recorded 2026-10-27. All measurements used synthetic sessions and an isolated `COACH_STATE_DIR`. Raw output and private session evidence stay on the workstation and are not in this repository.

## Tested versions

| Component | Version | How it was tested |
| --- | --- | --- |
| macOS | 27.0.1, arm64 | everything below |
| Linux | Docker `rust:latest` (Rust 1.99) | `cargo fmt`, `clippy`, `cargo test` (incl. `SO_PEERCRED`, `/proc` start time) and the Python tests. No live harness run |
| Rust | 1.98.1 locally; minimum 1.95 (`sysinfo` 0.39) | build and tests |
| Claude Code | 2.1.311 to 2.1.312 | live headless and interactive sessions |
| Codex | 0.164.0 | live `codex exec` after the user trusted the hooks |
| Pi | 1.4.0 | live `-p` sessions and a real flock worker and validator |
| Bun | 1.4.4 | Pi adapter tests and `tsc --noEmit` |
| Python | 3.15.1 (minimum 3.11) | configurator and dotfiles setup tests |

## Checks

| Command | Result |
| --- | --- |
| `cargo fmt --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo test` | 83 passed, 0 failed (lifecycle 28, adapters 21, debrief 21, store 13) |
| `python3 -m unittest discover -s tests -p 'test_*.py'` | 33 passed |
| `cd adapters/pi && bun test && bunx tsc --noEmit` | 11 passed, typecheck clean |
| dotfiles `python3 scripts/test-setup-coach.py` | 6 passed |
| dotfiles `bun test ./.pi/agent/extensions/flock` | 4 passed |

The lifecycle suite covers the crash, restart, multiple-root and grace-period race cases. It was rerun for this record.

The table shows the counts after the final-review fixes, run with `--test-threads=4`. During that run, every process start on the workstation took about 0.26 s, a machine-wide effect also seen with `/bin/ls`. At full parallelism that slowed daemon startup past the tests' 3 s waits, before and after the fixes. CI runs the suite at default parallelism on macOS and Ubuntu.

## Overhead

macOS arm64, release build, measured from a Python driver as wall time for fork, exec and exit of one synthetic Claude `PostToolUse` hook against a running daemon (200 runs each):

| Measure | p50 | p95 | max |
| --- | --- | --- | --- |
| `/usr/bin/true` (process floor) | 2.16 ms | 2.66 ms | 3.37 ms |
| `coach --version` (binary startup) | 3.67 ms | 4.21 ms | 5.13 ms |
| `coach hook claude` (warm) | 4.49 ms | 5.03 ms | 5.79 ms |
| 8 concurrent writers × 100 hooks | 6.76 ms | 7.92 ms | 10.14 ms |

- **Target missed.** The design asked for a p95 under 5 ms. Warm hooks measured 5.03 ms in this run and 5.2–5.7 ms in an earlier one. About 4.2 ms of that is starting any process plus the `coach` binary. The hook's own work (read stdin, bound, send one frame) adds about 0.8 ms. Hooks are asynchronous in Claude Code except Stop and SubagentStop, so most of this time is not on the agent's path.
- **Cold start.** The first hook after a fresh build took 144 ms, including daemon spawn and the macOS first-launch check of the new binary. An earlier run measured 18 ms for a cold start of an already-launched binary.
- **Load.** The 800-hook burst finished in 0.7 s wall time: 1001 events received and 1001 written, with 0 dropped, 0 write errors and 0 recorded losses.
- **Daemon footprint.** RSS was 13.8 MB after the burst. CPU time did not advance over 3 s idle (0.21 s before and after).

## Live runs

All live runs used a scratch directory, synthetic prompts and normal approval settings.

- **Onboarding checklist** in an isolated home: every README step gave its expected result. The parent recorded 10 events, the headless child was linked, a `failure_then_success` finding for `Bash:ls` appeared and was dismissed, the daemon exited after the session closed, and `remove` restored the settings and kept the evidence. The approval-prompt step was not re-exercised, because the test command did not prompt under the user's settings.
- **Two Claude Code roots.** The second root joined the first (2 leases). 14 events, 0 losses, 0 unclean exits, and the daemon exited after the last root closed.
- **Pi with flock.** One root session dispatched a real worker and validator. All 3 sessions and 18 events linked to the root, and each child recorded its own agent id, the parent, the run id and its role. 0 losses, and the daemon exited.
- **Codex.** After the user trusted the hooks, one `codex exec` recorded `session_start`, `prompt`, `tool_start`, `tool_result`, `idle` and `session_end`. All six were attributed to the Codex process, with none unknown.
- **Wrap-up debrief.** A real session debrief produced 3 candidates. The user accepted the repeated-approval finding with a reference and dismissed the other two. A new debrief read the decisions back.

## Known gaps

- Hook p95 is just above the 5 ms target. See [Overhead](#overhead).
- There has been no live Linux harness run; Linux is unit-tested only.
- Pi UI-wait pairing is unit-tested only. No live TUI prompt was triggered.
- Codex `PermissionRequest` and interactive TUI sessions were not exercised live.
- Coach does not see killed (SIGKILL) sessions end directly. It detects them by liveness checks.
