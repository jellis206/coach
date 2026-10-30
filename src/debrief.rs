//! Explainable grouping of friction into findings, with evidence references,
//! honest certainty, and idempotent cutoffs. No model calls, no wasted-time math.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::EventEnvelope;
use crate::store::{DecisionKind, Result, Store};

const MAX_EVIDENCE: usize = 20;
const REPEATED_INVOCATION: u64 = 5;
const SUBCOMMAND_CLIS: &[&str] = &[
    "git", "gh", "cargo", "npm", "pnpm", "yarn", "bun", "docker", "kubectl", "go", "pip", "uv",
    "brew", "linear", "codex", "claude", "coach",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    FailureThenSuccess,
    RepeatedFailure,
    RepeatedApproval,
    RepeatedInvocation,
    UiWait,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// Stable across sessions: hash of kind and fingerprint.
    pub id: String,
    pub kind: PatternKind,
    pub title: String,
    pub fingerprint: String,
    pub count: u64,
    pub evidence_ids: Vec<String>,
    pub sessions: Vec<String>,
    pub outcome_certainty: String,
    pub notes: Vec<String>,
    /// Present only for waits Pi actually measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_wait_ms: Option<i64>,
    pub truncated_evidence: bool,
    pub decision: Option<DecisionKind>,
    pub recurred_after_decision: bool,
    /// Evidence referenced by an earlier decision that has since been purged.
    pub missing_evidence: usize,
    /// Newest receipt among all matching events, including those past the evidence cap.
    #[serde(skip)]
    last_received_ms: i64,
}

#[derive(Debug, Default, Serialize)]
pub struct Coverage {
    pub events: usize,
    pub unknown_owner_events: usize,
    pub parse_error_events: usize,
    pub truncated_events: usize,
    pub unknown_kind_events: usize,
    pub sources: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Debrief {
    pub selection: Vec<String>,
    pub included_sessions: Vec<String>,
    /// Receipt time of the newest event considered.
    pub cutoff_ms: i64,
    /// False when an identical debrief was already produced for this selection.
    pub changed: bool,
    pub findings: Vec<Finding>,
    pub coverage: Coverage,
}

#[derive(Serialize, Deserialize)]
struct FindingRef {
    id: String,
    count: u64,
    title: String,
    evidence_ids: Vec<String>,
}

fn selection_key(sessions: &[String]) -> String {
    let mut s = sessions.to_vec();
    s.sort();
    s.dedup();
    s.join(",")
}

/// FNV-1a: stable across builds and platforms.
fn stable_id(kind: PatternKind, fingerprint: &str) -> String {
    let key = format!("{kind:?}|{fingerprint}");
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    });
    format!("f-{hash:016x}")
}

fn raw(e: &EventEnvelope) -> &Value {
    &e.payload["raw"]
}

