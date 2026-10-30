//! On-demand daemon: single instance per state dir, one SQLite writer, leases on
//! verified owner processes, exit after the last owner is gone.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::event::{EventEnvelope, MAX_FRAME_BYTES, WireMessage, decode_message};
use crate::paths::Paths;
use crate::process::{self, ProcessIdentity};
use crate::store::Store;

pub const LIVENESS_INTERVAL: Duration = Duration::from_secs(2);
pub const GRACE: Duration = Duration::from_secs(3);
pub const DRAIN: Duration = Duration::from_secs(2);
pub const QUEUE_CAPACITY: usize = 1024;
const MAX_CONNECTIONS: usize = 64;
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_WAIT: Duration = Duration::from_secs(3);
const BATCH: usize = 256;
/// Written when the daemon cannot open its store; hooks skip spawning while it is fresh.
const FAILED_MARKER: &str = "daemon.failed";
const FAILURE_BACKOFF: Duration = Duration::from_secs(60);

pub fn socket_path(paths: &Paths) -> PathBuf {
    paths.state_dir.join("coach.sock")
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

// --- Leases --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Live,
    Grace,
    Exit,
}

/// Owner processes keeping the daemon alive. Only verified registrations add
/// owners; events never renew anything.
pub struct Leases {
    owners: HashMap<String, ProcessIdentity>,
    empty_since: Option<u64>,
}

impl Leases {
    /// Starts empty and in grace, so a daemon nobody registers with exits.
    pub fn new(now_ms: u64) -> Self {
        Leases {
            owners: HashMap::new(),
            empty_since: Some(now_ms),
        }
    }

    pub fn register(
        &mut self,
        owner: ProcessIdentity,
        _now_ms: u64,
        alive: impl Fn(&ProcessIdentity) -> bool,
    ) -> bool {
        if !alive(&owner) {
            return false;
        }
        self.owners.insert(owner.instance_id.clone(), owner);
        self.empty_since = None;
        true
    }

    pub fn unregister(&mut self, instance_id: &str) {
        self.owners.remove(instance_id);
    }

    pub fn tick(&mut self, now_ms: u64, alive: impl Fn(&ProcessIdentity) -> bool) -> Tick {
        self.owners.retain(|_, o| alive(o));
        if !self.owners.is_empty() {
            self.empty_since = None;
            return Tick::Live;
        }
        let since = *self.empty_since.get_or_insert(now_ms);
        if now_ms.saturating_sub(since) >= GRACE.as_millis() as u64 {
            Tick::Exit
        } else {
            Tick::Grace
        }
    }

    pub fn len(&self) -> usize {
        self.owners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }
}

// --- Bounded event queue -------------------------------------------------------

pub struct EventQueue {
    tx: SyncSender<(EventEnvelope, i64)>,
    dropped: AtomicU64,
}

impl EventQueue {
    pub fn bounded(capacity: usize) -> (EventQueue, Receiver<(EventEnvelope, i64)>) {
        let (tx, rx) = sync_channel(capacity);
        (
            EventQueue {
                tx,
                dropped: AtomicU64::new(0),
            },
            rx,
        )
    }

