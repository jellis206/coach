use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use coach::debrief::{Debrief, PatternKind, debrief, render_markdown};
use coach::event::{CaptureStatus, EventEnvelope, Truncation};
use coach::store::{Decision, DecisionKind, Store};
use serde_json::{Value, json};

const T0: i64 = 1_800_000_000_000;

struct Fx {
    _dir: tempfile::TempDir,
    state: std::path::PathBuf,
    store: Store,
    n: i64,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let store = Store::open(&state.join("coach.db")).unwrap();
    Fx {
        _dir: dir,
        state,
        store,
        n: 0,
    }
}

impl Fx {
    /// Insert a synthetic event; `at` is minutes after T0 (occurrence and receipt).
    fn add(&mut self, session: &str, kind: &str, at: i64, payload: Value) -> String {
        self.add_with(session, kind, at, payload, |_| {})
    }

    fn add_with(
        &mut self,
        session: &str,
        kind: &str,
        at: i64,
        payload: Value,
        f: impl FnOnce(&mut EventEnvelope),
    ) -> String {
        self.n += 1;
        let id = format!("{session}-{kind}-{}", self.n);
        let mut e = EventEnvelope {
            schema_version: 1,
            event_id: id.clone(),
            process_instance_id: "p:1".into(),
            session_id: Some(session.into()),
            agent_id: None,
            parent_agent_id: None,
            turn_id: None,
            tool_call_id: Some(format!("t{}", self.n)),
            source: "claude".into(),
            source_version: None,
            kind: kind.into(),
            occurred_at_ms: T0 + at * 60_000,
            sequence: self.n as u64,
            payload,
            capture_status: CaptureStatus::default(),
        };
        f(&mut e);
        self.store.insert(&e, T0 + at * 60_000).unwrap();
        id
    }

    fn run(&mut self, sessions: &[&str]) -> Debrief {
        let s: Vec<String> = sessions.iter().map(|s| s.to_string()).collect();
        debrief(&mut self.store, &s, T0 + 10_000 * 60_000).unwrap()
    }
}

fn bash(command: &str) -> Value {
    json!({"raw": {"tool_name": "Bash", "tool_input": {"command": command}}})
}

fn failed(command: &str) -> Value {
    let mut v = bash(command);
    v["outcome"] = json!("error");
    v["raw"]["error"] = json!("Exit code 2\nrg: unrecognized flag --foo");
    v
}

fn ok(command: &str) -> Value {
    let mut v = bash(command);
    v["outcome"] = json!("success");
    v
}

#[test]
fn failed_invocations_then_success_are_one_sequence() {
    let mut f = fx();
    let a = f.add("s1", "tool_failure", 1, failed("rg --foo pattern src"));
    let b = f.add("s1", "tool_failure", 2, failed("rg --bar pattern src"));
    let c = f.add("s1", "tool_result", 3, ok("rg -n pattern src"));
    let d = f.run(&["s1"]);
    assert_eq!(d.findings.len(), 1, "{:#?}", d.findings);
    let x = &d.findings[0];
    assert_eq!(x.kind, PatternKind::FailureThenSuccess);
    assert_eq!(x.fingerprint, "Bash:rg");
    assert_eq!(x.count, 2);
    assert_eq!(x.evidence_ids, vec![a, b, c]);
    assert!(x.outcome_certainty.contains("inferred from order"));
}

#[test]
fn a_different_subcommand_is_not_the_working_retry() {
    let mut f = fx();
    f.add("s1", "tool_failure", 1, failed("git push origin x"));
    f.add("s1", "tool_result", 2, ok("git status"));
    assert!(f.run(&["s1"]).findings.is_empty());
}

#[test]
fn shell_reading_is_one_finding_per_tool() {
    let mut f = fx();
    for (i, c) in [
        "grep -n a x",
        "grep -rn a .",
        "sed -n 1p x",
        "cat x",
        "rg -n a",
    ]
    .iter()
    .enumerate()
    {
        f.add("s1", "tool_start", i as i64, bash(c));
    }
    let d = f.run(&["s1"]);
    assert_eq!(d.findings.len(), 1, "{:#?}", d.findings);
    let x = &d.findings[0];
    assert_eq!(x.fingerprint, "Bash:shell-read");
    assert_eq!(x.count, 5);
    assert!(x.title.contains("grep 2"), "{}", x.title);
}

