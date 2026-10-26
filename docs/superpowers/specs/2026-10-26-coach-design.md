# Coach — local agent improvement coach

Status: approved in Plannotator, with README onboarding guidance incorporated below. No product code or repository has been created.

## Intent

Coach observes friction in Claude Code, Codex CLI, and Pi, including instrumented subagents. It quietly collects evidence during work, then helps the user and their current agent review recurring problems at wrap-up and approve durable improvements: skills, instructions, tool documentation, configuration, or code fixes.

The primary examples are repeated approvals for routine navigation/file reading, awkward shell workarounds for reading file slices, and failed tool invocations followed by retries that discover the correct usage. Failures and approval prompts are evidence to investigate, not automatically mistakes or reasons to weaken security.

Success means fewer recurring avoidable interruptions and repeated discoveries across working sessions, without Coach becoming another interruption or a dependency for using an agent.

## Confirmed requirements

- Name: **Coach**, with executable `coach` subject to checking local command conflicts.
- Initial scope: coaching LLM agents, not their human user.
- Capture prompts, tool arguments, and bounded tool output/error excerpts by default.
- All collected data remains local and private to its workstation. No telemetry, synchronization, cloud database, or automatic remote model analysis.
- The SQLite database is never committed to Git or stored in the public project or dotfiles checkout. The same applies to evidence exports, transcripts, generated debriefs, runtime state, and logs containing captured content.
- Workstations are separate evidence domains. Personal and business evidence must not be mixed across machines. Approved general improvements can travel through code, skills, or dotfiles; the underlying evidence does not.
- Start one shared local daemon when the first supported harness starts; stop it when the last supported harness process exits, including headless children. Handle crashes rather than depending only on clean shutdown callbacks.
- Observe subagents and their relationships where the harness/launcher can expose them. Report unknown relationships honestly.
- Keep work quiet during execution. Surface improvement opportunities at wrap-up, particularly preparing to merge or after PR approval.
- Human approval precedes permanent improvements. No silent permission changes or skill rewrites.
- Publish a public GitHub project and separately integrate installation/configuration into the user's dotfiles for macOS and Linux.

## Non-goals and anti-goals

No historical GitHub ingestion, career ladders, human performance scoring, user coaching, cross-machine evidence aggregation, team server, dashboard, filesystem mount, custom harness replacement, or automatic background paid LLM. Future human coaching requires a separately approved design and data access.

Never change existing agent approvals, weaken safety hooks, block a tool on Coach availability, treat normal Stop/idle events as proof a task is complete, or claim complete capture when an adapter lacks visibility. Never copy private evidence into a public issue, fixture, commit, or improvement file automatically.

## Proposed technical choices (builder recommendations, not prior user decisions)

### Rust core, small native adapters

Use a Rust daemon and CLI. Rust is recommended for its mature SQLite, serialization, local IPC, and process-management ecosystem; Zig is not required for the workload. Use established crates rather than inventing a database or protocol framework. Pi still needs a small TypeScript extension to consume its native events.

One public project contains the core binary, adapter assets, synthetic fixtures, tests, install documentation, shared debrief skill, and CI. Dotfiles contain machine setup and registration, not a fork of core logic.

### Local storage instead of TigerFS

Use SQLite on local disk with WAL, a single daemon writer, and bounded batched transactions. CLI readers query the database. JSONL and Markdown are explicit exports, not a second authoritative event store.

TigerFS presents PostgreSQL through FUSE/NFS and adds a database server and mount layer. It is not needed for this local append-heavy observer. Do not install it for Coach.

Use machine-local state directories: `$XDG_STATE_HOME/coach` or `~/.local/state/coach` on Linux; `~/Library/Application Support/Coach` on macOS. Never resolve data storage underneath a Git checkout or the dotfiles symlink tree. Runtime socket/lock directories must be private and local; account for Unix socket path limits and macOS lacking XDG_RUNTIME_DIR. Directories mode 0700, files mode 0600. No local HTTP server by default.

Proposed retention default: retain detailed evidence for 30 days, with explicit local retention configuration and purge commands. Accepted improvement metadata and redacted summaries may persist until explicitly removed, but remain machine-local and private. This 30-day default is a builder proposal to review, not a confirmed user requirement. Data is unencrypted application data protected by OS permissions and the user's disk encryption; local backups may retain it. Purge does not promise secure erasure from SSDs, WAL history, or backups.

### Capture and evidence model

Store versioned normalized events with source harness/version, workstation-local process-instance ID, conversation/session ID, agent ID and parent agent ID when known, turn/tool-call IDs, event kind, timestamps, source provenance, payload, and explicit capture/parse status. Distinguish nested tool calls from subagents and fork lineage from spawned-worker lineage.

