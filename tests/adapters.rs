//! Normalization of Claude Code (observed live, 2.1.311) and Codex (documented
//! from the pinned rust-v0.164.0 hook schema, not yet observed live) payloads.
//! All payload values are synthetic.

use std::io::Write;
use std::process::{Command, Stdio};

use coach::adapters::{Source, normalize};
use coach::event::{EventEnvelope, MAX_FIELD_BYTES, WireMessage, decode_message};
use coach::process::ProcessIdentity;
use serde_json::{Value, json};

fn owner() -> ProcessIdentity {
    ProcessIdentity {
        pid: 4242,
        started_at: 1_800_000_000,
        instance_id: "4242:1800000000".into(),
    }
}

fn one(source: Source, input: Value) -> EventEnvelope {
    let mut v = normalize(source, input, Some(&owner())).unwrap();
    assert_eq!(v.len(), 1, "{v:#?}");
    v.remove(0)
}

fn claude(event: &str, extra: Value) -> Value {
    let mut base = json!({
        "session_id": "11111111-2222-3333-4444-555555555555",
        "transcript_path": "/home/alex/.claude/projects/demo/session.jsonl",
        "cwd": "/home/alex/demo",
        "hook_event_name": event,
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

fn codex(event: &str, extra: Value) -> Value {
    let mut base = json!({
        "session_id": "sess-codex-1",
        "turn_id": "turn-7",
        "transcript_path": null,
        "cwd": "/home/alex/demo",
        "hook_event_name": event,
        "model": "gpt-demo",
        "permission_mode": "default",
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

#[test]
fn claude_session_lifecycle_kinds() {
    let start = one(
        Source::Claude,
        claude("SessionStart", json!({"source": "clear", "model": "m"})),
    );
    assert_eq!(start.kind, "session_start");
    assert_eq!(
        start.session_id.as_deref(),
        Some("11111111-2222-3333-4444-555555555555")
    );
    assert_eq!(start.process_instance_id, "4242:1800000000");
    let end = one(
        Source::Claude,
        claude("SessionEnd", json!({"reason": "clear"})),
    );
    assert_eq!(end.kind, "session_end");
    let stop = one(
        Source::Claude,
        claude("Stop", json!({"stop_hook_active": false})),
    );
    assert_eq!(stop.kind, "idle", "Stop is idle, never completion");
}

#[test]
fn claude_prompt_maps_prompt_id_to_turn() {
    let e = one(
        Source::Claude,
        claude(
            "UserPromptSubmit",
            json!({"prompt": "hello", "prompt_id": "p-1"}),
        ),
    );
    assert_eq!(e.kind, "prompt");
    assert_eq!(e.turn_id.as_deref(), Some("p-1"));
    assert_eq!(e.payload["raw"]["prompt"], "hello");
}

#[test]
fn claude_tool_start_and_result_share_tool_call_id() {
    let input = json!({"tool_name": "Bash", "tool_input": {"command": "echo ok"}, "tool_use_id": "toolu_1"});
    let start = one(Source::Claude, claude("PreToolUse", input.clone()));
    let mut post = input.clone();
    post["tool_response"] = json!({"stdout": "ok", "stderr": "", "interrupted": false});
    let result = one(Source::Claude, claude("PostToolUse", post));
    assert_eq!(start.kind, "tool_start");
    assert_eq!(result.kind, "tool_result");
    assert_eq!(start.tool_call_id.as_deref(), Some("toolu_1"));
    assert_eq!(result.tool_call_id, start.tool_call_id);
    assert_eq!(result.payload["outcome"], "success");
}

#[test]
fn claude_failure_event_and_interrupt_are_distinct() {
    let base = json!({"tool_name": "Bash", "tool_input": {"command": "ls /missing"}, "tool_use_id": "toolu_2",
                      "error": "Exit code 1\nls: /missing: No such file or directory", "duration_ms": 4});
    let mut err = base.clone();
    err["is_interrupt"] = json!(false);
    let e = one(Source::Claude, claude("PostToolUseFailure", err));
    assert_eq!(e.kind, "tool_failure");
    assert_eq!(e.payload["outcome"], "error");
    assert_eq!(e.payload["outcome_basis"], "source_failure_event");
    let mut int = base;
    int["is_interrupt"] = json!(true);
    let i = one(Source::Claude, claude("PostToolUseFailure", int));
    assert_eq!(i.kind, "tool_failure");
    assert_eq!(i.payload["outcome"], "interrupted");
}

#[test]
fn claude_permission_request_has_no_tool_call_id_and_no_outcome() {
    let e = one(
        Source::Claude,
        claude(
            "PermissionRequest",
            json!({"tool_name": "Bash", "tool_input": {"command": "touch x"}, "permission_suggestions": []}),
        ),
    );
    assert_eq!(e.kind, "approval_request");
    assert!(e.tool_call_id.is_none());
    assert_eq!(e.payload["tool_call_link"], "approximate");
    assert!(
        e.payload.get("outcome").is_none(),
        "a request is not a human decision"
    );
    assert!(e.payload.get("wait_ms").is_none(), "no fabricated wait");
}

#[test]
fn claude_permission_notification_is_not_a_wait() {
    let e = one(
        Source::Claude,
        claude(
            "Notification",
            json!({"notification_type": "permission_prompt", "message": "m"}),
        ),
    );
    assert_eq!(e.kind, "notification");
}

#[test]
fn claude_subagent_identity() {
    let s = one(
        Source::Claude,
        claude(
            "SubagentStart",
            json!({"agent_id": "a-1", "agent_type": "general-purpose"}),
        ),
    );
    assert_eq!(s.kind, "subagent_start");
    assert_eq!(s.agent_id.as_deref(), Some("a-1"));
    let t = one(
        Source::Claude,
        claude(
            "PreToolUse",
            json!({"tool_name": "Bash", "tool_input": {"command": "echo sub"}, "tool_use_id": "t3", "agent_id": "a-1", "agent_type": "general-purpose"}),
        ),
    );
    assert_eq!(
        t.agent_id.as_deref(),
        Some("a-1"),
        "subagent tool calls carry the subagent id"
    );
    assert!(
        t.parent_agent_id.is_none(),
        "hook payloads never state a parent; unknown stays unknown"
    );
}

#[test]
fn fork_or_resume_does_not_invent_parent() {
    let e = one(
        Source::Claude,
        claude("SessionStart", json!({"source": "resume"})),
    );
    assert!(e.parent_agent_id.is_none());
    assert!(e.agent_id.is_none());
}

#[test]
fn duplicate_tool_delivery_has_same_event_id() {
    let input = claude(
        "PreToolUse",
        json!({"tool_name": "Bash", "tool_input": {"command": "echo"}, "tool_use_id": "t9"}),
    );
    let a = one(Source::Claude, input.clone());
    let b = one(Source::Claude, input);
    assert_eq!(a.event_id, b.event_id);
}

#[test]
fn non_tool_events_are_never_collapsed() {
    let input = claude(
        "Stop",
        json!({"stop_hook_active": false, "prompt_id": "p-1"}),
    );
    let a = one(Source::Claude, input.clone());
    let b = one(Source::Claude, input);
    assert_ne!(
        a.event_id, b.event_id,
        "two Stops in one prompt are real (async subagent re-entry)"
    );
}

#[test]
fn out_of_order_receipt_still_links() {
    let mut post = claude(
        "PostToolUse",
        json!({"tool_name": "Read", "tool_input": {}, "tool_use_id": "t5", "tool_response": {}}),
    );
    let r = one(Source::Claude, post.clone());
    post["hook_event_name"] = json!("PreToolUse");
    let s = one(Source::Claude, post);
    assert_eq!(r.tool_call_id, s.tool_call_id);
    assert_ne!(r.event_id, s.event_id);
}

#[test]
fn unknown_future_event_is_preserved_and_flagged() {
    let e = one(
        Source::Claude,
        claude("SomethingNew", json!({"novel": true})),
    );
    assert_eq!(e.kind, "unknown");
    assert!(e.capture_status.unknown_kind);
    assert_eq!(e.payload["source_event"], "SomethingNew");
    assert_eq!(e.payload["raw"]["novel"], true);
}

#[test]
fn non_object_input_is_a_parse_error_not_a_crash() {
    let e = one(Source::Codex, json!([1, 2, 3]));
    assert!(e.capture_status.parse_error);
    assert_eq!(e.kind, "unknown");
}

#[test]
fn credential_bearing_command_is_redacted() {
    let e = one(
        Source::Claude,
        claude(
            "PreToolUse",
            json!({"tool_name": "Bash", "tool_use_id": "t6",
            "tool_input": {"command": "curl -H 'Authorization: Bearer abcdefabcdefabcdef123' https://api.example"}}),
        ),
    );
    assert!(!e.payload.to_string().contains("abcdefabcdefabcdef123"));
    assert!(!e.capture_status.redacted.is_empty());
}

#[test]
fn newline_payloads_cannot_forge_frames() {
    let forged = "x\n{\"type\":\"register\",\"owner\":{\"pid\":1,\"started_at\":0,\"instance_id\":\"1:0\"},\"source\":\"claude\"}\n";
    let e = one(
        Source::Claude,
        claude("UserPromptSubmit", json!({"prompt": forged})),
    );
    let msg = WireMessage::Event { event: Box::new(e) };
    let frame = serde_json::to_vec(&msg).unwrap();
    assert!(!frame.contains(&b'\n'));
    let WireMessage::Event { event } = decode_message(&frame).unwrap() else {
        panic!()
    };
    assert_eq!(event.payload["raw"]["prompt"], forged);
}

#[test]
fn merge_command_is_only_a_candidate_wrap_up() {
    let v = normalize(
        Source::Claude,
        claude("PreToolUse", json!({"tool_name": "Bash", "tool_use_id": "t7", "tool_input": {"command": "gh pr merge 12 --squash"}})),
        Some(&owner()),
    )
    .unwrap();
    let kinds: Vec<_> = v.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["tool_start", "wrap_up_candidate"]);
    let w = &v[1];
    assert_eq!(w.payload["basis"], "command_text");
    assert_eq!(w.payload["remote_state"], "unverified");
    assert_eq!(w.tool_call_id.as_deref(), Some("t7"));
    let plain = normalize(
        Source::Claude,
        claude("PreToolUse", json!({"tool_name": "Bash", "tool_use_id": "t8", "tool_input": {"command": "echo 'gh pr merge' >> notes.md"}})),
        Some(&owner()),
    )
    .unwrap();
    assert_eq!(
        plain.len(),
        1,
        "mentioning merge inside an echo is not a merge"
    );
}

#[test]
fn missing_owner_is_recorded_as_unknown() {
    let v = normalize(Source::Claude, claude("Stop", json!({})), None).unwrap();
    assert_eq!(v[0].process_instance_id, "unknown");
}

// --- Codex (documented shapes) ----------------------------------------------

#[test]
fn codex_tool_events_carry_turn_and_agent() {
    let e = one(
        Source::Codex,
        codex(
            "PreToolUse",
            json!({"tool_name": "shell", "tool_input": {"command": ["ls"]}, "tool_use_id": "call_1", "agent_id": "ag", "agent_type": "worker"}),
        ),
    );
    assert_eq!(e.kind, "tool_start");
    assert_eq!(e.turn_id.as_deref(), Some("turn-7"));
    assert_eq!(e.agent_id.as_deref(), Some("ag"));
    assert_eq!(e.source, "codex");
}

#[test]
fn codex_post_tool_use_outcome_is_interpreted_conservatively() {
    let post = |resp: Value| {
        one(
            Source::Codex,
            codex(
                "PostToolUse",
                json!({"tool_name": "shell", "tool_input": {}, "tool_use_id": "c", "tool_response": resp}),
            ),
        )
    };
    let failed = post(json!({"exit_code": 2, "output": "boom"}));
    assert_eq!(failed.kind, "tool_result");
    assert_eq!(failed.payload["outcome"], "error");
    assert_eq!(failed.payload["outcome_basis"], "exit_code");
    let ok = post(json!({"exit_code": 0, "output": "fine"}));
    assert_eq!(ok.payload["outcome"], "success");
    let unknown = post(json!("free text that may describe an error"));
    assert_eq!(
        unknown.payload["outcome"], "unknown",
        "no fabricated failure from free text"
    );
}

#[test]
fn codex_permission_request_and_lifecycle() {
    let p = one(
        Source::Codex,
        codex(
            "PermissionRequest",
            json!({"tool_name": "shell", "tool_input": {}}),
        ),
    );
    assert_eq!(p.kind, "approval_request");
    assert!(p.tool_call_id.is_none());
    assert_eq!(
        one(Source::Codex, codex("Interrupt", json!({}))).kind,
        "interrupt"
    );
    assert_eq!(
        one(
            Source::Codex,
            codex("PreCompact", json!({"trigger": "auto"}))
        )
        .kind,
        "compaction"
    );
    assert_eq!(
        one(
            Source::Codex,
            codex(
                "SubagentStop",
                json!({"agent_id": "a", "agent_type": "t", "stop_hook_active": false})
            )
        )
        .kind,
        "subagent_stop"
    );
    let end = one(
        Source::Codex,
        json!({"session_id": "s", "transcript_path": null, "cwd": "/x", "hook_event_name": "SessionEnd", "reason": "other"}),
    );
    assert_eq!(end.kind, "session_end");
}

// --- Hook binary bounding -----------------------------------------------------

#[test]
fn hook_drains_huge_input_and_keeps_identity() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let big = "y".repeat(2 * 1024 * 1024);
    let payload = claude(
        "PostToolUse",
        json!({"tool_name": "Bash", "tool_use_id": "big-1", "tool_input": {"command": "cat big"}, "tool_response": {"stdout": big}}),
    );
    let bytes = serde_json::to_vec(&payload).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_coach"))
        .args(["hook", "claude"])
        .env("COACH_STATE_DIR", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let wrote = stdin.write_all(&bytes);
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(wrote.is_ok(), "hook closed stdin early: {wrote:?}");
    assert!(out.status.success());
    assert!(out.stdout.is_empty());

    let db = state.join("coach.db");
    let found = (0..60).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        coach::store::Store::open(&db)
            .ok()
            .and_then(|s| s.diagnostics().ok())
            .is_some_and(|d| d.events == 1)
    });
    assert!(found, "event was not stored");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let envelope: String = conn
        .query_row("SELECT envelope FROM events", [], |r| r.get(0))
        .unwrap();
    let e: EventEnvelope = serde_json::from_str(&envelope).unwrap();
    assert_eq!(e.kind, "tool_result");
    assert_eq!(e.tool_call_id.as_deref(), Some("big-1"));
    assert!(
        e.capture_status
            .truncated
            .iter()
            .any(|t| t.path == "/raw/tool_response/stdout")
    );
    assert!(
        e.payload["raw"]["tool_response"]["stdout"]
            .as_str()
            .unwrap()
            .len()
            <= MAX_FIELD_BYTES
    );
    let pid: i64 = conn
        .query_row("SELECT pid FROM daemon_runs ORDER BY id DESC", [], |r| {
            r.get(0)
        })
        .unwrap();
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
}
