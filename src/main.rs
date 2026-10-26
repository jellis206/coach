use std::io::Read;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use coach::adapters::{self, Source};
use coach::daemon;
use coach::event::{EventEnvelope, MAX_FRAME_BYTES, Truncation, WireMessage};
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

    fn source(self) -> Source {
        match self {
            Harness::Claude => Source::Claude,
            Harness::Codex => Source::Codex,
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

/// Largest hook input parsed. More is drained and discarded so the harness never
/// sees a broken pipe.
const MAX_HOOK_INPUT: u64 = 16 * 1024 * 1024;

fn hook(harness: Harness) {
    let mut raw = Vec::new();
    let mut stdin = std::io::stdin().lock();
    let _ = (&mut stdin).take(MAX_HOOK_INPUT).read_to_end(&mut raw);
    let excess = std::io::copy(&mut stdin, &mut std::io::sink()).unwrap_or(0);
    let Ok(paths) = Paths::discover() else { return };
    let input = if excess > 0 {
        json!({"coach_oversize_input_bytes": raw.len() as u64 + excess})
    } else {
        serde_json::from_slice(&raw).unwrap_or(Value::Null)
    };
    let owner = process::hook_owner(harness.pid_hint());
    let Ok(events) = adapters::normalize(harness.source(), input, owner.as_ref()) else {
        return;
    };
    let mut messages = Vec::new();
    if let Some(owner) = owner {
        messages.push(WireMessage::Register {
            owner,
            source: harness.name().into(),
        });
    }
    messages.extend(events.into_iter().map(|e| WireMessage::Event {
        event: Box::new(fit_frame(e)),
    }));
    let Some(mut stream) = daemon::connect_or_spawn(&paths) else {
        return;
    };
    let _ = daemon::send(&mut stream, &messages);
}

/// Fields are already capped; if the frame is still too big, keep the identity
/// and the source key names, drop the raw values, and say so.
fn fit_frame(mut e: EventEnvelope) -> EventEnvelope {
    let size = serde_json::to_vec(&WireMessage::Event {
        event: Box::new(e.clone()),
    })
    .map_or(usize::MAX, |v| v.len());
    if size > MAX_FRAME_BYTES {
        let keys: Vec<String> = e.payload["raw"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        e.payload["raw"] = json!({"coach_degraded": "frame_too_large", "keys": keys});
        e.capture_status.truncated.push(Truncation {
            path: "/raw".into(),
            original_bytes: size,
        });
    }
    e
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
