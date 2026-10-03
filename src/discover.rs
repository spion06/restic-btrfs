//! Discover btrfs filesystems and their mounted subvolumes from the running
//! system. Nothing here is hardcoded to a particular subvolume-naming scheme.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::btrfs::{BtrfsOps, Subvolume};
use crate::mountinfo::{self, MountInfoEntry};

/// One mounted btrfs subvolume.
#[derive(Debug, Clone)]
pub struct BtrfsMount {
    pub mount_point: PathBuf,
    /// Path of the mount's root within the filesystem (mountinfo field 4). Equals
    /// `subvol` for a normal subvolume mount; longer for a bind mount of a
    /// subdirectory.
    pub root: PathBuf,
    /// Subvolume path within the filesystem, e.g. `/@home`. `/` for the top level.
    pub subvol: String,
    pub subvolid: Option<u64>,
    pub mount_options: String,
}

impl BtrfsMount {
    /// Is this the whole top-level subvolume (not a bind mount of a directory in it)?
    pub fn is_top_level(&self) -> bool {
        self.is_subvol_root() && (self.subvolid == Some(mountinfo_fs_tree_id()) || self.subvol == "/")
    }

    /// Does this mount expose a whole subvolume, rather than a subdirectory of one
    /// (bind mount)? Only whole-subvolume mounts can be snapshotted faithfully.
    pub fn is_subvol_root(&self) -> bool {
        self.root == Path::new(&self.subvol)
    }
}

const fn mountinfo_fs_tree_id() -> u64 {
    5
}

/// A btrfs filesystem and everything of it currently mounted.
#[derive(Debug, Clone)]
pub struct BtrfsFilesystem {
    /// `major:minor` from mountinfo — stable across this filesystem's mounts.
    pub dev: String,
    /// A representative mount source, e.g. `/dev/nvme0n1p2`.
    pub source: String,
    /// Mounts of this filesystem, sorted by mount point.
    pub mounts: Vec<BtrfsMount>,
}

impl BtrfsFilesystem {
    /// The top-level (`subvolid=5`) mount, if one exists on the host.
    pub fn top_level_mount(&self) -> Option<&BtrfsMount> {
        self.mounts.iter().find(|m| m.is_top_level())
    }

    /// Mount serving `path` exactly, if any.
    pub fn mount_at(&self, path: &Path) -> Option<&BtrfsMount> {
        self.mounts.iter().find(|m| m.mount_point == path)
    }

    /// Mounts strictly nested under `roots` that are not themselves in `roots`.
    /// These would be captured as empty directories if their parent is snapshotted
    /// without them — the classic "backed up nothing" trap.
    pub fn nested_unselected(&self, roots: &[PathBuf]) -> Vec<&BtrfsMount> {
        self.mounts
            .iter()
            .filter(|m| {
                !roots.contains(&m.mount_point)
                    && roots.iter().any(|r| is_strict_descendant(&m.mount_point, r))
            })
            .collect()
    }

    /// Enumerate all subvolumes of this filesystem (requires `CAP_SYS_ADMIN`).
    /// Uses any mount of the filesystem as the entry path.
    pub fn subvolumes(&self, btrfs: &dyn BtrfsOps) -> Result<Vec<Subvolume>> {
        let anchor = self
            .mounts
            .first()
            .ok_or_else(|| anyhow!("filesystem {} has no mounts", self.dev))?;
        btrfs
            .list_subvolumes(&anchor.mount_point)
            .with_context(|| format!("listing subvolumes of {}", self.source))
    }
}

fn is_strict_descendant(child: &Path, ancestor: &Path) -> bool {
    child != ancestor && child.starts_with(ancestor)
}

/// Build the filesystem model from `/proc/self/mountinfo`.
pub fn discover() -> Result<Vec<BtrfsFilesystem>> {
    let entries = mountinfo::read().context("reading /proc/self/mountinfo")?;
    Ok(from_mountinfo(&entries))
}

pub fn from_mountinfo(entries: &[MountInfoEntry]) -> Vec<BtrfsFilesystem> {
    let mut by_dev: BTreeMap<String, BtrfsFilesystem> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.fs_type == "btrfs") {
        let fs = by_dev.entry(e.dev.clone()).or_insert_with(|| BtrfsFilesystem {
            dev: e.dev.clone(),
            source: e.source.clone(),
            mounts: Vec::new(),
        });
        fs.mounts.push(BtrfsMount {
            mount_point: e.mount_point.clone(),
            root: e.root.clone(),
            subvol: e.btrfs_subvol().unwrap_or_else(|| "/".to_string()),
            subvolid: e.btrfs_subvolid(),
            mount_options: e.mount_options.clone(),
        });
    }
    let mut out: Vec<_> = by_dev.into_values().collect();
    for fs in &mut out {
        fs.mounts.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Vec<BtrfsFilesystem> {
        from_mountinfo(&mountinfo::parse(s))
    }

    const SAMPLE: &str = "\
40 1 0:34 /@ / rw shared:1 - btrfs /dev/nvme0n1p2 rw,subvolid=256,subvol=/@
99 40 0:34 /@home /home rw shared:2 - btrfs /dev/nvme0n1p2 rw,subvolid=257,subvol=/@home
57 40 0:34 /@srv /srv rw shared:3 - btrfs /dev/nvme0n1p2 rw,subvolid=259,subvol=/@srv
60 99 0:34 /@home-nested /home/vm rw shared:4 - btrfs /dev/nvme0n1p2 rw,subvolid=270,subvol=/@home-nested
1 2 8:1 / /boot rw - ext4 /dev/sda1 rw
70 1 0:55 / /data rw - btrfs /dev/sdb1 rw,subvolid=5,subvol=/\n\
71 70 0:55 /sub /data-sub rw - btrfs /dev/sdb1 rw,subvolid=5,subvol=/";

    #[test]
    fn groups_by_filesystem() {
        let fs = parse(SAMPLE);
        assert_eq!(fs.len(), 2);
        let main = fs.iter().find(|f| f.dev == "0:34").unwrap();
        assert_eq!(main.mounts.len(), 4);
        assert!(main.top_level_mount().is_none());
        let other = fs.iter().find(|f| f.dev == "0:55").unwrap();
        assert_eq!(other.top_level_mount().unwrap().mount_point, PathBuf::from("/data"));
    }

    #[test]
    fn bind_mount_of_subdirectory_is_not_a_subvolume_root() {
        let fs = parse(SAMPLE);
        let other = fs.iter().find(|f| f.dev == "0:55").unwrap();
        assert!(!other.mount_at(Path::new("/data-sub")).unwrap().is_subvol_root());
        assert!(other.mount_at(Path::new("/data")).unwrap().is_subvol_root());
        let main = fs.iter().find(|f| f.dev == "0:34").unwrap();
        assert!(main.mounts.iter().all(|m| m.is_subvol_root()));
    }

    #[test]
    fn finds_nested_unselected() {
        let fs = parse(SAMPLE);
        let main = fs.iter().find(|f| f.dev == "0:34").unwrap();
        let roots = vec![PathBuf::from("/home"), PathBuf::from("/srv")];
        let nested = main.nested_unselected(&roots);
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].mount_point, PathBuf::from("/home/vm"));
    }
}