fn tool_name(e: &EventEnvelope) -> String {
    let r = raw(e);
    r.get("tool_name")
        .or_else(|| r.get("toolName"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string()
}

fn command(e: &EventEnvelope) -> Option<String> {
    let r = raw(e);
    let c = r
        .pointer("/tool_input/command")
        .or_else(|| r.pointer("/args/command"))?;
    match c {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => Some(
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// Shell setup that only prepares for the command the agent actually meant to run.
const SHELL_SETUP: &[&str] = &[
    "cd", "pushd", "popd", "export", "set", "unset", "source", ".", "local", "shopt", "trap",
    "umask", "for", "while", "until", "case", "done", "fi", "esac", "true", "false", ":",
];
/// Keywords that prefix a command inside a compound statement.
const SHELL_PREFIX: &[&str] = &["do", "then", "else", "if", "elif", "!", "time", "{", "("];
/// The harness's own tools: using them often is normal work, not a workaround.
const CORE_TOOLS: &[&str] = &[
    "Edit",
    "MultiEdit",
    "Read",
    "Write",
    "Grep",
    "Glob",
    "LS",
    "Agent",
    "Task",
    "TodoWrite",
    "ToolSearch",
    "apply_patch",
    "read",
    "edit",
    "write",
];

/// Shell programs that stand in for a harness's dedicated read/search tools.
const READ_WORKAROUNDS: &[&str] = &[
    "cat", "head", "tail", "sed", "awk", "grep", "egrep", "rg", "find", "ls", "wc",
];
/// Harnesses that ship dedicated read/search tools; Codex reads through its shell.
const HAS_READ_TOOL: &[&str] = &["claude", "pi"];

/// Top-level simple commands of a shell line, split on `\n ; | &` outside quotes and
/// `$(…)`/backticks, with comments and heredoc bodies dropped. None if quoting is
/// unbalanced, since then nothing past the first word can be trusted.
fn shell_segments(command: &str) -> Option<Vec<String>> {
    let (mut out, mut cur) = (Vec::new(), String::new());
    let (mut single, mut double, mut subst, mut tick) = (false, false, 0usize, false);
    let mut heredocs: Vec<String> = Vec::new();
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        let nested = single || double || subst > 0 || tick;
        match c {
            '\'' if !double && !tick => single = !single,
            '"' if !single && !tick => double = !double,
            '\\' if !single => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                continue;
            }
            '`' if !single => tick = !tick,
            '$' if !single && chars.peek() == Some(&'(') => {
                subst += 1;
                cur.push(c);
                cur.push(chars.next()?);
                continue;
            }
            ')' if !single && subst > 0 => subst -= 1,
            '#' if !nested && cur.chars().last().is_none_or(char::is_whitespace) => {
                while chars.next_if(|&n| n != '\n').is_some() {}
                continue;
            }
            '<' if !nested && chars.peek() == Some(&'<') => {
                chars.next();
                chars.next_if_eq(&'-');
                while chars.next_if(|n| *n == ' ').is_some() {}
                let mut delim = String::new();
                while let Some(n) = chars.next_if(|n| !n.is_whitespace() && !";|&<>".contains(*n)) {
                    delim.push(n);
                }
                heredocs.push(delim.trim_matches(['\'', '"']).to_string());
                continue;
            }
            '\n' | ';' | '|' | '&' if !nested => {
                out.push(std::mem::take(&mut cur));
                if c == '\n' {
                    // Heredoc bodies start on the next line and end at their delimiter.
                    for delim in heredocs.drain(..) {
                        loop {
                            let line: String = chars.by_ref().take_while(|&n| n != '\n').collect();
                            if line.trim() == delim || chars.peek().is_none() {
                                break;
                            }
                        }
                    }
                }
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    (!single && !double && subst == 0 && !tick).then_some(out)
}

/// A word that names a program, not a piece of quoted text or a redirection.
fn is_program(w: &str) -> bool {
    w.starts_with(|c: char| c.is_ascii_alphabetic() || "/._~".contains(c))
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._~+-:".contains(c))
}

/// The first command after shell setup; a command that is only setup stands for itself.
fn main_command(command: &str) -> Vec<String> {
    let Some(segments) = shell_segments(command) else {
        return command
            .split_whitespace()
            .take(1)
            .filter(|w| is_program(w))
            .map(String::from)
            .collect();
    };
    let segments: Vec<Vec<String>> = segments
        .iter()
        .filter_map(|s| {
            let words: Vec<&str> = s.split_whitespace().collect();
            let skip = words
                .iter()
                .take_while(|w| {
                    SHELL_PREFIX.contains(w) || (w.contains('=') && !w.starts_with('-'))
                })
                .count();
            // `f=$(fd x)` runs `fd`, but its arguments are private: skip the whole segment.
            if words[..skip]
                .iter()
                .any(|w| w.contains("$(") || w.contains('`'))
            {
                return None;
            }
            let rest: Vec<String> = words[skip..].iter().map(|w| w.to_string()).collect();
            rest.first().is_some_and(|w| is_program(w)).then_some(rest)
        })
        .collect();
    segments
        .iter()
        .find(|s| !SHELL_SETUP.contains(&s[0].as_str()))
        .or(segments.first())
        .cloned()
        .unwrap_or_default()
}

/// `(retry key, program + subcommand-or-flag)` without arguments that could be paths or
/// content. The retry key is the program, or `git push` for subcommand CLIs, so changing
/// flags is a retry but switching to another subcommand is not.
fn command_keys(command: &str) -> Option<(String, String)> {
    let words = main_command(command);
    let mut words = words.iter().map(String::as_str);
    let program = words.next()?.rsplit('/').next()?.to_string();
    let mut next = words.next();
    // Global options that take a value (`git -C <path>`, `cargo +nightly`) precede the subcommand.
    while let Some(w) = next {
        if program == "git" && ["-C", "-c", "--git-dir", "--work-tree"].contains(&w) {
            words.next();
        } else if !w.starts_with('+') {
            break;
        }
        next = words.next();
    }
    let sub = next.filter(|w| {
        // Only flags, or subcommands of CLIs built around them; plain arguments may be private.
        if !w.starts_with('-') && !SUBCOMMAND_CLIS.contains(&program.as_str()) {
            return false;
        }
        let w = w.trim_start_matches('-');
        !w.is_empty()
            && w.len() <= 24
            && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && w.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
    });
    let full = sub.map_or_else(|| program.clone(), |s| format!("{program} {s}"));
    let retry = if next.is_some_and(|w| !w.starts_with('-')) && sub.is_some() {
        full.clone()
    } else {
        program
    };
    Some((retry, full))
}

/// `(coarse, fine)` fingerprints: tool + program, tool + program + subcommand.
fn fingerprints(e: &EventEnvelope) -> (String, String) {
    let tool = tool_name(e);
    if CORE_TOOLS.contains(&tool.as_str()) {
        return (tool.clone(), tool); // e.g. a patch's `command` is file content
    }
    match command(e).as_deref().and_then(command_keys) {
        Some((p, full)) => (format!("{tool}:{p}"), format!("{tool}:{full}")),
        None => (tool.clone(), tool),
    }
}

/// The program, when this is shell reading or searching in a harness with a tool for it.
fn read_workaround(e: &EventEnvelope) -> Option<String> {
    if !HAS_READ_TOOL.contains(&e.source.as_str()) || CORE_TOOLS.contains(&tool_name(e).as_str()) {
        return None;
    }
    let command = command(e)?;
    let words = main_command(&command);
    // `cat > f`, `sed -i` and `awk … > out` write files; the dedicated tool there is Edit/Write.
    let (program, _) = command_keys(&command)?;
    let writes = words.iter().any(|w| {
        (program == "sed" && (w == "-i" || w.starts_with("-i.")))
            || (w.starts_with('>') && w != ">/dev/null")
    });
    (!writes && READ_WORKAROUNDS.contains(&program.as_str())).then_some(program)
}

/// `Bash:cargo tset` vs `Bash:cargo test`: same program, subcommands within two edits.
fn is_typo_of(failed: &str, worked: &str) -> bool {
    let (Some((fp, fs)), Some((wp, ws))) = (failed.split_once(' '), worked.split_once(' ')) else {
        return false;
    };
    if fp != wp || fs == ws {
        return false;
    }
    let (a, b): (Vec<char>, Vec<char>) = (fs.chars().collect(), ws.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (prev + (ca != cb) as usize)
                .min(row[j] + 1)
                .min(row[j + 1] + 1);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()] <= 2
}

#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Failure,
    Success,
    Unknown,
}

fn outcome(e: &EventEnvelope) -> Outcome {
    match (e.kind.as_str(), e.payload["outcome"].as_str()) {
        ("tool_failure", _) => Outcome::Failure,
        ("tool_result", Some("error")) => Outcome::Failure,
        ("tool_result", Some("success")) => Outcome::Success,
        _ => Outcome::Unknown,
    }
}

#[derive(Default)]
struct Acc {
    count: u64,
    evidence: Vec<String>,
    sessions: Vec<String>,
    truncated: bool,
    wait_ms: i64,
    last_received_ms: i64,
}

impl Acc {
    fn add(&mut self, e: &EventEnvelope, received_ms: i64) {
        self.last_received_ms = self.last_received_ms.max(received_ms);
        if self.evidence.len() < MAX_EVIDENCE {
            self.evidence.push(e.event_id.clone());
        }
        if let Some(s) = &e.session_id
            && !self.sessions.contains(s)
        {
            self.sessions.push(s.clone());
        }
        self.truncated |= !e.capture_status.truncated.is_empty();
    }
}

pub fn debrief(store: &mut Store, sessions: &[String], now_ms: i64) -> Result<Debrief> {
    let included = store.sessions_with_descendants(sessions)?;
    let events = store.events_for(&included)?;
    let cutoff_ms = events.iter().map(|(_, r)| *r).max().unwrap_or(0);

    let mut coverage = Coverage {
        events: events.len(),
        ..Default::default()
    };
    let mut sources = std::collections::BTreeSet::new();
    for (e, _) in &events {
        sources.insert(e.source.clone());
        coverage.unknown_owner_events += (e.process_instance_id == "unknown") as usize;
        coverage.parse_error_events += e.capture_status.parse_error as usize;
        coverage.truncated_events += (!e.capture_status.truncated.is_empty()) as usize;
        coverage.unknown_kind_events += e.capture_status.unknown_kind as usize;
    }
    coverage.sources = sources.into_iter().collect();

    // Per coarse fingerprint: failures awaiting a success, and completed sequences.
    let mut pending: BTreeMap<String, Acc> = BTreeMap::new();
    let mut sequences: BTreeMap<String, Acc> = BTreeMap::new();
    let mut approvals: BTreeMap<String, Acc> = BTreeMap::new();
    // Per tool: all shell reading together, with counts per program for the title.
    let mut invocations: BTreeMap<String, (Acc, BTreeMap<String, u64>)> = BTreeMap::new();
    let mut waits = Acc::default();

    for &(ref e, received) in &events {
        match e.kind.as_str() {
            "tool_failure" | "tool_result" => {
                let (coarse, _) = fingerprints(e);
                match outcome(e) {
                    Outcome::Failure => {
                        let acc = pending.entry(coarse).or_default();
                        acc.count += 1;
                        acc.add(e, received);
                    }
                    Outcome::Success => {
                        let typo = pending.keys().find(|k| is_typo_of(k, &coarse)).cloned();
                        let found = pending.remove(&coarse).or_else(|| pending.remove(&typo?));
                        if let Some(mut acc) = found {
                            acc.add(e, received);
                            let seq = sequences.entry(coarse).or_default();
                            seq.count += acc.count;
                            seq.last_received_ms = seq.last_received_ms.max(acc.last_received_ms);
                            for id in acc.evidence {
                                if seq.evidence.len() < MAX_EVIDENCE {
                                    seq.evidence.push(id);
                                }
                            }
                            for s in acc.sessions {
                                if !seq.sessions.contains(&s) {
                                    seq.sessions.push(s);
                                }
                            }
                            seq.truncated |= acc.truncated;
                        }
                    }
                    Outcome::Unknown => {}
                }
            }
            "approval_request" => {
                let acc = approvals.entry(fingerprints(e).1).or_default();
                acc.count += 1;
                acc.add(e, received);
            }
            "tool_start" => {
                if let Some(program) = read_workaround(e) {
                    let (acc, programs) = invocations
                        .entry(format!("{}:shell-read", tool_name(e)))
                        .or_default();
                    acc.count += 1;
                    acc.add(e, received);
                    *programs.entry(program).or_default() += 1;
                }
            }
            "ui_wait_end" => {
                if let Some(ms) = e.payload["wait_ms"].as_i64() {
                    waits.count += 1;
                    waits.wait_ms += ms;
                    waits.add(e, received);
                }
            }
            _ => {}
        }
    }

    let mut findings = Vec::new();
    let mut push = |kind,
                    fingerprint: String,
                    acc: Acc,
                    title: String,
                    certainty: &str,
                    notes: Vec<String>| {
        findings.push(Finding {
            id: stable_id(kind, &fingerprint),
            kind,
            title,
            fingerprint,
            count: acc.count,
            evidence_ids: acc.evidence,
            sessions: acc.sessions,
            outcome_certainty: certainty.into(),
            notes,
            measured_wait_ms: (kind == PatternKind::UiWait).then_some(acc.wait_ms),
            truncated_evidence: acc.truncated,
            decision: None,
            recurred_after_decision: false,
            missing_evidence: 0,
            last_received_ms: acc.last_received_ms,
        });
    };
    for (fp, acc) in sequences {
        let title = format!(
            "`{fp}`: {} failed attempt(s) before a working invocation",
            acc.count
        );
        push(
            PatternKind::FailureThenSuccess,
            fp,
            acc,
            title,
            "observed failures followed by a success of the same program; the link is inferred from order",
            vec!["Candidate for documenting the working usage in a skill or tool doc.".into()],
        );
    }
    for (fp, acc) in pending.into_iter().filter(|(_, a)| a.count >= 2) {
        let title = format!(
            "`{fp}`: failed {} times with no later success observed",
            acc.count
        );
        push(
            PatternKind::RepeatedFailure,
            fp,
            acc,
            title,
            "observed failures; no later success seen in the selection",
            vec![],
        );
    }
    for (fp, acc) in approvals.into_iter().filter(|(_, a)| a.count >= 2) {
        let title = format!("`{fp}`: {} approval requests", acc.count);
        push(
            PatternKind::RepeatedApproval,
            fp,
            acc,
            title,
            "requests observed; the human decision and time spent are not observable",
            vec![
                "This may be a correct safety check. Review whether a narrower rule or a different tool fits; never broaden permissions without explicit review.".into(),
            ],
        );
    }
    for (fp, (acc, programs)) in invocations
        .into_iter()
        .filter(|(_, (a, _))| a.count >= REPEATED_INVOCATION)
    {
        let mut programs: Vec<_> = programs.into_iter().collect();
        programs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let breakdown: Vec<_> = programs.iter().map(|(p, n)| format!("{p} {n}")).collect();
        let title = format!(
            "`{fp}`: {} shell reads or searches ({})",
            acc.count,
            breakdown.join(", ")
        );
        push(
            PatternKind::RepeatedInvocation,
            fp,
            acc,
            title,
            "observed shell invocations; the harness offers dedicated read/search tools",
            vec!["The harness has dedicated read and search tools; check whether an instruction should steer the agent to them.".into()],
        );
    }
    if waits.count > 0 {
        let title = format!("{} measured UI prompt wait(s)", waits.count);
        push(
            PatternKind::UiWait,
            "pi:ui_prompt".into(),
            waits,
            title,
            "measured by Pi UI prompt start/end events",
            vec![],
        );
    }

    for f in &mut findings {
        if let Some((d, decided_at_ms)) = store.decision_at(&f.id)? {
            f.decision = Some(d.kind);
            f.recurred_after_decision = f.last_received_ms > decided_at_ms;
            for id in &d.evidence_ids {
                f.missing_evidence += (!store.exists(id)?) as usize;
            }
        }
    }

    let key = selection_key(sessions);
    let refs: Vec<FindingRef> = findings
        .iter()
        .map(|f| FindingRef {
            id: f.id.clone(),
            count: f.count,
            title: f.title.clone(),
            evidence_ids: f.evidence_ids.clone(),
        })
        .collect();
    let refs_json = serde_json::to_string(&refs)?;
    let summary = |r: &[FindingRef]| {
        r.iter()
            .map(|f| (f.id.clone(), f.count))
            .collect::<HashSet<_>>()
    };
    let changed = match store.last_debrief(&key)? {
        None => true,
        Some((_, last)) => {
            summary(&serde_json::from_str::<Vec<FindingRef>>(&last)?) != summary(&refs)
        }
    };
    store.record_debrief(&key, cutoff_ms, &refs_json, now_ms)?;

    Ok(Debrief {
        selection: sessions.to_vec(),
        included_sessions: included,
        cutoff_ms,
        changed,
        findings,
        coverage,
    })
}

/// Coach-generated finding summary for a decision record: never raw output.
pub fn decision_summary(store: &Store, finding_id: &str) -> Result<Option<(String, Vec<String>)>> {
    let Some(json) = store.debrief_mentioning(finding_id)? else {
        return Ok(None);
    };
    let refs: Vec<FindingRef> = serde_json::from_str(&json)?;
    Ok(refs
        .into_iter()
        .find(|f| f.id == finding_id)
        .map(|f| (f.title, f.evidence_ids)))
}

pub fn render_markdown(d: &Debrief) -> String {
    let mut out = format!(
        "# Coach debrief\n\nSessions: {} (selected: {})\nEvents considered: {} from {}\n",
        d.included_sessions.join(", "),
        d.selection.join(", "),
        d.coverage.events,
        d.coverage.sources.join(", ")
    );
    let c = &d.coverage;
    if c.unknown_owner_events + c.parse_error_events + c.truncated_events + c.unknown_kind_events
        > 0
    {
        out += &format!(
            "Coverage gaps: {} unknown-owner, {} unparsed, {} truncated, {} unknown-kind events\n",
            c.unknown_owner_events, c.parse_error_events, c.truncated_events, c.unknown_kind_events
        );
    }
    if !d.changed {
        out += "No change since the last debrief of this selection.\n";
    }
    if d.findings.is_empty() {
        out += "\nNo recurring patterns found.\n";
    }
    for f in &d.findings {
        out += &format!("\n## {} ({})\n\n", f.title, f.id);
        out += &format!("- Certainty: {}\n", f.outcome_certainty);
        if let Some(ms) = f.measured_wait_ms {
            out += &format!("- Measured total: {:.1} s\n", ms as f64 / 1000.0);
        }
        for n in &f.notes {
            out += &format!("- Note: {n}\n");
        }
        if let Some(dk) = f.decision {
            out += &format!(
                "- Earlier decision: {dk:?}{}\n",
                if f.recurred_after_decision {
                    "; pattern recurred since"
                } else {
                    ""
                }
            );
        }
        if f.missing_evidence > 0 {
            out += &format!(
                "- {} earlier evidence event(s) have been purged\n",
                f.missing_evidence
            );
        }
        if f.truncated_evidence {
            out += "- Some evidence was truncated at capture\n";
        }
        out += &format!(
            "- Evidence: {} (inspect with `coach evidence <id>`)\n",
            f.evidence_ids.join(", ")
        );
    }
    out
}
