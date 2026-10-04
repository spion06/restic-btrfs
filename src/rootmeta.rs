//! Metadata of the directories that are the roots of a backup (each subvolume's mount
//! point and each extra path).
//!
//! rustic_core stores the *contents* of a backup source, but synthesises the source
//! directory itself with default metadata (mode 0755, root, no mtime). Restoring a
//! subvolume would then give its top directory the wrong mode and owner. So the real
//! values are saved in the merged snapshot's description and put back by `restore`.

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const PREFIX: &str = "rbtrfs-roots:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootMeta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
}

/// Recorded path -> metadata of that directory.
pub type Roots = BTreeMap<String, RootMeta>;

/// Read the metadata of `dir` (not following a symlink).
pub fn capture(dir: &Path) -> Result<RootMeta> {
    let m = std::fs::symlink_metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
    Ok(RootMeta {
        mode: m.permissions().mode() & 0o7777,
        uid: m.uid(),
        gid: m.gid(),
        user: nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(m.uid()))
            .ok()
            .flatten()
            .map(|u| u.name),
        group: nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(m.gid()))
            .ok()
            .flatten()
            .map(|g| g.name),
        mtime_sec: m.mtime(),
        mtime_nsec: m.mtime_nsec(),
    })
}

/// The text stored in a snapshot's description.
pub fn encode(roots: &Roots) -> String {
    format!(
        "{PREFIX}{}",
        serde_json::to_string(roots).expect("plain data serialises")
    )
}

/// Parse a snapshot description; anything that is not ours gives no roots.
pub fn decode(description: Option<&str>) -> Roots {
    description
        .and_then(|d| d.strip_prefix(PREFIX))
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default()
}

/// Put `meta` on `dir`. Ownership is only set when running as root; with `by_name`
/// the recorded user and group names are looked up here, falling back to the numeric
/// ids when the name does not exist.
pub fn apply(dir: &Path, meta: &RootMeta, by_name: bool) -> Result<()> {
    if crate::is_root() {
        let (mut uid, mut gid) = (meta.uid, meta.gid);
        if by_name {
            if let Some(u) = meta
                .user
                .as_deref()
                .and_then(|n| nix::unistd::User::from_name(n).ok().flatten())
            {
                uid = u.uid.as_raw();
            }
            if let Some(g) = meta
                .group
                .as_deref()
                .and_then(|n| nix::unistd::Group::from_name(n).ok().flatten())
            {
                gid = g.gid.as_raw();
            }
        }
        std::os::unix::fs::chown(dir, Some(uid), Some(gid))
            .with_context(|| format!("chown {}", dir.display()))?;
    }
    // After chown, which can clear setuid/setgid bits.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(meta.mode))
        .with_context(|| format!("chmod {}", dir.display()))?;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    let omit = nix::libc::timespec {
        tv_sec: 0,
        tv_nsec: nix::libc::UTIME_OMIT,
    };
    let mtime = nix::libc::timespec {
        tv_sec: meta.mtime_sec,
        tv_nsec: meta.mtime_nsec,
    };
    let times = [omit, mtime];
    // SAFETY: `path` is a valid NUL-terminated string and `times` has two entries.
    let rc = unsafe {
        nix::libc::utimensat(
            nix::libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            nix::libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("setting mtime of {}", dir.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_round_trips_and_foreign_text_is_ignored() {
        let mut r = Roots::new();
        r.insert(
            "/home".into(),
            RootMeta {
                mode: 0o700,
                uid: 1000,
                gid: 1000,
                user: Some("alice".into()),
                group: None,
                mtime_sec: 1_700_000_000,
                mtime_nsec: 5,
            },
        );
        assert_eq!(decode(Some(&encode(&r))), r);
        assert!(decode(Some("a note someone typed")).is_empty());
        assert!(decode(Some("rbtrfs-roots:{broken")).is_empty());
        assert!(decode(None).is_empty());
    }

    #[test]
    fn apply_sets_mode_and_mtime() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("x");
        std::fs::create_dir(&sub).unwrap();
        let meta = RootMeta {
            mode: 0o750,
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
            user: None,
            group: None,
            mtime_sec: 1_600_000_000,
            mtime_nsec: 123,
        };
        apply(&sub, &meta, false).unwrap();
        let got = capture(&sub).unwrap();
        assert_eq!(
            (got.mode, got.mtime_sec, got.mtime_nsec),
            (0o750, 1_600_000_000, 123)
        );
    }
}
