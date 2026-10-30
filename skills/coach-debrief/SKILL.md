---
name: coach-debrief
description: Wrap-up review of this session's recorded agent friction with Coach. Use when preparing to merge, after a PR is approved, or when the user asks for a debrief, a retrospective, or what to improve about how the agent worked.
---

# Coach debrief

Coach records friction locally while you work: failed tool calls and the retries that found the working usage, repeated approval requests, shell reads that bypass a dedicated read or search tool, measured UI waits. At **wrap-up** you and the user review that evidence together and decide on durable improvements. Wrap-up means preparing to merge, an approved PR, or the user asking. An ordinary idle turn or a Stop is not wrap-up.

## Steps

1. **Find the session.**
   - Claude Code: `$CLAUDE_CODE_SESSION_ID`.
   - Codex and Pi: the session id from the harness. If you don't have it, use `coach sessions` and confirm the choice with the user.
   - To review more than this session, ask the user which sessions to add. Never add sessions just because they share a directory.
   - Done when you have one or more session ids the user agrees with.

2. **Run the debrief:** `coach debrief --session <id> [--session <id> ...]`. Add `--json` if you want to parse the result.
   - Read the coverage line before the findings: unknown-owner, unparsed or truncated events mean the picture is partial.
   - `No change since the last debrief` means there is nothing new to raise; say so and stop.
   - Done when you have read every finding.

3. **Tell the user what each finding is evidence of.** Keep three things apart:
   - **Evidence:** the counts and event ids.
   - **Certainty:** the line Coach prints.
   - **Your interpretation.**

   An approval request proves only that a request happened. The user's decision and the time they spent are not recorded, so report neither. A repeated approval may be a correct safety check. When a finding needs detail, use `coach evidence <event-id>`, and quote only what the discussion needs.

4. **Propose one general improvement per finding the user wants to act on.** Examples: a skill or doc line naming the working invocation, a dedicated tool, or a project instruction. Permission changes are proposals for the user to review and apply. Leave approvals and safety hooks as they are unless the user explicitly changes them.

5. **Make approved changes through the normal workflow**, with review and tests. Before an edit lands in a shared or public place (a public repo, shared skills, dotfiles), **sanitize** it:
   - Write the general rule, not the captured example.
   - Remove paths, hostnames, customer or business names, tokens and command output.

6. **Record each decision:** `coach finding accept <finding-id> --reference <where the improvement lives>`, or `coach finding dismiss <finding-id>`. Decisions keep references and Coach's own summary, never raw evidence. A later debrief shows if the pattern recurs.

Done when every finding the user reviewed has an accept or dismiss decision recorded.

## Privacy

- **Local.** Evidence lives in Coach's database on this workstation. It is never committed, synced or uploaded by Coach.
- **Model provider.** Whatever you read into this conversation goes to this harness's configured model provider. A local database does not make the model local. Bring in only the evidence the review needs.
- **Redaction.** Redaction is best effort; captured text can still contain secrets. Treat `coach evidence` and `coach export` output as private.
