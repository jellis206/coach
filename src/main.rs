use std::io::Read;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use coach::daemon::{self, now_ms};
use coach::event::{CaptureStatus, EventEnvelope, MAX_FRAME_BYTES, SCHEMA_VERSION, WireMessage};
use coach::paths::Paths;
use coach::process;
use coach::store::Store;

#[derive(Parser)]
#[command(
    name = "coach",
    version,
    about = "Local, private observer that coaches LLM agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the on-demand daemon (normally started by hooks).
    Daemon,
    /// Forward one hook payload from stdin. Always exits 0 with no output.
    Hook { harness: Harness },
    /// Report registration and health without printing captured content.
    Doctor {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Harness {
    Claude,
    Codex,
}

impl Harness {
    fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }

    /// Environment variable the harness sets to its own PID for hooks, if any.
    fn pid_hint(self) -> Option<&'static str> {
        match self {
            Harness::Claude => Some("CLAUDE_PID"),
            Harness::Codex => None,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Hook { harness } => {
            hook(harness);
            ExitCode::SUCCESS
        }
        Cmd::Daemon => match Paths::discover()
            .map_err(Into::into)
            .and_then(|p| daemon::run(&p))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("coach daemon: {e}");
                ExitCode::FAILURE
            }
        },
        Cmd::Doctor { json } => doctor(json),
    }
}

fn hook(harness: Harness) {
    let mut raw = Vec::new();
    let _ = std::io::stdin()
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut raw);
    let Ok(paths) = Paths::discover() else { return };
    let mut status = CaptureStatus::default();
    let payload = if raw.len() > MAX_FRAME_BYTES {
        status.parse_error = true;
        json!({"coach_oversize_bytes_at_least": raw.len()})
    } else {
        serde_json::from_slice(&raw).unwrap_or_else(|_| {
            status.parse_error = true;
            Value::Null
        })
    };
    let owner = process::hook_owner(harness.pid_hint());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let event = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: format!("{}-{}-{nanos:x}", harness.name(), std::process::id()),
        process_instance_id: owner
            .as_ref()
            .map_or_else(|| "unknown".into(), |o| o.instance_id.clone()),
        session_id: payload
            .get("session_id")
            .and_then(Value::as_str)
            .map(Into::into),
        agent_id: None,
        parent_agent_id: None,
        turn_id: None,
        tool_call_id: None,
        source: harness.name().into(),
        source_version: None,
        kind: "unknown".into(),
        occurred_at_ms: now_ms(),
        sequence: nanos,
        payload,
        capture_status: status,
    };
    let Some(mut stream) = daemon::connect_or_spawn(&paths) else {
        return;
    };
    let mut messages = Vec::new();
    if let Some(owner) = owner {
        messages.push(WireMessage::Register {
            owner,
            source: harness.name().into(),
        });
    }
    messages.push(WireMessage::Event {
        event: Box::new(event),
    });
    let _ = daemon::send(&mut stream, &messages);
}

fn doctor(as_json: bool) -> ExitCode {
    let report = match Paths::discover() {
        Err(e) => json!({"ok": false, "error": e.to_string()}),
        Ok(paths) => {
            let daemon = daemon::query_status(&daemon::socket_path(&paths))
                .unwrap_or_else(|| json!({"running": false}));
            let store = Store::open(&paths.db)
                .and_then(|s| s.diagnostics())
                .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
                .unwrap_or_else(|e| json!({"error": e.to_string()}));
            json!({
                "ok": store.get("error").is_none(),
                "version": env!("CARGO_PKG_VERSION"),
                "state_dir": paths.state_dir,
                "daemon": daemon,
                "store": store,
            })
        }
    };
    if as_json {
        println!("{report}");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    }
    if report["ok"] == true {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
