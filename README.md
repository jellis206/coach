# Coach

Coach is a private observer for Claude Code, Codex and Pi. It runs on your workstation and records where an agent struggled: failed commands and the retry that worked, repeated approval prompts, repeated workarounds and measured waits. When you wrap up a piece of work, you and the agent review that evidence in a **debrief** and decide on durable fixes, such as a skill, a hook or an instruction.

- Evidence stays on the machine, in a private SQLite database outside every Git repository. Coach sends no telemetry. See [docs/privacy.md](docs/privacy.md).
- Hooks fail open. A Coach problem never blocks, slows down or changes a tool call, prompt or approval.
- A debrief shows the evidence and the interpretation separately. You accept or dismiss each finding.

## Point your agent here

Paste this request into Claude Code, Codex or Pi on the machine you want to set up:

> Set up Coach from this repository by following the "Setup walkthrough" and "Onboarding checklist" in its README. Ask me for consent before registering any hooks. Do not change my approval, permission or sandbox settings. Do not trust Codex hooks on my behalf; tell me when it is my turn to review and trust them. Stop and report if any step's result differs from what the README says to expect.

The rest of this section is written for that agent.

### Setup walkthrough

1. **Prerequisites.** You need macOS or Linux, Rust 1.95 or newer (`cargo --version`), Python 3.11 or newer for the configurator, and at least one of Claude Code, Codex or Pi. Done when `cargo` and `python3` both run.

2. **Consent.** Show the user [docs/privacy.md](docs/privacy.md) and summarise it: Coach records hook payloads (prompts, tool inputs and outputs, bounded and redacted) and keeps them for 30 days. Done when the user agrees. Without agreement, stop here.

3. **Install the binary.** Build from source in this checkout:

   ```sh
   cargo install --locked --path .
   command -v coach && coach --version
   ```

   Or use a release from [GitHub Releases](https://github.com/jellis206/coach/releases), which has archives for aarch64-apple-darwin and x86_64-unknown-linux-gnu plus a `SHA256SUMS` file. Download the archive and `SHA256SUMS`, run `shasum -a 256 -c SHA256SUMS --ignore-missing`, and use the binary only if the check passes. Never pipe a download into a shell. Each archive also contains the matching `scripts/configure.py`, Pi adapter and skill, so run the next steps from the extracted directory. Done when `coach --version` prints a version.

4. **Preview the registrations.**

   ```sh
   python3 scripts/configure.py install --harness all --dry-run
   ```

   The configurator adds only entries it owns, which are hook commands exactly equal to `<path>/coach hook claude|codex` and a Pi extension directory carrying a `.coach-managed` marker. Existing hooks, including safety hooks, stay as they are. It skips harnesses that are not installed. It refuses and explains when:

   - a settings file is malformed;
   - the config is a symlink, or lives inside a Git checkout such as dotfiles. Register Coach through that repository's own tooling instead. If the user explicitly wants this script to edit the checkout, pass `--allow-managed` and have the user commit the change;
   - a `coach` Pi extension exists that Coach did not install.

   Show the user the dry-run output. Done when they approve it.

5. **Register.** Run the same command without `--dry-run`. Before each change it writes a `*.coach-backup-<timestamp>` copy next to the file, and it writes atomically.

6. **Restart or reload each harness.**

   - **Claude Code:** start a new session. `/hooks` lists the Coach hooks. Stop and SubagentStop run synchronously because Claude Code never delivers them asynchronously; every other hook runs asynchronously.
   - **Codex:** Coach needs `features.hooks = true` in `~/.codex/config.toml`. The configurator reports this setting but never edits it. Codex silently skips hooks it has not trusted. Ask the user to start `codex`, review the Coach hooks when Codex offers them (or open `/hooks`) and trust them. Never write trust entries yourself.
   - **Pi:** run `/reload` in open sessions, or start a new one. The extension finds `coach` on `PATH` or through `COACH_BIN=/absolute/path/to/coach`.

   Done when `coach doctor --json | jq .registrations` shows `"registered_no_events"` for every installed harness. It changes to `"registered"` once that harness has delivered events (onboarding step 2).

7. **Install the debrief skill.** Copy `skills/coach-debrief/` into each harness's skills directory: `~/.claude/skills/` for Claude Code, and `~/.agents/skills/` for Codex and Pi. Pi also gets the `/coach-debrief` command from its extension.

8. **Run the onboarding checklist** below.

### Custom launchers and subagents

Coach links a child session to its parent only when it is told about the relationship:

- the harness reports it, as Claude Code does for its own subagents, which share the parent session;
- or a launcher sets environment variables on the child process.

Coach does not discover arbitrary subagents.

| Variable | Set by | Meaning |
| --- | --- | --- |
| `COACH_PARENT_AGENT_ID` | your launcher | the parent's session id (`$CLAUDE_CODE_SESSION_ID` in Claude Code, `$PI_SESSION_ID` in Pi's bash tool), or its agent id |
| `COACH_AGENT_ID` | the Pi extension, in Pi's own process | this agent's id (`pi:<session id>`), for launchers running inside Pi such as extensions. Pi's bash tool does not see it; use `$PI_SESSION_ID` there |
| `COACH_RUN_ID` | your launcher | a dispatcher run or batch id (recorded by Pi) |
| `COACH_AGENT_ROLE` | your launcher | for example `worker` or `validator` (recorded by Pi) |

A debrief of the parent session includes its known descendants.