#[test]
fn repeated_failures_without_success() {
    let mut f = fx();
    f.add("s1", "tool_failure", 1, failed("cargo tset"));
    f.add("s1", "tool_failure", 2, failed("cargo tset"));
    let d = f.run(&["s1"]);
    assert_eq!(d.findings[0].kind, PatternKind::RepeatedFailure);
    assert_eq!(d.findings[0].count, 2);
}

#[test]
fn single_failure_is_not_a_pattern() {
    let mut f = fx();
    f.add("s1", "tool_failure", 1, failed("ls /missing"));
    assert!(f.run(&["s1"]).findings.is_empty());
}

#[test]
fn repeated_approvals_stay_neutral_and_claim_no_wait() {
    let mut f = fx();
    for i in 0..3 {
        f.add_with(
            "s1",
            "approval_request",
            i,
            bash("git push origin feature"),
            |e| e.tool_call_id = None,
        );
    }
    let d = f.run(&["s1"]);
    let x = &d.findings[0];
    assert_eq!(x.kind, PatternKind::RepeatedApproval);
    assert_eq!(x.fingerprint, "Bash:git push");
    assert_eq!(x.count, 3);
    assert!(x.outcome_certainty.contains("not observable"));
    let text = serde_json::to_string(&d).unwrap() + &render_markdown(&d);
    for banned in ["wasted", "wait_ms", "bypass", "allow all"] {
        assert!(!text.to_lowercase().contains(banned), "{banned} in {text}");
    }
    assert!(x.notes.iter().any(|n| n.contains("correct safety check")));
}

#[test]
fn pi_waits_are_measured_not_inferred() {
    let mut f = fx();
    f.add(
        "s1",
        "ui_wait_end",
        1,
        json!({"wait_id": "w1", "wait_ms": 4000, "raw": {"kind": "confirm"}}),
    );
    f.add(
        "s1",
        "ui_wait_end",
        2,
        json!({"wait_id": "w2", "wait_ms": 6000, "raw": {"kind": "confirm"}}),
    );
    f.add(
        "s1",
        "ui_wait_end",
        3,
        json!({"wait_id": null, "wait_ms": null, "raw": {"kind": "confirm"}}),
    );
    let d = f.run(&["s1"]);
    let x = d
        .findings
        .iter()
        .find(|x| x.kind == PatternKind::UiWait)
        .unwrap();
    assert_eq!(x.count, 2, "unmatched ends are not measured waits");
    assert_eq!(x.measured_wait_ms, Some(10_000));
}

#[test]
fn repeated_invocation_of_the_same_workaround() {
    let mut f = fx();
    for i in 0..5 {
        f.add(
            "s1",
            "tool_start",
            i,
            bash(&format!("sed -n {i},+20p src/lib.rs")),
        );
    }
    let d = f.run(&["s1"]);
    let x = d
        .findings
        .iter()
        .find(|x| x.kind == PatternKind::RepeatedInvocation)
        .unwrap();
    assert_eq!(x.fingerprint, "Bash:shell-read");
    assert_eq!(x.count, 5);
}

#[test]
fn fingerprints_never_include_plain_arguments() {
    let mut f = fx();
    for i in 0..5 {
        f.add("s1", "approval_request", i, bash("cat acmecorp-contract"));
        f.add("s1", "approval_request", i, bash("FOO=1 rg -n acmecorp"));
    }
    let d = f.run(&["s1"]);
    let fps: Vec<_> = d.findings.iter().map(|x| x.fingerprint.as_str()).collect();
    assert!(fps.contains(&"Bash:cat"), "{fps:?}");
    assert!(fps.contains(&"Bash:rg -n"), "{fps:?}");
    assert!(!render_markdown(&d).contains("acmecorp"));
}

/// Fingerprint via repeated approvals, which group every program at full detail.
fn fingerprint_of(command: &str) -> String {
    let mut f = fx();
    for i in 0..2 {
        f.add("s1", "approval_request", i, bash(command));
    }
    let d = f.run(&["s1"]);
    assert_eq!(d.findings.len(), 1, "{command}: {:#?}", d.findings);
    d.findings[0].fingerprint.clone()
}

#[test]
fn fingerprints_name_the_program_past_shell_setup() {
    for (command, want) in [
        ("cd /repo && cargo test --quiet", "Bash:cargo test"),
        ("cd /repo; FOO=1 git status", "Bash:git status"),
        ("set -euo pipefail\nexport X=1\nrg -n foo", "Bash:rg -n"),
        ("for f in a b; do wc -l \"$f\"; done", "Bash:wc -l"),
        ("if true; then sed -n 1p x; fi", "Bash:sed -n"),
        ("cd /repo", "Bash:cd"),
    ] {
        assert_eq!(fingerprint_of(command), want, "{command}");
    }
}

