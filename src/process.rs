//! Verified process identity: (pid, start time), so PID reuse cannot impersonate an owner.

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Seconds since the epoch, as reported by the OS.
    pub started_at: u64,
    /// Stable for the life of the process: `"{pid}:{started_at}"`.
    pub instance_id: String,
}

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub started_at: u64,
    pub name: String,
}

/// Processes that sit between a harness and its hook and are never owners.
const WRAPPERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "fish", "env", "timeout", "coach",
];

fn info(sys: &mut System, pid: u32) -> Option<ProcInfo> {
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[p]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let proc = sys.process(p)?;
    Some(ProcInfo {
        pid,
        ppid: proc.parent().map_or(0, |p| p.as_u32()),
        started_at: proc.start_time(),
        name: proc.name().to_string_lossy().into_owned(),
    })
}

pub fn identify(pid: u32) -> std::io::Result<ProcessIdentity> {
    let p = info(&mut System::new(), pid).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, format!("no process {pid}"))
    })?;
    Ok(identity_of(&p))
}

fn identity_of(p: &ProcInfo) -> ProcessIdentity {
    ProcessIdentity {
        pid: p.pid,
        started_at: p.started_at,
        instance_id: format!("{}:{}", p.pid, p.started_at),
    }
}

/// Alive and still the same process (start time unchanged). Zombies count as dead.
pub fn is_alive(owner: &ProcessIdentity) -> bool {
    let mut sys = System::new();
    let p = Pid::from_u32(owner.pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[p]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(p).is_some_and(|proc| {
        proc.start_time() == owner.started_at && proc.status() != sysinfo::ProcessStatus::Zombie
    })
}

/// Ancestry of `pid`, nearest first, including `pid` itself.
pub fn ancestry(pid: u32) -> Vec<ProcInfo> {
    let mut sys = System::new();
    let mut chain = Vec::new();
    let mut next = pid;
    while next > 1 && chain.len() < 32 {
        let Some(p) = info(&mut sys, next) else { break };
        next = p.ppid;
        chain.push(p);
    }
    chain
}

/// `chain[0]` is the hook process. A harness-provided `hint` PID wins only when it
/// is a real ancestor; otherwise the nearest non-wrapper ancestor is the owner.
pub fn owner_from_chain(chain: &[ProcInfo], hint: Option<u32>) -> Option<&ProcInfo> {
    let ancestors = chain.get(1..)?;
    if let Some(h) = hint.and_then(|h| ancestors.iter().find(|p| p.pid == h)) {
        return Some(h);
    }
    ancestors
        .iter()
        .find(|p| !WRAPPERS.contains(&p.name.trim_start_matches('-')))
}

/// The process that owns the current hook invocation, if it can be proven.
pub fn hook_owner(hint_var: Option<&str>) -> Option<ProcessIdentity> {
    let hint = hint_var
        .and_then(|v| std::env::var(v).ok())
        .and_then(|v| v.parse().ok());
    let chain = ancestry(std::process::id());
    owner_from_chain(&chain, hint).map(identity_of)
}
