//! Bounded wire envelope, normalized event kinds and capture status.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::process::ProcessIdentity;

pub const SCHEMA_VERSION: u32 = 1;
/// Largest accepted wire frame. Anything bigger is rejected before parsing.
pub const MAX_FRAME_BYTES: usize = 512 * 1024;
/// Largest stored textual field; longer strings are cut at a char boundary.
pub const MAX_FIELD_BYTES: usize = 64 * 1024;

/// Kinds adapters normalize to. Anything else is stored with `unknown_kind`.
pub const KNOWN_KINDS: &[&str] = &[
    "session_start",
    "session_end",
    "prompt",
    "tool_start",
    "tool_result",
    "tool_failure",
    "approval_request",
    "ui_wait_start",
    "ui_wait_end",
    "subagent_start",
    "subagent_stop",
    "idle",
    "wrap_up_candidate",
    "notification",
    "unknown",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub event_id: String,
    pub process_instance_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub parent_agent_id: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    pub source: String,
    #[serde(default)]
    pub source_version: Option<String>,
    pub kind: String,
    pub occurred_at_ms: i64,
    pub sequence: u64,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub capture_status: CaptureStatus,
}

/// One newline-delimited JSON frame on the local socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireMessage {
    Register {
        owner: ProcessIdentity,
        source: String,
    },
    Event {
        event: Box<EventEnvelope>,
    },
    Unregister {
        instance_id: String,
    },
    Ping {
        instance_id: String,
    },
    /// Ask for payload-free daemon counters; the daemon answers with one JSON line.
    Status,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureStatus {
    #[serde(default)]
    pub unknown_kind: bool,
    /// Adapter could not parse the source payload; `payload` holds what was salvaged.
    #[serde(default)]
    pub parse_error: bool,
    #[serde(default)]
    pub truncated: Vec<Truncation>,
    /// JSON pointer paths whose value was replaced or partially masked.
    #[serde(default)]
    pub redacted: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Truncation {
    pub path: String,
    pub original_bytes: usize,
}

#[derive(Debug)]
pub enum FrameError {
    TooLarge(usize),
    Malformed(serde_json::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge(n) => write!(f, "frame of {n} bytes exceeds {MAX_FRAME_BYTES}"),
            FrameError::Malformed(e) => write!(f, "malformed frame: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Parse one wire frame into a sanitized envelope.
pub fn decode_frame(bytes: &[u8]) -> Result<EventEnvelope, FrameError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(bytes.len()));
    }
    let mut event: EventEnvelope = serde_json::from_slice(bytes).map_err(FrameError::Malformed)?;
    event.sanitize();
    Ok(event)
}

/// Parse one wire message; events are sanitized before they leave this function.
pub fn decode_message(bytes: &[u8]) -> Result<WireMessage, FrameError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(bytes.len()));
    }
    let mut msg: WireMessage = serde_json::from_slice(bytes).map_err(FrameError::Malformed)?;
    if let WireMessage::Event { event } = &mut msg {
        event.sanitize();
    }
    Ok(msg)
}

// Key names whose string values are credentials regardless of content.
static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(passw(or)?d|secret|token|api[_-]?key|authorization|cookie|credential|private[_-]?key)").unwrap()
});
// Common token shapes inside free text. Best effort, not a guarantee.
static SECRET_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(bearer\s+)[a-z0-9._~+/=-]{12,}",
        r"|sk-[a-z0-9_-]{16,}",
        r"|gh[pousr]_[a-z0-9]{20,}",
        r"|github_pat_[a-z0-9_]{20,}",
        r"|xox[abprs]-[a-z0-9-]{10,}",
        r"|AKIA[0-9A-Z]{16}",
        r"|-----BEGIN [A-Z ]*PRIVATE KEY-----",
    ))
    .unwrap()
});

pub const REDACTED: &str = "[REDACTED]";

impl EventEnvelope {
    /// Redact, bound and classify in place. Idempotent.
    pub fn sanitize(&mut self) {
        self.capture_status.unknown_kind = !KNOWN_KINDS.contains(&self.kind.as_str());
        let mut status = std::mem::take(&mut self.capture_status);
        sanitize_value(&mut self.payload, String::new(), &mut status);
        self.capture_status = status;
    }
}

fn sanitize_value(value: &mut Value, path: String, status: &mut CaptureStatus) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let child_path = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                if child.is_string() && SECRET_KEY.is_match(key) {
                    *child = Value::String(REDACTED.into());
                    push_unique(&mut status.redacted, child_path);
                } else {
                    sanitize_value(child, child_path, status);
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter_mut().enumerate() {
                sanitize_value(child, format!("{path}/{i}"), status);
            }
        }
        Value::String(text) => {
            if SECRET_TEXT.is_match(text) {
                *text = SECRET_TEXT
                    .replace_all(text, |c: &regex::Captures| match c.get(1) {
                        Some(prefix) => format!("{}{REDACTED}", prefix.as_str()),
                        None => REDACTED.to_string(),
                    })
                    .into_owned();
                push_unique(&mut status.redacted, path.clone());
            }
            if text.len() > MAX_FIELD_BYTES {
                let original_bytes = text.len();
                text.truncate(text.floor_char_boundary(MAX_FIELD_BYTES));
                status.truncated.push(Truncation {
                    path,
                    original_bytes,
                });
            }
        }
        _ => {}
    }
}

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}