#[test]
fn patches_group_as_one_tool_without_their_content() {
    let patch = |name: &str| {
        json!({"raw": {"tool_name": "apply_patch",
            "tool_input": {"command": format!("*** Begin Patch\n*** Add File: /private/{name}\n+secret\n*** End Patch")}}})
    };
    let mut f = fx();
    for i in 0..5 {
        f.add("s1", "tool_start", i, patch(&format!("acmecorp-{i}")));
    }
    let d = f.run(&["s1"]);
    assert!(
        d.findings.is_empty(),
        "patching is the harness's own edit tool: {:#?}",
        d.findings
    );
}

#[test]
fn only_shell_file_reading_is_a_workaround_and_only_beside_a_read_tool() {
    let mut f = fx();
    for i in 0..10 {
        f.add("s1", "tool_start", i, bash("git status"));
        f.add("s1", "tool_start", i, bash("pnpm test"));
        f.add_with("s1", "tool_start", i, bash("cat notes.txt"), |e| {
            e.source = "codex".into()
        });
    }
    assert!(f.run(&["s1"]).findings.is_empty());
    for i in 0..5 {
        f.add("s2", "tool_start", i, bash("cat notes.txt"));
    }
    let d = f.run(&["s2"]);
    assert_eq!(d.findings.len(), 1);
    assert!(
        d.findings[0].notes[0].contains("dedicated"),
        "{:?}",
        d.findings[0].notes
    );
}

#[test]
fn quoted_separators_do_not_make_programs() {
    assert_eq!(
        fingerprint_of("cd /x\n\"select a\" | -S b 2>&1\nrg -n y"),
        "Bash:rg -n"
    );
}

#[test]
fn core_tools_are_not_repeated_workarounds() {
    let mut f = fx();
    for tool in [
        "Edit",
        "Read",
        "Write",
        "Grep",
        "Glob",
        "Agent",
        "apply_patch",
    ] {
        for i in 0..10 {
            f.add(
                "s1",
                "tool_start",
                i,
                json!({"raw": {"tool_name": tool, "tool_input": {}}}),
            );
        }
    }
    assert!(f.run(&["s1"]).findings.is_empty());
}

#[test]
fn unrelated_same_cwd_sessions_are_excluded() {
    let mut f = fx();
    f.add("s1", "tool_failure", 1, failed("cargo tset"));
    f.add("other", "tool_failure", 2, failed("cargo tset"));
    let d = f.run(&["s1"]);
    assert!(
        d.findings.is_empty(),
        "one failure per session; other session must not be merged"
    );
    assert_eq!(d.included_sessions, vec!["s1".to_string()]);
}

#[test]
fn known_descendants_are_included() {
    let mut f = fx();
    f.add_with("root", "session_start", 0, json!({}), |e| {
        e.agent_id = Some("pi:root".into())
    });
    f.add_with("child", "tool_failure", 1, failed("make tset"), |e| {
        e.parent_agent_id = Some("pi:root".into())
    });
    f.add_with("child", "session_start", 1, json!({}), |e| {
        e.agent_id = Some("pi:child".into());
        e.parent_agent_id = Some("pi:root".into());
    });
    f.add_with("grandchild", "tool_failure", 2, failed("make tset"), |e| {
        e.parent_agent_id = Some("pi:child".into())
    });
    f.add("stranger", "tool_failure", 3, failed("make tset"));
    let d = f.run(&["root"]);
    assert_eq!(
        d.included_sessions,
        vec!["child".to_string(), "grandchild".into(), "root".into()]
    );
    assert_eq!(d.findings[0].count, 2);
}

#[test]
fn headless_child_may_name_its_parent_session() {
    // A top-level Claude session has no agent id, so a launcher passes the session id.
    let mut f = fx();
    f.add("parent-session", "tool_failure", 0, failed("make tset"));
    f.add_with("headless", "tool_failure", 1, failed("make tset"), |e| {
        e.parent_agent_id = Some("parent-session".into())
    });
    let d = f.run(&["parent-session"]);
    assert_eq!(
        d.included_sessions,
        vec!["headless".to_string(), "parent-session".into()]
    );
}

