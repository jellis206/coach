use std::os::unix::fs::{PermissionsExt, symlink};

use coach::event::{CaptureStatus, EventEnvelope, MAX_FIELD_BYTES, MAX_FRAME_BYTES, decode_frame};
use coach::paths::ensure_private_dir;
use coach::store::{Decision, DecisionKind, RETENTION_MS, Store};
use serde_json::{Value, json};

const NOW: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;

fn ev(id: &str, kind: &str, payload: Value) -> EventEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: id.into(),
        process_instance_id: "proc-1".into(),
        session_id: Some("sess-1".into()),
        agent_id: None,
        parent_agent_id: None,
        turn_id: None,
        tool_call_id: None,
        source: "claude".into(),
        source_version: Some("2.1.311".into()),
        kind: kind.into(),
        occurred_at_ms: NOW,
        sequence: 1,
        payload,
        capture_status: CaptureStatus::default(),
    }
}

fn open(dir: &tempfile::TempDir) -> Store {
    Store::open(&dir.path().join("state").join("coach.db")).unwrap()
}

#[test]
fn retains_evidence_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    open(&dir)
        .insert(&ev("e1", "prompt", json!({"prompt": "hello"})), NOW)
        .unwrap();
    let store = open(&dir);
    let got = store.get("e1").unwrap().unwrap();
    assert_eq!(got.payload["prompt"], "hello");
    assert_eq!(got.session_id.as_deref(), Some("sess-1"));
}

#[test]
fn opens_private_wal_database_with_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    assert_eq!(store.journal_mode().unwrap(), "wal");
    assert_eq!(store.user_version().unwrap(), 2);
    let db = dir.path().join("state").join("coach.db");
    assert_eq!(
        std::fs::metadata(&db).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let state = dir.path().join("state");
    assert_eq!(
        std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let mut store = store;
    store.insert(&ev("e1", "prompt", json!({})), NOW).unwrap();
    for side in ["coach.db-wal", "coach.db-shm"] {
        let mode = std::fs::metadata(state.join(side))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{side}");
    }
}

#[test]
fn deduplicates_event_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store
        .insert(&ev("e1", "prompt", json!({"prompt": "a"})), NOW)
        .unwrap();
    store
        .insert(&ev("e1", "prompt", json!({"prompt": "b"})), NOW + 1)
        .unwrap();
    assert_eq!(store.diagnostics().unwrap().events, 1);
    assert_eq!(store.get("e1").unwrap().unwrap().payload["prompt"], "a");
}

#[test]
fn caps_utf8_fields_at_64k() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let big = "é".repeat(70_000); // 140_000 bytes, two bytes per char
    store
        .insert(
            &ev("e1", "prompt", json!({"prompt": big, "small": "ok"})),
            NOW,
        )
        .unwrap();
    let got = store.get("e1").unwrap().unwrap();
    let prompt = got.payload["prompt"].as_str().unwrap();
    assert!(prompt.len() <= MAX_FIELD_BYTES);
    assert!(prompt.len() > MAX_FIELD_BYTES - 4);
    assert!(prompt.chars().all(|c| c == 'é'));
    assert_eq!(got.payload["small"], "ok");
    let t = &got.capture_status.truncated;
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].path, "/prompt");
    assert_eq!(t[0].original_bytes, 140_000);
}

#[test]
fn rejects_oversize_frame() {
    let frame = serde_json::to_vec(&ev(
        "e1",
        "prompt",
        json!({"prompt": "x".repeat(MAX_FRAME_BYTES)}),
    ))
    .unwrap();
    assert!(frame.len() > MAX_FRAME_BYTES);
    assert!(decode_frame(&frame).is_err());
    let ok = serde_json::to_vec(&ev("e2", "prompt", json!({"prompt": "x"}))).unwrap();
    assert_eq!(decode_frame(&ok).unwrap().event_id, "e2");
}

#[test]
fn rejects_malformed_frame() {
    assert!(decode_frame(b"{not json").is_err());
    assert!(decode_frame(br#"{"schema_version":1}"#).is_err());
}

#[test]
fn preserves_unknown_event_with_status() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store
        .insert(
            &ev("e1", "future_hook_kind", json!({"novel": {"field": 1}})),
            NOW,
        )
        .unwrap();
    let got = store.get("e1").unwrap().unwrap();
    assert_eq!(got.kind, "future_hook_kind");
    assert!(got.capture_status.unknown_kind);
    assert_eq!(got.payload["novel"]["field"], 1);
    assert_eq!(store.diagnostics().unwrap().unknown_kind_events, 1);
}

#[test]
fn diagnostics_count_unknown_owner_events() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store.insert(&ev("e1", "prompt", json!({})), NOW).unwrap();
    let mut orphan = ev("e2", "session_end", json!({}));
    orphan.process_instance_id = "unknown".into();
    store.insert(&orphan, NOW).unwrap();
    assert_eq!(store.diagnostics().unwrap().unknown_owner_events, 1);
}