Capture prompt text, bounded tool arguments/results, errors, approval-request signals, UI wait start/end when exposed, tool completion, retries, lifecycle changes, and explicit wrap-up markers. Never ingest credential stores, provider headers, environment dumps, or hidden reasoning intentionally. Redact obvious credential fields and common token patterns best-effort, while documenting that free text can still contain secrets. Do not crawl arbitrary files or import all historical transcripts by default.

Proposed payload limit: 64 KiB per textual prompt/argument/result field, with explicit truncation metadata. Preserve source references when available without assuming the source will exist forever. Oversized/unknown payloads must not crash ingestion. Validation covers size, schema version, malformed input, identity, and bounded IPC frames. Store event occurrence and receipt times separately; process-local sequence numbers support ordering without assuming clocks are perfect.

Findings reference immutable local event IDs and show evidence versus interpretation. Repeated events are grouped into a sequence or recurring pattern, not counted as independent mistakes by default. Missing outcomes and approximate correlation must be labeled. Do not infer time wasted or exact wait duration from permission-request counts alone.

### Daemon lifecycle and delivery

Separate process ownership from session/conversation ownership. A clear, fork, runtime reload, or session switch can replace the conversation without ending the harness process.

At harness startup, adapters establish a process instance and start/connect to Coach. Startup uses exclusive locking to prevent duplicate daemons. Track root and child process identities with protection against PID reuse. Pi can maintain a persistent connection and lightweight liveness signal; command-hook adapters require process identity verification by the daemon because the short-lived hook executable is not the owning harness. Prototype and verify owner identification for Claude/Codex on macOS/Linux before claiming this requirement is solved. If reliable ownership cannot be obtained natively, present the minimal launcher fallback for review rather than silently introducing wrappers.

SessionEnd/shutdown helps release references but is not sufficient for SIGKILL. Check owner liveness on a bounded interval. Last confirmed harness exit starts a short grace period, drains queued events, closes the database, and exits. New connections during grace cancel shutdown. Stale queued events do not indefinitely resurrect dead clients. Recovery and client reconnect must work after daemon crashes. No launch-at-login service or permanent idle watcher in the minimum design.

Adapter work is bounded enqueue/transport only. Database writes, pattern grouping, and debrief generation occur out of process. Hook output must not contain permission decisions, blocking exit codes, or model-context injection during normal collection. Use supported background hooks where possible, with a tiny fail-open sender when unavailable; measure startup overhead instead of promising zero latency.

Proposed overload policy: bounded memory queue, dropped-event counters, and visible coverage gaps in diagnostics/debriefs. Do not add an unbounded spool or hang agents to guarantee capture. Bounded best-effort collection can lose events during outages, abrupt shutdown, saturation, or disk-full. Test these paths. Prefer retaining error/approval/lifecycle events over noisy progress updates; do not subscribe to token streaming in v1.

### Harness adapters and coverage

**Claude Code:** settings-based observer hooks for supported session, tool start/completion/failure, permission, prompt, and subagent events. Merge registrations beside existing safety and Herdr hooks. Validate CLI-specific contracts and background restrictions against installed versions, not only SDK docs.

**Codex:** native hooks, not legacy notify-only assumptions. Local version inspected: 0.164.0. Its pinned schema exposes session, permission, tool, turn and subagent fields; tool_response needs adapter-specific failure interpretation and transcript paths can be null. There is no assumed Claude-style dedicated failure event. Preserve hook trust; installer must explain any approval needed and never bypass it. Report supported versions and coverage.

**Pi:** extension starts resources on session_start and releases them idempotently on session_shutdown; observe tool events, input/prompt events, UI prompt start/end, and settled notifications. Do not return mutations from observer hooks. Collection works in TUI, JSON, print, and RPC modes when extensions are loaded; presentation is mode-aware. Avoid slow awaited handlers.

**User's Pi dispatcher:** workers run `pi --mode json -p --no-session`, so transcript tailing alone misses them. Propagate explicit Coach parent/run identities through the existing launcher and observe worker and validator processes, including repeated worker launches. Keep this local dispatcher change small and separately tested. Other arbitrary launchers cannot be assumed to propagate lineage; use documented environment variables and honest unknown-parent reporting.

Provide `coach doctor` to report registration, versions, daemon/database health, dropped events, and coverage limitations without printing captured private content.

### Debrief workflow

A CLI debrief command plus a shared agent skill is the reliable v1 entry point. The skill tells the active agent to run the local query at genuine wrap-up (pre-merge, reviewed/approved PR, or user request), then review findings with the user. It does not grant new external access.

Observed merge/review-related commands and explicit markers can flag a candidate wrap-up and prepare a debrief, but a command mentioning approval is not proof of remote PR state. Do not add GitHub polling or credential requirements. Pi idle/Claude Stop/Codex Stop can help deliver an already-ready notice but must not emit a new debrief after every ordinary turn. Never block a merge or keep an agent running to force a debrief.