#[test]
fn out_of_order_receipt_is_ordered_by_occurrence() {
    let mut f = fx();
    f.add("s1", "tool_result", 5, ok("rg -n x"));
    f.add("s1", "tool_failure", 1, failed("rg --foo x"));
    let d = f.run(&["s1"]);
    assert_eq!(d.findings[0].kind, PatternKind::FailureThenSuccess);
}

#[test]
fn truncated_evidence_is_flagged() {
    let mut f = fx();
    for at in [1, 2] {
        f.add_with("s1", "tool_failure", at, failed("cargo tset"), |e| {
            e.capture_status.truncated.push(Truncation {
                path: "/raw/error".into(),
                original_bytes: 100_000,
            })
        });
    }
    assert!(f.run(&["s1"]).findings[0].truncated_evidence);
}

#[test]
fn repeat_debrief_is_stable_until_new_evidence() {
    let mut f = fx();
    f.add("s1", "tool_failure", 1, failed("cargo tset"));
    f.add("s1", "tool_failure", 2, failed("cargo tset"));
    let first = f.run(&["s1"]);
    let again = f.run(&["s1"]);
    assert!(first.changed);
    assert!(!again.changed, "unchanged evidence must not notify again");
    assert_eq!(first.findings[0].id, again.findings[0].id);
    assert_eq!(first.cutoff_ms, again.cutoff_ms);
    f.add("s1", "tool_failure", 3, failed("cargo tset"));
    let later = f.run(&["s1"]);
    assert!(later.changed);
    assert!(later.cutoff_ms > first.cutoff_ms);
    assert_eq!(later.findings[0].id, first.findings[0].id);
    assert_eq!(later.findings[0].count, 3);
}

#[test]
fn decisions_survive_restart_and_recurrence_shows_missing_evidence() {
    let mut f = fx();
    let a = f.add("s1", "tool_failure", 1, failed("cargo tset"));
    let b = f.add("s1", "tool_failure", 2, failed("cargo tset"));
    let id = f.run(&["s1"]).findings[0].id.clone();
    let decision = Decision {
        finding_id: id.clone(),
        kind: DecisionKind::Accepted,
        reference: Some("skills/cargo-aliases".into()),
        summary: "Use the documented cargo alias".into(),
        evidence_ids: vec![a, b],
    };
    f.store.decide(&decision, T0).unwrap();
    f.store.prune(T0 + 60 * 60_000).unwrap(); // purge s1's detailed evidence
    let mut store = Store::open(&f.state.join("coach.db")).unwrap();
    drop(std::mem::replace(&mut f.store, store));
    f.add("s2", "tool_failure", 100, failed("cargo tset"));
    f.add("s2", "tool_failure", 101, failed("cargo tset"));
    let d = f.run(&["s2"]);
    let x = &d.findings[0];
    assert_eq!(x.id, id, "finding ids are stable across sessions");
    assert_eq!(x.decision, Some(DecisionKind::Accepted));
    assert!(x.recurred_after_decision);
    assert_eq!(
        x.missing_evidence, 2,
        "purged evidence is reported, not silently dropped"
    );
    store = Store::open(&f.state.join("coach.db")).unwrap();
    assert!(store.get(&decision.evidence_ids[0]).unwrap().is_none());
}

#[test]
fn recurrence_is_seen_past_the_evidence_cap() {
    let mut f = fx();
    for at in 1..=25 {
        f.add("s1", "tool_failure", at, failed("cargo tset"));
    }
    let first = f.run(&["s1"]).findings.remove(0);
    let decision = Decision {
        finding_id: first.id.clone(),
        kind: DecisionKind::Accepted,
        reference: None,
        summary: "Use the documented cargo alias".into(),
        evidence_ids: first.evidence_ids,
    };
    f.store.decide(&decision, T0 + 30 * 60_000).unwrap();
    assert!(!f.run(&["s1"]).findings[0].recurred_after_decision);
    f.add("s1", "tool_failure", 40, failed("cargo tset"));
    assert!(f.run(&["s1"]).findings[0].recurred_after_decision);
}

#[test]
fn markdown_shows_evidence_and_interpretation_separately() {
    let mut f = fx();
    f.add("s1", "tool_failure", 1, failed("cargo tset"));
    f.add("s1", "tool_failure", 2, failed("cargo tset"));
    let md = render_markdown(&f.run(&["s1"]));
    assert!(md.contains("Evidence:"));
    assert!(md.contains("Certainty:"));
    assert!(md.contains("coach evidence"));
}