#[test]
fn redacts_named_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let payload = json!({
        "tool_input": {
            "command": "curl -H 'Authorization: Bearer abcdef0123456789abcdef' https://x",
            "api_key": "zz-plain-secret",
        },
        "env": {"GITHUB_TOKEN": "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
        "note": "key is sk-ant-api03-bbbbbbbbbbbbbbbbbbbbbbbb and AKIAABCDEFGHIJKLMNOP",
        "token_count": 12,
    });
    store.insert(&ev("e1", "tool_start", payload), NOW).unwrap();
    let got = store.get("e1").unwrap().unwrap();
    let text = got.payload.to_string();
    for secret in [
        "zz-plain-secret",
        "ghp_aaaa",
        "abcdef0123456789abcdef",
        "sk-ant-api03-bbbb",
        "AKIAABCDEFGHIJKLMNOP",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert!(text.contains("curl -H"));
    assert_eq!(
        got.payload["token_count"], 12,
        "non-string counts are not credentials"
    );
    let redacted = &got.capture_status.redacted;
    assert!(redacted.contains(&"/tool_input/api_key".to_string()));
    assert!(redacted.contains(&"/env/GITHUB_TOKEN".to_string()));
    assert!(redacted.contains(&"/note".to_string()));
}

#[test]
fn redacts_whole_private_key_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmU\nQyNTUxOQAAACDsyntheticbody\n-----END OPENSSH PRIVATE KEY-----";
    let payload = json!({
        "stdout": format!("before\n{key}\nafter"),
        // Cut off by bounding before the END line.
        "truncated": "-----BEGIN RSA PRIVATE KEY-----\nMIIEsyntheticbody",
    });
    store
        .insert(&ev("e1", "tool_result", payload), NOW)
        .unwrap();
    let got = store.get("e1").unwrap().unwrap();
    let text = got.payload.to_string();
    for body in ["b3BlbnNzaC1rZXktdjEAAAAABG5vbmU", "Qy", "MIIE"] {
        assert!(!text.contains(body), "{body} leaked: {text}");
    }
    assert!(
        got.payload["stdout"]
            .as_str()
            .unwrap()
            .starts_with("before\n")
    );
    assert!(got.payload["stdout"].as_str().unwrap().ends_with("\nafter"));
}

#[test]
fn prunes_only_expired_detail() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store
        .insert(&ev("old", "prompt", json!({})), NOW - 31 * DAY)
        .unwrap();
    store
        .insert(&ev("fresh", "prompt", json!({})), NOW - DAY)
        .unwrap();
    assert_eq!(RETENTION_MS, 30 * DAY);
    assert_eq!(store.prune(NOW - RETENTION_MS).unwrap(), 1);
    assert!(store.get("old").unwrap().is_none());
    assert!(store.get("fresh").unwrap().is_some());
}

#[test]
fn decisions_survive_prune_without_cloning_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store
        .insert(
            &ev("e1", "tool_failure", json!({"error": "RAW-PRIVATE-OUTPUT"})),
            NOW - 31 * DAY,
        )
        .unwrap();
    let decision = Decision {
        finding_id: "f1".into(),
        kind: DecisionKind::Accepted,
        reference: Some("skills/read-slices".into()),
        summary: "S".repeat(100_000),
        evidence_ids: vec!["e1".into()],
    };
    store.decide(&decision, NOW).unwrap();
    store.prune(NOW - RETENTION_MS).unwrap();
    drop(store);
    let store = open(&dir);
    let got = store.decision("f1").unwrap().unwrap();
    assert_eq!(got.kind, DecisionKind::Accepted);
    assert_eq!(got.reference.as_deref(), Some("skills/read-slices"));
    assert!(got.summary.len() <= 4096, "decision summaries are bounded");
    assert_eq!(
        got.evidence_ids,
        vec!["e1".to_string()],
        "references, not copies"
    );
    assert!(store.get("e1").unwrap().is_none());
}

#[test]
fn diagnostics_exclude_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store
        .insert(
            &ev("e1", "prompt", json!({"prompt": "PRIVATE-PROMPT-TEXT"})),
            NOW,
        )
        .unwrap();
    let diag = serde_json::to_string(&store.diagnostics().unwrap()).unwrap();
    assert!(!diag.contains("PRIVATE-PROMPT-TEXT"));
    assert!(diag.contains("claude"));
}

#[test]
fn rejects_git_and_insecure_paths() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    assert!(
        Store::open(&repo.join("state/coach.db")).is_err(),
        "inside a Git worktree"
    );
    assert!(ensure_private_dir(&repo.join("deep/state")).is_err());

    let open_dir = dir.path().join("world-readable");
    std::fs::create_dir(&open_dir).unwrap();
    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        ensure_private_dir(&open_dir).is_err(),
        "pre-existing permissive dir"
    );

    let target = dir.path().join("target-dir");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.path().join("link");
    symlink(&target, &link).unwrap();
    assert!(ensure_private_dir(&link).is_err(), "symlinked state dir");

    assert!(ensure_private_dir(&dir.path().join("fresh/state")).is_ok());
}
