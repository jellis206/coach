//! Machine-local private state locations and their permission checks.

use std::fs::DirBuilder;
use std::io::{Error, ErrorKind, Result};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub db: PathBuf,
}

impl Paths {
    /// macOS: `~/Library/Application Support/Coach`; elsewhere `$XDG_STATE_HOME/coach`
    /// or `~/.local/state/coach`. `COACH_STATE_DIR` overrides both (tests, isolated homes).
    pub fn discover() -> Result<Paths> {
        let state_dir = match std::env::var_os("COACH_STATE_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => default_state_dir()?,
        };
        let state_dir = ensure_private_dir(&state_dir)?;
        Ok(Paths {
            db: state_dir.join("coach.db"),
            state_dir,
        })
    }
}

fn default_state_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "HOME is not set"))?;
    if cfg!(target_os = "macos") {
        return Ok(home.join("Library/Application Support/Coach"));
    }
    Ok(match std::env::var_os("XDG_STATE_HOME") {
        Some(xdg) if Path::new(&xdg).is_absolute() => PathBuf::from(xdg).join("coach"),
        _ => home.join(".local/state/coach"),
    })
}

/// Create `dir` (mode 0700) if missing, then require: a real directory (not a
/// symlink), owned by this user, no group/other permission bits, and not inside
/// a Git worktree. Returns the canonical path.
pub fn ensure_private_dir(dir: &Path) -> Result<PathBuf> {
    if let Some(repo) = enclosing_git_root(dir) {
        return Err(insecure(format!(
            "{} is inside Git worktree {}",
            dir.display(),
            repo.display()
        )));
    }
    if std::fs::symlink_metadata(dir).is_err() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(insecure(format!(
            "{} is not a plain directory",
            dir.display()
        )));
    }
    if meta.uid() != unsafe { libc::getuid() } {
        return Err(insecure(format!(
            "{} is owned by another user",
            dir.display()
        )));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(insecure(format!(
            "{} is accessible to other users",
            dir.display()
        )));
    }
    dir.canonicalize()
}

/// Walk up from `path` (or its nearest existing ancestor, canonicalized) looking for `.git`.
pub fn enclosing_git_root(path: &Path) -> Option<PathBuf> {
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        probe = probe.parent()?.to_path_buf();
    }
    let real = probe.canonicalize().ok()?;
    real.ancestors()
        .find(|p| p.join(".git").exists())
        .map(Path::to_path_buf)
}

fn insecure(msg: String) -> Error {
    Error::new(ErrorKind::PermissionDenied, msg)
}