// --- CLI ------------------------------------------------------------------------

fn cli(state: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_coach"))
        .args(args)
        .env("COACH_STATE_DIR", state)
        .output()
        .unwrap()
}

#[test]
fn doctor_is_payload_free_but_evidence_is_explicit() {
    let mut f = fx();
    let id = f.add(
        "s1",
        "prompt",
        1,
        json!({"raw": {"prompt": "PRIVATE-PROMPT-TEXT"}}),
    );
    let doc = cli(&f.state, &["doctor", "--json"]);
    assert!(!String::from_utf8_lossy(&doc.stdout).contains("PRIVATE-PROMPT-TEXT"));
    let ev = cli(&f.state, &["evidence", &id, "--json"]);
    assert!(ev.status.success());
    assert!(String::from_utf8_lossy(&ev.stdout).contains("PRIVATE-PROMPT-TEXT"));
    let gone = cli(&f.state, &["evidence", "never-existed"]);
    assert!(!gone.status.success());
    assert!(String::from_utf8_lossy(&gone.stderr).contains("purged or never recorded"));
}

#[test]
fn sessions_lists_without_payloads() {
    let mut f = fx();
    f.add(
        "s1",
        "prompt",
        1,
        json!({"raw": {"prompt": "PRIVATE-PROMPT-TEXT"}}),
    );
    let out = cli(&f.state, &["sessions", "--json"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"s1\""));
    assert!(!text.contains("PRIVATE-PROMPT-TEXT"));
}

#[test]
fn export_refuses_unsafe_destinations() {
    let mut f = fx();
    f.add("s1", "prompt", 1, json!({"raw": {"prompt": "x"}}));
    let repo = f._dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let inside = repo.join("export.jsonl");
    assert!(
        !cli(
            &f.state,
            &[
                "export",
                "--session",
                "s1",
                "--output",
                inside.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert!(!inside.exists());
    let existing = f._dir.path().join("existing.jsonl");
    std::fs::write(&existing, "keep").unwrap();
    assert!(
        !cli(
            &f.state,
            &[
                "export",
                "--session",
                "s1",
                "--output",
                existing.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "keep");
    let good = f._dir.path().join("out.jsonl");
    assert!(
        cli(
            &f.state,
            &[
                "export",
                "--session",
                "s1",
                "--output",
                good.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert_eq!(std::fs::read_to_string(&good).unwrap().lines().count(), 1);
    assert_eq!(
        std::fs::metadata(&good).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn purge_requires_confirmation_and_a_valid_date() {
    let mut f = fx();
    f.add("s1", "prompt", 1, json!({}));
    assert!(
        !cli(&f.state, &["purge", "--before", "2099-01-01"])
            .status
            .success(),
        "--yes required"
    );
    assert!(
        !cli(&f.state, &["purge", "--before", "2026-13-40", "--yes"])
            .status
            .success()
    );
    assert!(
        !cli(&f.state, &["purge", "--before", "yesterday", "--yes"])
            .status
            .success()
    );
    assert!(
        cli(&f.state, &["purge", "--before", "2099-01-01", "--yes"])
            .status
            .success()
    );
    assert_eq!(
        Store::open(&f.state.join("coach.db"))
            .unwrap()
            .diagnostics()
            .unwrap()
            .events,
        0
    );
}

#[test]
fn finding_decision_via_cli_persists_references_only() {
    let mut f = fx();
    let a = f.add("s1", "tool_failure", 1, failed("cargo tset"));
    f.add("s1", "tool_failure", 2, failed("cargo tset"));
    let out = cli(&f.state, &["debrief", "--session", "s1", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let d: Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = d["findings"][0]["id"].as_str().unwrap().to_string();
    let acc = cli(
        &f.state,
        &[
            "finding",
            "accept",
            &id,
            "--reference",
            "skills/cargo-aliases",
        ],
    );
    assert!(
        acc.status.success(),
        "{}",
        String::from_utf8_lossy(&acc.stderr)
    );
    let got = Store::open(&f.state.join("coach.db"))
        .unwrap()
        .decision(&id)
        .unwrap()
        .unwrap();
    assert_eq!(got.kind, DecisionKind::Accepted);
    assert!(got.evidence_ids.contains(&a));
    assert!(
        !got.summary.contains("unrecognized flag"),
        "summaries are Coach-generated, not raw output"
    );
    assert!(
        !cli(&f.state, &["finding", "dismiss", "no-such-finding"])
            .status
            .success()
    );
}
