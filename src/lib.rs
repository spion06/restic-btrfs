//! rbtrfs — consistent btrfs-snapshot backups into a restic-format repository.
//!
//! See `DESIGN.md` for the architecture and the Milestone 0 spike results.

pub mod backup;
pub mod btrfs;
pub mod cli;
pub mod config;
pub mod discover;
pub mod excludes;
pub mod forget;
pub mod gc;
pub mod hooks;
pub mod lock;
pub mod mountinfo;
pub mod ns;
pub mod repo;
pub mod restore;
pub mod runid;
pub mod select;
pub mod signals;
pub mod snapshot;

/// Are we running as root (`CAP_SYS_ADMIN` proxy)? Subvolume enumeration,
/// snapshotting and mounting all need it.
pub fn is_root() -> bool {
    nix::unistd::Uid::effective().is_root()
}

/// This machine's hostname, as recorded on snapshots.
pub fn hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "localhost".to_string())
}
