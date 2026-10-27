use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use coach::adapters::{self, Source};
use coach::event::{EventEnvelope, MAX_FRAME_BYTES, Truncation, WireMessage};
use coach::paths::{self, Paths};
use coach::store::{Decision, DecisionKind, Store};
use coach::{daemon, debrief, process};

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
    /// Review recurring friction for sessions and their known descendants.
    Debrief {
        #[arg(long = "session", required = true)]
        sessions: Vec<String>,
        #[arg(long)]
        json: bool,
        /// Print nothing unless a wrap-up candidate arrived since the last debrief.
        #[arg(long)]
        if_ready: bool,
    },
    /// List recorded sessions (no captured content).
    Sessions {
        #[arg(long)]
        json: bool,
    },
    /// Show one captured event in full. This prints private content.
    Evidence {
        event_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Write a session's events as JSONL to a new private file outside Git.
    Export {
        #[arg(long)]
        session: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Delete detailed evidence received before a date (YYYY-MM-DD, UTC).
    Purge {
        #[arg(long)]
        before: String,
        #[arg(long)]
        yes: bool,
    },
    /// Record a decision on a finding from a debrief.
    Finding {
        #[arg(value_enum)]
        decision: DecisionArg,
        finding_id: String,
        /// Where the approved general improvement lives (skill, doc, commit).
        #[arg(long)]
        reference: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum DecisionArg {
    Accept,
    Dismiss,
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
        cmd => match run(cmd) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("coach: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

type BoxResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn run(cmd: Cmd) -> BoxResult<()> {
    let paths = Paths::discover()?;
    let mut store = Store::open(&paths.db)?;
    match cmd {
        Cmd::Debrief {
            sessions,
            json,
            if_ready,
        } => {
            if if_ready && !debrief::wrap_up_ready(&store, &sessions)? {
                return Ok(());
            }
            let d = debrief::debrief(&mut store, &sessions, daemon::now_ms())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                print!("{}", debrief::render_markdown(&d));
            }
        }
        Cmd::Sessions { json } => {
            let sessions = store.sessions()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                for s in sessions {
                    println!(
                        "{}  {}  {} events{}",
                        s.session_id,
                        s.sources,
                        s.events,
                        if s.has_parent { "  (child)" } else { "" }
                    );
                }
            }
        }
        Cmd::Evidence { event_id, json } => {
            let e = store
                .get(&event_id)?
                .ok_or_else(|| format!("event {event_id} was purged or never recorded"))?;
            println!(
                "{}",
                if json {
                    serde_json::to_string(&e)?
                } else {
                    serde_json::to_string_pretty(&e)?
                }
            );
        }
        Cmd::Export { session, output } => export(&store, &session, &output)?,
        Cmd::Purge { before, yes } => {
            let cutoff =
                parse_date_ms(&before).ok_or("--before must be a valid YYYY-MM-DD date")?;
            if !yes {
                return Err("purge deletes evidence permanently; pass --yes to confirm".into());
            }
            let n = store.prune(cutoff)?;
            println!("purged {n} event(s); decisions and summaries were kept");
        }
        Cmd::Finding {
            decision,
            finding_id,
            reference,
        } => {
            let (summary, evidence_ids) = debrief::decision_summary(&store, &finding_id)?
                .ok_or_else(|| {
                    format!("finding {finding_id} is not in any debrief; run `coach debrief` first")
                })?;
            let kind = match decision {
                DecisionArg::Accept => DecisionKind::Accepted,
                DecisionArg::Dismiss => DecisionKind::Dismissed,
            };
            store.decide(
                &Decision {
                    finding_id: finding_id.clone(),
                    kind,
                    reference,
                    summary,
                    evidence_ids,
                },
                daemon::now_ms(),
            )?;
            println!("recorded {kind:?} for {finding_id}");
        }
        Cmd::Hook { .. } | Cmd::Daemon | Cmd::Doctor { .. } => unreachable!("handled in main"),
    }
    Ok(())
}

fn export(store: &Store, session: &str, output: &Path) -> BoxResult<()> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if let Some(repo) = paths::enclosing_git_root(parent) {
        return Err(format!(
            "refusing to export evidence into Git worktree {}",
            repo.display()
        )
        .into());
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(output)
        .map_err(|e| {
            format!(
                "cannot create {} (it must not exist): {e}",
                output.display()
            )
        })?;
    for (e, _) in store.events_for(&[session.to_string()])? {
        serde_json::to_writer(&mut file, &e)?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

/// UTC midnight of a strict `YYYY-MM-DD` date, in epoch milliseconds.
fn parse_date_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (
        s[0..4].parse().ok()?,
        s[5..7].parse().ok()?,
        s[8..10].parse().ok()?,
    );
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in_month = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month[(m - 1) as usize] {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400_000)
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
