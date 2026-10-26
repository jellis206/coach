//! Hook payload normalization for Claude Code and Codex command hooks.
//!
//! The envelope payload is `{source_event, raw, ...interpretation}`: `raw` keeps
//! the (sanitized) source payload; interpretation fields say what Coach
//! concluded and on what basis. Nothing here infers a human decision or a wait.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::event::{CaptureStatus, EventEnvelope, SCHEMA_VERSION};
use crate::process::ProcessIdentity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Claude,
    Codex,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Claude => "claude",
            Source::Codex => "codex",
        }
    }
}

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

static DELIVERY: AtomicU64 = AtomicU64::new(0);

/// Normalize one hook delivery. `owner` is None when ownership could not be proven.
pub fn normalize(
    source: Source,
    input: Value,
    owner: Option<&ProcessIdentity>,
) -> Result<Vec<EventEnvelope>> {
    let parse_error = !input.is_object();
    let raw = if parse_error {
        Map::new()
    } else {
        input.as_object().cloned().unwrap_or_default()
    };
    let text = |k: &str| raw.get(k).and_then(Value::as_str).map(str::to_owned);
    let source_event = text("hook_event_name").unwrap_or_default();

    let kind = match source_event.as_str() {
        "SessionStart" => "session_start",
        "SessionEnd" => "session_end",
        "UserPromptSubmit" => "prompt",
        "PreToolUse" => "tool_start",
        "PostToolUse" => "tool_result",
        "PostToolUseFailure" => "tool_failure",
        "PermissionRequest" => "approval_request",
        "Notification" => "notification",
        "Stop" => "idle",
        "SubagentStart" => "subagent_start",
        "SubagentStop" => "subagent_stop",
        "Interrupt" => "interrupt",
        "PreCompact" | "PostCompact" => "compaction",
        _ => "unknown",
    };

    let mut payload = json!({"source_event": source_event});
    match source_event.as_str() {
        "PostToolUseFailure" => {
            let interrupted = raw.get("is_interrupt").and_then(Value::as_bool) == Some(true);
            payload["outcome"] = json!(if interrupted { "interrupted" } else { "error" });
            payload["outcome_basis"] = json!(if interrupted {
                "is_interrupt"
            } else {
                "source_failure_event"
            });
        }
        "PostToolUse" => {
            let (outcome, basis) = result_outcome(source, raw.get("tool_response"));
            payload["outcome"] = json!(outcome);
            payload["outcome_basis"] = json!(basis);
        }
        // Neither harness links a permission request to its tool call id.
        "PermissionRequest" => payload["tool_call_link"] = json!("approximate"),
        _ => {}
    }

    let session_id = text("session_id");
    let tool_call_id = text("tool_use_id").filter(|_| kind != "approval_request");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    // Only tool events have a per-occurrence source key; everything else is unique per delivery.
    let event_id = match (&tool_call_id, kind) {
        (Some(id), "tool_start" | "tool_result" | "tool_failure") => format!(
            "{}:{}:{}:{id}",
            source.name(),
            session_id.as_deref().unwrap_or("-"),
            source_event
        ),
        _ => format!(
            "{}-{}-{nanos:x}-{}",
            source.name(),
            std::process::id(),
            DELIVERY.fetch_add(1, Ordering::Relaxed)
        ),
    };
    let wrap_up = (kind == "tool_start")
        .then(|| command_text(raw.get("tool_input")))
        .flatten()
        .filter(|c| is_wrap_up_command(c));

    payload["raw"] = Value::Object(raw.clone());
    let base = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: event_id.clone(),
        process_instance_id: owner.map_or_else(|| "unknown".into(), |o| o.instance_id.clone()),
        session_id,
        agent_id: text("agent_id"),
        // Hook payloads never name a parent agent; only an explicit launcher can.
        parent_agent_id: std::env::var("COACH_PARENT_AGENT_ID")
            .ok()
            .filter(|v| !v.is_empty()),
        turn_id: text("turn_id").or_else(|| text("prompt_id")),
        tool_call_id,
        source: source.name().into(),
        source_version: None,
        kind: kind.into(),
        occurred_at_ms: (nanos / 1_000_000) as i64,
        sequence: nanos,
        payload,
        capture_status: CaptureStatus {
            parse_error,
            ..Default::default()
        },
    };

    let mut out = vec![base];
    if let Some(command) = wrap_up {
        let mut w = out[0].clone();
        w.event_id = format!("{event_id}:wrap_up");
        w.kind = "wrap_up_candidate".into();
        w.payload =
            json!({"basis": "command_text", "remote_state": "unverified", "command": command});
        out.push(w);
    }
    for e in &mut out {
        e.sanitize();
    }
    Ok(out)
}

/// Claude reports failures as their own event, so a PostToolUse is a success.
/// Codex has no failure event: trust only explicit structured fields.
fn result_outcome(source: Source, response: Option<&Value>) -> (&'static str, &'static str) {
    if source == Source::Claude {
        return ("success", "no_failure_event");
    }
    let Some(obj) = response.and_then(Value::as_object) else {
        return ("unknown", "unstructured_response");
    };
    if let Some(code) = obj.get("exit_code").and_then(Value::as_i64) {
        return if code == 0 {
            ("success", "exit_code")
        } else {
            ("error", "exit_code")
        };
    }
    match obj.get("success").and_then(Value::as_bool) {
        Some(true) => ("success", "success_field"),
        Some(false) => ("error", "success_field"),
        None => ("unknown", "no_outcome_field"),
    }
}

fn command_text(tool_input: Option<&Value>) -> Option<String> {
    match tool_input?.get("command")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// A shell segment that itself runs a merge or an approving review. Text that
/// merely mentions one (echo, comments, heredocs) is not a candidate.
fn is_wrap_up_command(command: &str) -> bool {
    command
        .split(['\n', ';', '|', '&'])
        .map(str::trim)
        .any(|seg| {
            seg.starts_with("gh pr merge")
                || seg.starts_with("git merge ")
                || (seg.starts_with("gh pr review") && seg.contains("--approve"))
        })
}
