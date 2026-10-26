use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use coach::daemon::{EventQueue, GRACE, LIVENESS_INTERVAL, Leases, Tick};
use coach::event::{CaptureStatus, EventEnvelope, WireMessage};
use coach::process::{self, ProcInfo, ProcessIdentity};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_coach");
/// Longest time the daemon may take to notice a dead owner and finish grace.
const EXIT_WITHIN: Duration = Duration::from_secs(9);

struct Env {
    _dir: tempfile::TempDir,
    state: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    Env { _dir: dir, state }
}

fn coach(env: &Env) -> Command {
    let mut c = Command::new(BIN);
    c.env("COACH_STATE_DIR", &env.state);
    c
}

fn start_daemon(env: &Env) -> Child {
    let child = coach(env)
        .arg("daemon")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(3), || {
        env.state.join("coach.sock").exists()
    });
    child
}

fn sleeper() -> Child {
    Command::new("sleep").arg("120").spawn().unwrap()
}

fn identity(child: &Child) -> ProcessIdentity {
    process::identify(child.id()).unwrap()
}

fn send(env: &Env, messages: &[WireMessage]) {
    let mut s = UnixStream::connect(env.state.join("coach.sock")).unwrap();
    for m in messages {
        let mut line = serde_json::to_vec(m).unwrap();
        line.push(b'\n');
        s.write_all(&line).unwrap();
    }
}

fn register(env: &Env, owner: ProcessIdentity) {
    send(
        env,
        &[WireMessage::Register {
            owner,
            source: "claude".into(),
        }],
    );
}

fn event(owner: &ProcessIdentity, id: &str, kind: &str) -> WireMessage {
    WireMessage::Event {
        event: Box::new(EventEnvelope {
            schema_version: 1,
            event_id: id.into(),
            process_instance_id: owner.instance_id.clone(),
            session_id: Some("s".into()),
            agent_id: None,
            parent_agent_id: None,
            turn_id: None,
            tool_call_id: None,
            source: "claude".into(),
            source_version: None,
            kind: kind.into(),
            occurred_at_ms: 1,
            sequence: 1,
            payload: json!({}),
            capture_status: CaptureStatus::default(),
        }),
    }
}

fn doctor(env: &Env) -> Value {
    let out = coach(env).args(["doctor", "--json"]).output().unwrap();
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

fn exits_within(child: &mut Child, limit: Duration) -> bool {
    wait_until(limit, || child.try_wait().unwrap().is_some())
}

fn kill(child: &mut Child) {
    child.kill().ok();
    child.wait().ok();
}

// --- Pure lease state machine (injected clock and liveness) -------------------

fn ident(pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        started_at: 100,
        instance_id: format!("{pid}:100"),
    }
}

#[test]
fn leases_start_in_grace_and_exit_without_registration() {
    let mut l = Leases::new(0);
    assert_eq!(l.tick(0, |_| true), Tick::Grace);
    assert_eq!(l.tick(GRACE.as_millis() as u64, |_| true), Tick::Exit);
}

#[test]
fn leases_reject_dead_or_reused_owner() {
    let mut l = Leases::new(0);
    assert!(!l.register(ident(1), 0, |_| false));
    assert_eq!(l.len(), 0);
}

#[test]
fn leases_grace_is_cancelled_by_registration() {
    let mut l = Leases::new(0);
    assert!(l.register(ident(1), 0, |_| true));
    assert_eq!(l.tick(10, |_| false), Tick::Grace);
    assert!(l.register(ident(2), 1000, |_| true));
    assert_eq!(l.tick(10_000, |o| o.pid == 2), Tick::Live);
}

#[test]
fn leases_two_roots_live_until_last_exits() {
    let mut l = Leases::new(0);
    l.register(ident(1), 0, |_| true);
    l.register(ident(2), 0, |_| true);
    assert_eq!(l.tick(10, |o| o.pid == 2), Tick::Live);
    assert_eq!(l.tick(20, |_| false), Tick::Grace);
    assert_eq!(l.tick(20 + GRACE.as_millis() as u64, |_| false), Tick::Exit);
}

