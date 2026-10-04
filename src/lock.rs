//! Host-wide run lock: one mutating rbtrfs run (backup or gc) at a time.
//!
//! Snapshot staging, local GC and the snapshot burst all assume nobody else is
//! touching the same staging area. The lock is an `flock` on a file in `/run`,
//! which is shared by every mount namespace, so it also serialises runs that
//! each live in their own private namespace. The kernel drops it on exit,
//! including on crash or SIGKILL.
//!
//! This does NOT coordinate with other restic/rustic processes: rustic_core 0.13
//! writes no restic lock files, so `restic forget --prune` can still run
//! concurrently with a backup. See the README.

use std::fs::{DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

/// Directory for rbtrfs runtime state (lock file, transient mount targets).
pub fn run_dir() -> PathBuf {
    PathBuf::from("/run/rbtrfs")
}

pub struct RunLock {
    _flock: Flock<File>,
}

/// Take the exclusive run lock, failing immediately if another run holds it.
pub fn acquire() -> Result<RunLock> {
    let dir = run_dir();
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))?;
    let path = dir.join("rbtrfs.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening lock file {}", path.display()))?;
    // A file left by an older version keeps its old mode otherwise.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(flock) => Ok(RunLock { _flock: flock }),
        Err((_, Errno::EWOULDBLOCK)) => {
            bail!(
                "another rbtrfs run is in progress (lock held: {})",
                path.display()
            )
        }
        Err((_, e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}