These variables are inherited by everything the child starts. A launcher should set all of them for each child it starts, or remove them. Otherwise a grandchild is recorded under the original parent rather than the child that started it.

### Day to day

- **Debrief:** `coach debrief --session <id>` shows the findings, and `coach finding accept|dismiss <finding-id> [--reference <where the fix lives>]` records your decision on each. The skill walks through this at wrap-up, meaning before a merge or after an approved review, never on an idle turn.
- **Other commands:** `coach sessions` lists recorded sessions, `coach evidence <event-id>` prints one event in full (private content), and `coach export --session <id> --output <file>` writes a private JSONL file outside Git.
- **Retention:** detailed evidence older than 30 days is pruned automatically when the daemon starts. To delete sooner, run `coach purge --before YYYY-MM-DD --yes`.
- **Update:** pull, run `cargo install --locked --path .` again, then re-run the configurator so the Pi extension copy matches the binary. The configurator is idempotent.
- **Remove:** run `python3 scripts/configure.py remove --harness all`. This deletes only Coach-owned entries and the Coach Pi extension. Evidence stays until you purge it or delete the state directory reported by `coach doctor`.

### Coverage limits

`coach doctor --json` lists these under `coverage_limits`:

- **Codex trust.** Codex skips untrusted hooks without saying so. `registered_no_events` means the hooks are configured but nothing has arrived yet. `registered` means events from that harness are in the store.
- **Killed sessions.** A harness killed with SIGKILL sends no session end. The daemon notices through liveness checks.
- **Unattributed events.** Hook events whose owner process cannot be verified are recorded with an `unknown` owner, and the debrief's coverage line counts them. Examples are a hook run by hand from a terminal, or a script under a harness that runs `coach hook`. Such events never keep the daemon alive. Claude Code's asynchronous SessionEnd hook usually runs after Claude has exited, so expect one unknown-owner event per Claude session. It still carries the session id.
- **Inherited lineage.** See [Custom launchers and subagents](#custom-launchers-and-subagents).
- **Platforms.** Linux ownership and peer credentials are covered by tests. Live harness checks were run on macOS with Claude Code 2.1.312, Codex 0.164.0 and Pi 1.4.0. Versions, measurements and gaps are in [docs/validation.md](docs/validation.md), and harness contracts in [docs/integrations.md](docs/integrations.md).
- **Overhead.** A warm hook takes about 4.5 ms at the median and 5 ms at the 95th percentile, mostly process startup. Claude Code runs every Coach hook except Stop and SubagentStop asynchronously.

## Onboarding checklist

Run these checks in a scratch directory outside every repository, for example `mkdir -p ~/coach-onboarding && cd ~/coach-onboarding`. Use only synthetic content. None of the steps needs relaxed approvals. When the harness asks for permission, the user answers normally.

1. **Baseline.** Run `coach doctor --json`. Expect:
   - `"ok": true`;
   - `"daemon": {"running": false}` while no harness session is open;
   - `registrations` reports `"registered_no_events"` for each harness you set up and `"harness_absent"` for the others;
   - `store.recorded_losses` and `store.unclean_daemon_exits` are `0`.

2. **Live capture.** Start a fresh session in the scratch directory and ask the agent to run `echo coach-onboarding`. While the session is open, `coach doctor --json` shows:
   - `daemon.running` is `true`;
   - `daemon.leases` is at least `1`;
   - `store.by_source.<claude|codex|pi>` is greater than `0`;
   - that harness's registration is now `"registered"`.

   If it still says `"registered_no_events"`, the hooks were not loaded. For Codex, that means they are untrusted or `features.hooks` is off. For Claude Code, the settings file may have failed validation.

3. **Intentional failure.** Ask the agent to run `ls /coach-onboarding-does-not-exist`, then `ls .`. That gives one failure followed by a working command.

4. **Prompt and wait.** Ask for one command that triggers a permission prompt in the current settings, for example `touch coach-onboarding.txt` in Claude Code's default mode. Wait a few seconds, then approve or deny it as you normally would. Pi measures UI waits directly. Claude Code and Codex record the approval request without claiming a wait time.

5. **Headless child.** From the same session, launch one child that names its parent:
   - **Claude Code:** `COACH_PARENT_AGENT_ID=$CLAUDE_CODE_SESSION_ID claude -p "run: ls /coach-onboarding-child-missing"`
   - **Pi:** `COACH_PARENT_AGENT_ID=$PI_SESSION_ID pi -p "run: ls /coach-onboarding-child-missing"`

   The child keeps its normal permission settings.

6. **Debrief.** Run `coach sessions`, then `coach debrief --session <parent-session-id> --json`. Expect:
   - `included_sessions` lists the parent and the headless child;
   - the coverage counts are present;
   - a `failure_then_success` finding for `Bash:ls`, citing the failed and the working `ls` (Pi names the tool `bash`);
   - the approval appears only as a `repeated_approval` finding once it recurs, because one prompt on its own is not a pattern.

   Review the finding with the user, then record a decision with `coach finding dismiss <id>`, because this was a demo.

7. **Close.** Exit the sessions. Within about 10 seconds, `coach doctor --json` shows `"daemon": {"running": false}`.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
python3 -m unittest discover -s tests -p 'test_*.py'
(cd adapters/pi && bun install --frozen-lockfile && bun test && bunx tsc --noEmit)
```

The design and plan are in `docs/superpowers/`. Harness contracts are in [docs/integrations.md](docs/integrations.md).

## License

MIT