    /// Never blocks: a full queue drops the event and counts it.
    pub fn push(&self, event: EventEnvelope, received_ms: i64) {
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.tx.try_send((event, received_ms))
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct Counters {
    received: AtomicU64,
    written: AtomicU64,
    rejected_frames: AtomicU64,
    rejected_registrations: AtomicU64,
    rejected_connections: AtomicU64,
    write_errors: AtomicU64,
}

#[derive(Serialize)]
pub struct Status {
    pub running: bool,
    pub pid: u32,
    pub leases: usize,
    pub received: u64,
    pub written: u64,
    pub dropped_queue_full: u64,
    pub rejected_frames: u64,
    pub rejected_registrations: u64,
    /// Connections refused over the connection cap; their events are lost.
    pub rejected_connections: u64,
    pub write_errors: u64,
    pub queue_capacity: usize,
}

struct Shared {
    leases: Mutex<Leases>,
    queue: EventQueue,
    counters: Counters,
    connections: AtomicUsize,
    stop_requested: AtomicBool,
}

impl Shared {
    fn status(&self) -> Status {
        let c = &self.counters;
        Status {
            running: true,
            pid: std::process::id(),
            leases: self.leases.lock().unwrap().len(),
            received: c.received.load(Ordering::Relaxed),
            written: c.written.load(Ordering::Relaxed),
            dropped_queue_full: self.queue.dropped(),
            rejected_frames: c.rejected_frames.load(Ordering::Relaxed),
            rejected_registrations: c.rejected_registrations.load(Ordering::Relaxed),
            rejected_connections: c.rejected_connections.load(Ordering::Relaxed),
            write_errors: c.write_errors.load(Ordering::Relaxed),
            queue_capacity: QUEUE_CAPACITY,
        }
    }
}

// --- Daemon --------------------------------------------------------------------

type BoxResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Run until the last verified owner has exited. Returns Ok immediately when
/// another daemon already serves this state directory.
pub fn run(paths: &Paths) -> BoxResult<()> {
    let socket = socket_path(paths);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(paths.state_dir.join("daemon.lock"))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        // A healthy daemon already answers; a draining one releases the lock soon.
        if UnixStream::connect(&socket).is_ok() || Instant::now() >= deadline {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Open the store before binding: a daemon that cannot write must not accept
    // events, and the marker stops every hook from respawning it (see FAILURE_BACKOFF).
    let failed = paths.state_dir.join(FAILED_MARKER);
    let mut store = Store::open(&paths.db).inspect_err(|e| {
        let _ = std::fs::write(&failed, format!("{e}\n"));
    })?;
    let _ = std::fs::remove_file(&failed);

    // We hold the lock, so anything at the socket path is stale or hostile.
    match std::fs::symlink_metadata(&socket) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(&socket)?,
        Ok(_) => return Err(format!("{} exists and is not a socket", socket.display()).into()),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let run_id = store.begin_run(std::process::id(), now_ms())?;
    store.prune(now_ms() - crate::store::RETENTION_MS)?;
    let (queue, rx) = EventQueue::bounded(QUEUE_CAPACITY);
    let shared = Arc::new(Shared {
        leases: Mutex::new(Leases::new(now_ms() as u64)),
        queue,
        counters: Counters::default(),
        connections: AtomicUsize::new(0),
        stop_requested: AtomicBool::new(false),
    });
    let stopping = Arc::new(AtomicBool::new(false));
    let writer = {
        let (shared, stopping) = (shared.clone(), stopping.clone());
        std::thread::spawn(move || write_loop(store, rx, &shared, &stopping))
    };

    let mut last_check = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => accept(stream, &shared),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
        let check = last_check.elapsed() >= LIVENESS_INTERVAL;
        if check {
            last_check = Instant::now();
        }
        let tick = shared
            .leases
            .lock()
            .unwrap()
            .tick(now_ms() as u64, |o| !check || process::is_alive(o));
        if tick == Tick::Exit || shared.stop_requested.load(Ordering::Relaxed) {
            break;
        }
    }

    drop(listener);
    let _ = std::fs::remove_file(&socket);
    stopping.store(true, Ordering::Relaxed);
    let mut store = writer.join().map_err(|_| "writer panicked")?;
    store.end_run(run_id, now_ms(), &shared.status())?;
    Ok(())
}

fn accept(stream: UnixStream, shared: &Arc<Shared>) {
    if shared.connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
        shared.connections.fetch_sub(1, Ordering::AcqRel);
        shared
            .counters
            .rejected_connections
            .fetch_add(1, Ordering::Relaxed);
        return; // dropping closes it
    }
    let shared = shared.clone();
    std::thread::spawn(move || {
        handle(stream, &shared);
        shared.connections.fetch_sub(1, Ordering::AcqRel);
    });
}

fn handle(stream: UnixStream, shared: &Shared) {
    // macOS rejects these with EINVAL once the peer has closed, but the frames it
    // sent are still buffered and must be read, so failures are ignored.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    let mut peer: Option<String> = None;
    loop {
        line.clear();
        let n = match (&mut reader)
            .take(MAX_FRAME_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(n) => n,
            Err(_) => return, // read timeout (slow sender) or reset
        };
        if n == 0 {
            return;
        }
        if line.last() != Some(&b'\n') {
            // Oversize, or the peer closed mid-frame. Either way the stream is unusable.
            shared
                .counters
                .rejected_frames
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        match decode_message(&line[..line.len() - 1]) {
            Err(_) => {
                shared
                    .counters
                    .rejected_frames
                    .fetch_add(1, Ordering::Relaxed);
            }
            Ok(WireMessage::Register { owner, .. }) => {
                if !shared.leases.lock().unwrap().register(
                    owner,
                    now_ms() as u64,
                    process::is_alive,
                ) {
                    shared
                        .counters
                        .rejected_registrations
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(WireMessage::RegisterPeer { .. }) => {
                let owner = peer_pid(&writer).and_then(|pid| process::identify(pid).ok());
                let registered = owner.is_some_and(|o| {
                    peer = Some(o.instance_id.clone());
                    shared
                        .leases
                        .lock()
                        .unwrap()
                        .register(o, now_ms() as u64, process::is_alive)
                });
                if registered {
                    // A verified live peer (Pi) idles between events; only unregistered
                    // connections are held to the slow-sender timeout.
                    let _ = reader.get_ref().set_read_timeout(None);
                } else {
                    shared
                        .counters
                        .rejected_registrations
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(WireMessage::Event { mut event }) => {
                if event.process_instance_id == "peer" {
                    event.process_instance_id = peer.clone().unwrap_or_else(|| "unknown".into());
                }
                shared.counters.received.fetch_add(1, Ordering::Relaxed);
                shared.queue.push(*event, now_ms());
            }
            Ok(WireMessage::Unregister { instance_id }) => {
                shared.leases.lock().unwrap().unregister(&instance_id)
            }
            Ok(WireMessage::Ping { .. }) => {}
            Ok(WireMessage::Stop) => shared.stop_requested.store(true, Ordering::Relaxed),
            Ok(WireMessage::Status) => {
                let mut out = serde_json::to_vec(&shared.status()).unwrap_or_default();
                out.push(b'\n');
                if writer.write_all(&out).is_err() {
                    return;
                }
            }
        }
    }
}

/// PID of the process on the other end of a Unix socket, from kernel credentials.
fn peer_pid(stream: &UnixStream) -> Option<u32> {
    let fd = stream.as_raw_fd();
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut len,
            )
        };
        (rc == 0 && pid > 0).then_some(pid as u32)
    }
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &mut len,
            )
        };
        (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
    }
}

fn write_loop(
    mut store: Store,
    rx: Receiver<(EventEnvelope, i64)>,
    shared: &Shared,
    stopping: &AtomicBool,
) -> Store {
    let mut drain_deadline = None;
    loop {
        let first = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(item) => Some(item),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return store,
        };
        if stopping.load(Ordering::Relaxed) {
            let deadline = *drain_deadline.get_or_insert_with(|| Instant::now() + DRAIN);
            if first.is_none() || Instant::now() >= deadline {
                return store;
            }
        }
        let Some(first) = first else { continue };
        let mut batch = vec![first];
        batch.extend(rx.try_iter().take(BATCH - 1));
        let n = batch.len() as u64;
        match store.insert_batch(&batch) {
            Ok(()) => shared.counters.written.fetch_add(n, Ordering::Relaxed),
            Err(_) => shared.counters.write_errors.fetch_add(n, Ordering::Relaxed),
        };
    }
}

