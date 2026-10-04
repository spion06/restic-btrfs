//! `BtrfsOps` implementation backed by the `libbtrfsutil` crate.

use std::path::Path;

use anyhow::{Context, Result};
use libbtrfsutil::{CreateSnapshotOptions, IterateSubvolume};

use super::{BtrfsOps, Subvolume};

/// `BTRFS_ROOT_SUBVOL_RDONLY`
const RDONLY_FLAG: u64 = 0x1;

#[derive(Debug, Default, Clone, Copy)]
pub struct LibBtrfsUtil;

impl BtrfsOps for LibBtrfsUtil {
    fn is_subvolume(&self, path: &Path) -> Result<bool> {
        libbtrfsutil::is_subvolume(path)
            .with_context(|| format!("is_subvolume({})", path.display()))
    }

    fn list_subvolumes(&self, path: &Path) -> Result<Vec<Subvolume>> {
        let mut it = IterateSubvolume::new(path);
        it.all();
        let iter = it
            .iter_with_info()
            .with_context(|| format!("iterate subvolumes under {}", path.display()))?;
        let mut out = Vec::new();
        for entry in iter {
            let (rel_path, info) = entry
                .with_context(|| format!("reading subvolume entry under {}", path.display()))?;
            out.push(Subvolume {
                id: info.id(),
                parent_id: info.parent_id().map(|n| n.get()),
                path: rel_path,
                read_only: info.flags() & RDONLY_FLAG != 0,
                uuid: info.uuid().to_string(),
                parent_uuid: info.parent_uuid().map(|u| u.to_string()),
            });
        }
        Ok(out)
    }

    fn snapshot_readonly(&self, src: &Path, dest: &Path) -> Result<()> {
        CreateSnapshotOptions::new()
            .readonly(true)
            .create(src, dest)
            .with_context(|| format!("snapshot {} -> {}", src.display(), dest.display()))
    }

    fn delete_subvolume(&self, path: &Path) -> Result<()> {
        libbtrfsutil::delete_subvolume(path)
            .with_context(|| format!("delete_subvolume({})", path.display()))
    }

    fn create_subvolume(&self, path: &Path) -> Result<()> {
        libbtrfsutil::create_subvolume(path)
            .with_context(|| format!("create_subvolume({})", path.display()))
    }
}