#[test]
fn leases_reregistration_is_idempotent() {
    let mut l = Leases::new(0);
    l.register(ident(1), 0, |_| true);
    l.register(ident(1), 5, |_| true);
    assert_eq!(l.len(), 1);
}

#[test]
fn queue_saturation_counts_drops() {
    let (q, rx) = EventQueue::bounded(2);
    let owner = ident(1);
    for i in 0..5 {
        let WireMessage::Event { event } = event(&owner, &format!("e{i}"), "prompt") else {
            unreachable!()
        };
        q.push(*event, 0);
    }
    assert_eq!(q.dropped(), 3);
    assert_eq!(rx.try_iter().count(), 2);
}

#[test]
fn owner_skips_shell_wrappers_and_prefers_verified_hint() {
    let chain = [
        ProcInfo {
            pid: 30,
            ppid: 20,
            started_at: 3,
            name: "coach".into(),
        },
        ProcInfo {
            pid: 20,
            ppid: 10,
            started_at: 2,
            name: "sh".into(),
        },
        ProcInfo {
            pid: 10,
            ppid: 5,
            started_at: 1,
            name: "codex".into(),
        },
        ProcInfo {
            pid: 5,
            ppid: 1,
            started_at: 0,
            name: "claude".into(),
        },
    ];
    assert_eq!(process::owner_from_chain(&chain, None).unwrap().pid, 10);
    assert_eq!(
        process::owner_from_chain(&chain, Some(5)).unwrap().pid,
        5,
        "hint that is an ancestor wins"
    );
    assert_eq!(
        process::owner_from_chain(&chain, Some(999)).unwrap().pid,
        10,
        "unverified hint is ignored"
    );
    assert!(
        process::owner_from_chain(&chain[..2], None).is_none(),
        "only shells: no owner"
    );
}

#[test]
fn identify_detects_pid_reuse_by_start_time() {
    let mut s = sleeper();
    let real = identity(&s);
    assert!(process::is_alive(&real));
    let reused = ProcessIdentity {
        started_at: real.started_at - 10,
        ..real.clone()
    };
    assert!(!process::is_alive(&reused));
    kill(&mut s);
    assert!(!process::is_alive(&real));
}

// --- Real daemon ---------------------------------------------------------------