// --- Client side -------------------------------------------------------------

/// Connect, starting a detached daemon if none answers. Bounded to ~0.5 s.
pub fn connect_or_spawn(paths: &Paths) -> Option<UnixStream> {
    let socket = socket_path(paths);
    if let Ok(s) = UnixStream::connect(&socket) {
        return Some(s);
    }
    let recently_failed = std::fs::metadata(paths.state_dir.join(FAILED_MARKER))
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < FAILURE_BACKOFF));
    if recently_failed {
        return None;
    }
    spawn_daemon().ok()?;
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if let Ok(s) = UnixStream::connect(&socket) {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

fn spawn_daemon() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(std::env::current_exe()?)
        .arg("daemon")
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

/// Send messages as newline-delimited frames; errors are the caller's to ignore.
pub fn send(stream: &mut UnixStream, messages: &[WireMessage]) -> std::io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let mut buf = Vec::new();
    for m in messages {
        serde_json::to_writer(&mut buf, m)?;
        buf.push(b'\n');
    }
    stream.write_all(&buf)
}

/// Ask a running daemon to drain and exit; true once it has (or none was running).
pub fn stop(socket: &Path) -> bool {
    let Ok(mut s) = UnixStream::connect(socket) else {
        return true;
    };
    if send(&mut s, &[WireMessage::Stop]).is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_err() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Ask a running daemon for its counters. None when no daemon answers.
pub fn query_status(socket: &Path) -> Option<serde_json::Value> {
    let mut s = UnixStream::connect(socket).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    send(&mut s, &[WireMessage::Status]).ok()?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}
