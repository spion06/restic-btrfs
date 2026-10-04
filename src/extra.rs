//! `extra_paths`: directories on any filesystem that are backed up live, next to
//! the snapshotted subvolumes, into the same restic snapshot.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Result};

use crate::mountinfo::MountInfoEntry;
use crate::select::{key_for, Selected};

/// One validated extra path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraPath {
    pub path: PathBuf,
    /// Directory-name-safe key, shared with subvolume keys (see [`key_for`]).
    pub key: String,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub paths: Vec<ExtraPath>,
    pub warnings: Vec<String>,
}

/// The mount that `path` lives on: the one with the longest mount point that is an
/// ancestor of (or equal to) `path`.
pub fn mount_containing<'a>(
    mounts: &'a [MountInfoEntry],
    path: &Path,
) -> Option<&'a MountInfoEntry> {
    mounts
        .iter()
        .filter(|m| path.starts_with(&m.mount_point))
        .max_by_key(|m| m.mount_point.components().count())
}

/// Check `extra` against the selected subvolumes and the mount table.
///
/// `is_dir` is passed in so the checks can be tested without a filesystem.
///
/// - not a directory, relative, or containing `..`: error
/// - already a selected subvolume, or inside one: error (the snapshot covers it, and
///   a live copy would override the snapshot's version when the two are merged)
/// - on a btrfs filesystem that is not selected: warning (a snapshot is better)
pub fn plan(
    extra: &[PathBuf],
    mounts: &[MountInfoEntry],
    selected: &[&Selected],
    is_dir: impl Fn(&Path) -> bool,
) -> Result<Plan> {
    let mut out = Plan::default();
    for raw in extra {
        let path: PathBuf = raw.components().collect(); // drops trailing slashes
        if !path.is_absolute() {
            bail!("extra_paths: {} is not an absolute path", raw.display());
        }
        if path.components().any(|c| c == Component::ParentDir) {
            bail!("extra_paths: {} must not contain `..`", raw.display());
        }
        if !is_dir(&path) {
            bail!("extra_paths: {} is not a directory", path.display());
        }
        if out.paths.iter().any(|p| p.path == path) {
            continue;
        }
        let key = key_for(&path);

        if selected
            .iter()
            .any(|s| s.mount_point == path || s.key == key)
        {
            bail!(
                "extra_paths: {} is already selected in `subvolumes`; remove it from one of them",
                path.display()
            );
        }
        if let Some(m) = mount_containing(mounts, &path) {
            if m.fs_type == "btrfs" {
                if let Some(s) = selected.iter().find(|s| s.mount_point == m.mount_point) {
                    bail!(
                        "extra_paths: {} is inside the selected subvolume {}, which is already \
                         backed up from its snapshot; remove it",
                        path.display(),
                        s.mount_point.display()
                    );
                }
                out.warnings.push(format!(
                    "extra path {} is on btrfs ({}); add that mount to `subvolumes` to back it \
                     up from a consistent snapshot instead of live",
                    path.display(),
                    m.mount_point.display()
                ));
            }
        }
        out.paths.push(ExtraPath { path, key });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mountinfo;

    const MOUNTS: &str = "\
40 1 0:34 /@ / rw - btrfs /dev/sda2 rw,subvolid=256,subvol=/@
41 40 0:34 /@home /home rw - btrfs /dev/sda2 rw,subvolid=257,subvol=/@home
42 40 8:1 / /boot rw - vfat /dev/sda1 rw
43 42 8:1 / /boot/efi rw - vfat /dev/sda3 rw
44 40 0:34 /@data /data rw - btrfs /dev/sda2 rw,subvolid=258,subvol=/@data
45 40 0:50 / /mnt/nas rw - nfs4 nas:/export rw";

    fn sel(mp: &str) -> Selected {
        Selected {
            mount_point: mp.into(),
            subvol: format!("/@{}", mp.trim_matches('/')),
            key: key_for(Path::new(mp)),
        }
    }

    fn run(extra: &[&str], selected: &[&Selected]) -> Result<Plan> {
        let mounts = mountinfo::parse(MOUNTS);
        let extra: Vec<PathBuf> = extra.iter().map(PathBuf::from).collect();
        plan(&extra, &mounts, selected, |_| true)
    }

    #[test]
    fn finds_the_mount_a_path_lives_on() {
        let mounts = mountinfo::parse(MOUNTS);
        let on = |p: &str| {
            mount_containing(&mounts, Path::new(p))
                .unwrap()
                .mount_point
                .clone()
        };
        assert_eq!(on("/boot/efi/EFI"), PathBuf::from("/boot/efi"));
        assert_eq!(on("/boot/loader"), PathBuf::from("/boot"));
        assert_eq!(on("/etc"), PathBuf::from("/"));
        assert_eq!(
            on("/homework"),
            PathBuf::from("/"),
            "/home is not a prefix of /homework"
        );
    }

    #[test]
    fn other_filesystems_are_accepted_without_warnings() {
        let root = sel("/");
        let p = run(&["/boot", "/boot/efi/", "/mnt/nas/share"], &[&root]).unwrap();
        let got: Vec<_> = p.paths.iter().map(|e| e.path.to_str().unwrap()).collect();
        assert_eq!(got, ["/boot", "/boot/efi", "/mnt/nas/share"]);
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn a_path_inside_a_selected_subvolume_is_rejected() {
        let root = sel("/");
        for bad in ["/etc", "/var/lib/x"] {
            let e = run(&[bad], &[&root]).unwrap_err().to_string();
            assert!(e.contains("already") && e.contains("snapshot"), "{e}");
        }
    }

    #[test]
    fn a_selected_subvolume_itself_is_rejected() {
        let home = sel("/home");
        let e = run(&["/home"], &[&home]).unwrap_err().to_string();
        assert!(e.contains("already selected"), "{e}");
    }

    #[test]
    fn btrfs_that_is_not_selected_only_warns() {
        let root = sel("/");
        let p = run(&["/data/projects"], &[&root]).unwrap();
        assert_eq!(p.paths.len(), 1);
        assert!(p.warnings[0].contains("/data") && p.warnings[0].contains("consistent snapshot"));
    }

    #[test]
    fn rejects_relative_dotdot_and_non_directories() {
        let root = sel("/");
        assert!(run(&["boot"], &[&root]).is_err());
        assert!(run(&["/mnt/../etc"], &[&root]).is_err());
        let mounts = mountinfo::parse(MOUNTS);
        let e = plan(&[PathBuf::from("/boot/file")], &mounts, &[&root], |_| false).unwrap_err();
        assert!(e.to_string().contains("not a directory"));
    }

    #[test]
    fn duplicates_are_collapsed() {
        let root = sel("/");
        let p = run(&["/boot", "/boot/"], &[&root]).unwrap();
        assert_eq!(p.paths.len(), 1);
    }
}
