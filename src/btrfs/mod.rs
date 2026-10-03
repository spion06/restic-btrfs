//! btrfs operations, behind a trait so the backend (libbtrfsutil today, raw
//! ioctls or `btrfs(8)` later) can be swapped without touching callers.

use std::path::{Path, PathBuf};

use anyhow::Result;

pub mod util;

pub use util::LibBtrfsUtil;

/// A subvolume as enumerated from a filesystem.
#[derive(Debug, Clone)]
pub struct Subvolume {
    pub id: u64,
    pub parent_id: Option<u64>,
    /// Path relative to the filesystem root (no leading `/`), e.g. `@home`.
    pub path: PathBuf,
    pub read_only: bool,
    pub uuid: String,
    /// For snapshots: the UUID of the source subvolume.
    pub parent_uuid: Option<String>,
}

pub trait BtrfsOps {
    /// Is `path` the root of a btrfs subvolume?
    fn is_subvolume(&self, path: &Path) -> Result<bool>;

    /// Enumerate every subvolume in the filesystem containing `path`.
    /// Requires `CAP_SYS_ADMIN`.
    fn list_subvolumes(&self, path: &Path) -> Result<Vec<Subvolume>>;

    /// Create a read-only snapshot of `src` at `dest`. `dest`'s parent must exist
    /// and `dest` must not. Called back-to-back in the snapshot burst, so
    /// implementations must do nothing here beyond the snapshot itself.
    fn snapshot_readonly(&self, src: &Path, dest: &Path) -> Result<()>;

    /// Delete the subvolume at `path`.
    fn delete_subvolume(&self, path: &Path) -> Result<()>;

    /// Create a new, empty subvolume at `path` (which must not exist).
    fn create_subvolume(&self, path: &Path) -> Result<()>;
}