Working episodes are initially explicit session roots plus known descendants. Keep repository/worktree metadata for queries, but do not silently combine unrelated sessions solely because their directories match. Allow the CLI/skill to select additional sessions deliberately for a larger review. Debrief generation tracks the event cutoff so repeated requests do not re-notify unchanged findings and new evidence can produce an updated review.

Start with explainable grouping: repeated command/tool fingerprints and errors, repeated approval patterns, actual prompt waits, and failure-to-success sequences. Surface candidate patterns with evidence, counts, recovery examples, and uncertainty. Do not classify every failure or permission check as waste. No automatic fuzzy semantic judge is required in the background.

The CLI produces a compact local report. The user and current agent interpret the evidence and propose improvements. Any evidence passed into the active agent follows that harness's configured provider; local storage does not imply local-only model inference. No report is automatically sent to an external model by the daemon. The user's explicit debrief interaction authorizes the normal active-agent workflow, not background uploads.

Review each proposed skill/docs/config/code change with ordinary approval and tests. Sanitize examples and check for business/private identifiers before placing an improvement in public Coach or shared dotfiles. Link accepted improvements to findings locally and measure recurrence later; shared improvements contain general rules, not copied raw evidence.

## Installation and distribution

Check executable/repository-name availability before creation. The public GitHub repository is explicitly desired, but create/publish only after design and implementation-plan approval. Default public code license proposal: MIT, subject to review. No credentials in repo setup.

The public README must provide an agent-readable standalone setup walkthrough so another user can point their agent at the project and obtain the same Claude Code, Codex and Pi hooks, headless/subagent instrumentation, and capture verification without this user's dotfiles. Explain prerequisites, installation, hook trust/restarts, privacy and retention, coverage limitations, debrief usage, and safe removal. Launcher-specific parent propagation must be documented as an explicit integration step, not promised automatically for arbitrary subagents.

Dotfiles installation should use a pinned release/version, verify artifacts, merge hook/extension registration idempotently, preserve pre-existing settings, honor Codex trust, and provide removal that does not delete evidence. Use the existing .init.sh, setup-codex.py, and shared-skill reconciliation conventions. Support macOS and Linux; do not claim Windows support.

Keep any Coach data paths out of repository trees, add defensive ignore patterns for SQLite/WAL/SHM files and local exports, and test generated fixtures with synthetic data only. Installation updates core and adapters coherently and leaves runtime content machine-local. No automatic installation during design review.

## Validation and release criteria

1. Synthetic fixtures prove normalization and grouping for each harness, including errors, retries, approval requests, unknown schemas, large payloads, and redaction/truncation.
2. Installed-harness smoke tests demonstrate actual events on the versions in use; document unsupported paths rather than claiming complete coverage.
3. Multi-process tests cover simultaneous startup, multiple roots, headless children, worker/validator lineage, process crashes/PID reuse, daemon restart, session clear/reload, and shutdown grace races.
4. Fault tests cover database busy/disk-full, disconnected sockets, malformed frames, queue overflow, and interrupted writes; agent operations remain usable and gaps are reported.
5. Baseline/treatment measurements report hook and extension enqueue overhead and daemon idle/active resource use. Proposed warm enqueue target: p95 under 5 ms, excluding one-time startup; record cold-start cost and environment separately. If unmet, show results and revise before claiming negligible overhead.
6. Debrief tests show candidate wrap-up does not fire on every idle turn, repeated debriefs do not duplicate notifications, evidence links work, and interpretation does not fabricate wasted time or approval outcomes.
7. Installer tests prove idempotent registration/removal, preserved safety hooks/trust, no runtime data in Git paths, and separate machine-local state.
8. Self-observe a real working session, jointly review one useful finding, and approve or dismiss one improvement without automatic permission changes.

## Research basis and remaining uncertainty

Read-only research notes are kept outside this repository on the authoring workstation. Relevant primary sources:
- https://code.claude.com/docs/en/hooks
- https://code.claude.com/docs/en/agent-sdk/hooks
- https://developers.openai.com/codex/hooks
- https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/hooks/src/schema.rs
- https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/extensions/types.ts
- https://github.com/timescale/tigerfs
- https://www.sqlite.org/wal.html

Direct retrieval of full Claude/Codex hook pages was partly blocked; official search excerpts, Codex pinned source and installed Pi docs informed this design. Exact hook timing, exit semantics, ownership detection and delivery must be verified in the implementation's first integration spike.

Independent builder-check dispatch was unavailable because this session is outside tmux. This draft received a self-check for implementability; independent review remains an implementation-plan review item. Proposed retention, payload caps, timings, license and CLI details are reviewable defaults rather than user commitments.