#[test]
fn concurrent_first_startup_runs_one_daemon() {
    let env = env();
    let mut owner = sleeper();
    let mut daemons: Vec<Child> = (0..4)
        .map(|_| {
            coach(&env)
                .arg("daemon")
                .stdin(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    wait_until(Duration::from_secs(3), || {
        env.state.join("coach.sock").exists()
    });
    register(&env, identity(&owner));
    std::thread::sleep(Duration::from_millis(3500));
    let running = daemons
        .iter_mut()
        .filter_map(|d| d.try_wait().unwrap())
        .count();
    let running = daemons.len() - running;
    assert_eq!(running, 1);
    kill(&mut owner);
    for d in &mut daemons {
        assert!(exits_within(d, EXIT_WITHIN));
    }
}

#[test]
fn daemon_lives_until_last_of_two_roots_exits() {
    let env = env();
    let mut d = start_daemon(&env);
    let (mut a, mut b) = (sleeper(), sleeper());
    register(&env, identity(&a));
    register(&env, identity(&b));
    kill(&mut a);
    assert!(!exits_within(
        &mut d,
        LIVENESS_INTERVAL + GRACE + Duration::from_secs(1)
    ));
    assert_eq!(doctor(&env)["daemon"]["leases"], 1);
    kill(&mut b);
    assert!(exits_within(&mut d, EXIT_WITHIN));
}

#[test]
fn child_outliving_parent_keeps_daemon() {
    let env = env();
    let mut d = start_daemon(&env);
    // Parent shell starts a long-lived child and exits immediately.
    let out = Command::new("sh")
        .args(["-c", "sleep 120 >/dev/null & echo $!"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let parent = identity(&out);
    register(&env, parent);
    let out = out.wait_with_output().unwrap();
    let child_pid: u32 = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    register(&env, process::identify(child_pid).unwrap());
    assert!(!exits_within(
        &mut d,
        LIVENESS_INTERVAL + GRACE + Duration::from_secs(1)
    ));
    unsafe { libc::kill(child_pid as i32, libc::SIGKILL) };
    assert!(exits_within(&mut d, EXIT_WITHIN));
}

#[test]
fn reused_pid_registration_is_rejected() {
    let env = env();
    let mut d = start_daemon(&env);
    let mut s = sleeper();
    let real = identity(&s);
    register(
        &env,
        ProcessIdentity {
            started_at: real.started_at - 10,
            ..real
        },
    );
    std::thread::sleep(Duration::from_millis(300));
    let doc = doctor(&env);
    assert_eq!(doc["daemon"]["leases"], 0);
    assert_eq!(doc["daemon"]["rejected_registrations"], 1);
    assert!(
        exits_within(&mut d, EXIT_WITHIN),
        "live pid with wrong start time must not keep daemon alive"
    );
    kill(&mut s);
}

#[test]
fn session_end_does_not_release_process() {
    let env = env();
    let mut d = start_daemon(&env);
    let mut s = sleeper();
    let owner = identity(&s);
    register(&env, owner.clone());
    send(&env, &[event(&owner, "e1", "session_end")]);
    assert!(!exits_within(
        &mut d,
        LIVENESS_INTERVAL + GRACE + Duration::from_secs(1)
    ));
    kill(&mut s);
    assert!(exits_within(&mut d, EXIT_WITHIN));
}

#[test]
fn stale_events_do_not_renew_dead_lease() {
    let env = env();
    let mut d = start_daemon(&env);
    let mut s = sleeper();
    let owner = identity(&s);
    register(&env, owner.clone());
    kill(&mut s);
    let end = Instant::now() + EXIT_WITHIN;
    let mut i = 0;
    while Instant::now() < end && d.try_wait().unwrap().is_none() {
        if let Ok(mut c) = UnixStream::connect(env.state.join("coach.sock")) {
            let mut line = serde_json::to_vec(&event(&owner, &format!("e{i}"), "prompt")).unwrap();
            line.push(b'\n');
            c.write_all(&line).ok();
        }
        i += 1;
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        d.try_wait().unwrap().is_some(),
        "events from a dead owner kept the daemon alive"
    );
}

#[test]
fn new_registration_during_grace_cancels_shutdown() {
    let env = env();
    let mut d = start_daemon(&env);
    let (mut a, mut b) = (sleeper(), sleeper());
    register(&env, identity(&a));
    kill(&mut a);
    std::thread::sleep(Duration::from_millis(1000));
    register(&env, identity(&b));
    assert!(!exits_within(
        &mut d,
        LIVENESS_INTERVAL + GRACE + Duration::from_secs(2)
    ));
    kill(&mut b);
    assert!(exits_within(&mut d, EXIT_WITHIN));
}

fn hook(env: &Env, payload: &str) -> std::process::Output {
    let mut c = coach(env)
        .args(["hook", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    c.wait_with_output().unwrap()
}

#[test]
fn hook_starts_daemon_and_reconnects_after_crash() {
    let env = env();
    let out = hook(
        &env,
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"s1","prompt":"hi"}"#,
    );
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert!(wait_until(Duration::from_secs(3), || doctor(&env)["store"]
        ["events"]
        == 1));
    let pid = doctor(&env)["daemon"]["pid"].as_u64().unwrap();
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    std::thread::sleep(Duration::from_millis(200));
    let out = hook(&env, r#"{"hook_event_name":"Stop","session_id":"s1"}"#);
    assert!(out.status.success());
    assert!(wait_until(Duration::from_secs(3), || doctor(&env)["store"]
        ["events"]
        == 2));
    let doc = doctor(&env);
    assert_ne!(doc["daemon"]["pid"].as_u64().unwrap(), pid);
    assert_eq!(
        doc["store"]["unclean_daemon_exits"], 1,
        "crash losses are reported as uncounted"
    );
    let pid = doc["daemon"]["pid"].as_u64().unwrap();
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
}

#[test]
fn hook_is_fail_open_and_silent_when_state_is_unusable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    let start = Instant::now();
    let mut c = Command::new(BIN)
        .args(["hook", "claude"])
        .env("COACH_STATE_DIR", dir.path().join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(b"{not json").unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn partial_slow_and_malformed_clients_do_not_block() {
    let env = env();
    let mut d = start_daemon(&env);
    let mut s = sleeper();
    let owner = identity(&s);
    let mut partial = UnixStream::connect(env.state.join("coach.sock")).unwrap();
    partial.write_all(br#"{"type":"event","ev"#).unwrap();
    let _silent = UnixStream::connect(env.state.join("coach.sock")).unwrap();
    let mut bad = UnixStream::connect(env.state.join("coach.sock")).unwrap();
    bad.write_all(b"garbage\n").unwrap();
    register(&env, owner.clone());
    send(&env, &[event(&owner, "e1", "prompt")]);
    assert!(wait_until(Duration::from_secs(2), || doctor(&env)["store"]
        ["events"]
        == 1));
    assert!(wait_until(
        Duration::from_secs(2),
        || doctor(&env)["daemon"]["rejected_frames"] == 1
    ));
    drop(partial);
    kill(&mut s);
    assert!(exits_within(&mut d, EXIT_WITHIN));
}

#[test]
fn oversize_frame_is_rejected_without_reading_it_all() {
    let env = env();
    let mut d = start_daemon(&env);
    let mut s = sleeper();
    register(&env, identity(&s));
    let mut c = UnixStream::connect(env.state.join("coach.sock")).unwrap();
    c.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    let _ = c.write_all(&vec![b'x'; 2 * 1024 * 1024]);
    assert!(wait_until(
        Duration::from_secs(2),
        || doctor(&env)["daemon"]["rejected_frames"] == 1
    ));
    kill(&mut s);
    assert!(exits_within(&mut d, EXIT_WITHIN));
}

#[test]
fn socket_symlink_is_refused() {
    let env = env();
    std::fs::create_dir_all(&env.state).unwrap();
    std::fs::set_permissions(
        &env.state,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let target = env._dir.path().join("elsewhere");
    std::fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, env.state.join("coach.sock")).unwrap();
    let out = coach(&env).arg("daemon").output().unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
}

#[test]
fn sqlite_lock_is_reported_and_recovered() {
    let env = env();
    let mut d = start_daemon(&env);
    let mut s = sleeper();
    let owner = identity(&s);
    register(&env, owner.clone());
    let blocker = rusqlite::Connection::open(env.state.join("coach.db")).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    send(&env, &[event(&owner, "locked", "prompt")]);
    assert!(wait_until(
        Duration::from_secs(6),
        || doctor(&env)["daemon"]["write_errors"].as_u64() >= Some(1)
    ));
    blocker.execute_batch("COMMIT").unwrap();
    drop(blocker);
    send(&env, &[event(&owner, "after", "prompt")]);
    assert!(wait_until(Duration::from_secs(3), || doctor(&env)["store"]
        ["events"]
        == 1));
    kill(&mut s);
    assert!(exits_within(&mut d, EXIT_WITHIN));
}

#[test]
fn doctor_without_daemon_reports_not_running() {
    let env = env();
    let doc = doctor(&env);
    assert_eq!(doc["daemon"]["running"], false);
}
