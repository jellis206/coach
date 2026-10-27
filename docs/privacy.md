# Privacy and data handling

Coach captures detailed evidence by default because the point is to see what actually happened. This page states exactly what is kept, where it is kept, and what leaves the machine.

## What is captured

- Prompts, tool arguments and bounded tool output and error text, as delivered by Claude Code and Codex hooks and by Pi extension events.
- Lifecycle signals: session start/end, idle, approval requests, subagent start/stop, UI prompt waits (Pi), and candidate wrap-up commands.
- Process identity for liveness (PID plus start time) and session, agent, turn and tool-call identifiers.

Coach does not intentionally read credential stores, provider request headers, environment dumps, hidden reasoning or historical transcripts. It never crawls files.

## Bounds and redaction

- Each text field is cut at 64 KiB. The cut is recorded with the field's JSON path and original size.
- Wire frames are capped at 512 KiB. An oversized event keeps its identity fields, and its raw values are replaced by their key names.
- Redaction is **best effort**. It masks:
  - values under credential-like keys (`password`, `token`, `api_key`, `authorization`, `cookie`, `secret`, …);
  - common token shapes in free text: bearer tokens, `sk-…`, GitHub and Slack tokens, AWS access key IDs and PEM private-key headers.

  Free text can still contain secrets. Treat all evidence as sensitive.

## Where it lives

- **Location.** Data lives in one directory per user per workstation:
  - macOS: `~/Library/Application Support/Coach`.
  - Linux: `$XDG_STATE_HOME/coach` or `~/.local/state/coach`.
  - `COACH_STATE_DIR` overrides both.
- **Permissions.** The directory is mode 0700; the database and its WAL/SHM files are mode 0600. Coach refuses a directory that is group/other accessible, owned by someone else, a symlink, or inside a Git worktree.
- **At rest.** Data is unencrypted application data, protected by OS permissions and your disk encryption. Local backups (Time Machine, snapshots) may copy it.

## What leaves the machine

- **Coach itself sends nothing.** The daemon and CLI make no network calls: no telemetry, no sync, no background model analysis. Evidence is never written to a Git checkout or dotfiles repository by Coach.
- **Debriefs use your agent's model.** When you run a debrief with your agent, whatever the agent reads goes to that harness's configured model provider, like any other content in the conversation. A local database does not make the model local. Bring in only what the review needs.
- **Shared improvements carry no evidence.** Improvements you approve for shared places (public repos, shared skills, dotfiles) must be general rules with examples rewritten. They must not contain captured evidence.
- **Workstations stay separate.** Each workstation is its own evidence domain; Coach has no mechanism to combine them.

## Retention and removal

- **Expiry.** Detailed evidence expires after 30 days. The daemon prunes it whenever it starts.
- **Manual purge.** `coach purge --before YYYY-MM-DD --yes` deletes earlier evidence now.
- **What persists.** Decisions (`coach finding accept|dismiss`) keep only the finding id, Coach's generated title, evidence ids and your reference. A debrief reports evidence that a decision refers to but that has been purged.
- **Not secure erasure.** Purging removes rows, but SQLite pages, WAL history, SSD remapping and backups may retain old bytes. Delete the state directory to remove everything Coach stored.

## Commands and what they print

| Command | Prints captured content? |
|---|---|
| `coach doctor` | No. Counts, versions, paths and health only. |
| `coach sessions` | No. Session ids, sources, counts and times. |
| `coach debrief` | Titles built from tool and program names (for example `Bash:rg`), counts and event ids. No arguments or output. |
| `coach evidence <id>` | **Yes.** The full stored event. |
| `coach export --session <id> --output <file>` | **Yes.** It writes a new 0600 file and refuses existing files and Git worktrees. |
