//! Resolve configured subvolume patterns against currently-mounted btrfs subvolumes.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::discover::{BtrfsFilesystem, BtrfsMount};

/// A source subvolume chosen for backup.
#[derive(Debug, Clone)]
pub struct Selected {
    /// Host mount point, recorded verbatim in the restic snapshot (`/home`).
    pub mount_point: PathBuf,
    /// Subvolume path within the filesystem (`/@home`).
    pub subvol: String,
    /// Stable key derived from the mount point (`home`, `var-log`); see [`key_for`].
    pub key: String,
}

/// A filesystem plus the subvolumes selected from it.
#[derive(Debug)]
pub struct FilesystemSelection<'a> {
    pub fs: &'a BtrfsFilesystem,
    pub selected: Vec<Selected>,
    /// Mounted subvolumes nested under a selection but not themselves selected.
    pub nested_unselected: Vec<PathBuf>,
}

/// Result of matching the configured patterns.
#[derive(Debug)]
pub struct Resolution<'a> {
    /// One entry per filesystem with at least one selected subvolume.
    pub selections: Vec<FilesystemSelection<'a>>,
    /// Matched mounts that were left out, and why (bind mounts, duplicates).
    pub warnings: Vec<String>,
}

/// Directory-name-safe key for a mount point: `/` becomes `-`, and literal `-`
/// and `%` are percent-escaped so distinct mount points never share a key
/// (`/var/log` -> `var-log`, `/var-log` -> `var%2dlog`). The root filesystem is
/// the one reserved key, `rootfs`; a mount literally named `/rootfs` escapes to
/// `rootfs` too, so it is disambiguated with a trailing `%2f`.
pub fn key_for(mount_point: &Path) -> String {
    let s = mount_point.to_string_lossy();
    let trimmed = s.trim_matches('/');
    if trimmed.is_empty() {
        return "rootfs".to_string();
    }
    let mut key = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        match c {
            '/' => key.push('-'),
            '-' => key.push_str("%2d"),
            '%' => key.push_str("%25"),
            c => key.push(c),
        }
    }
    if key == "rootfs" {
        key.push_str("%2f");
    }
    key
}

/// Match `patterns` (exact paths or globs) against every mounted subvolume across
/// `filesystems`, returning one [`FilesystemSelection`] per filesystem that has
/// at least one match.
pub fn resolve<'a>(
    filesystems: &'a [BtrfsFilesystem],
    patterns: &[String],
) -> Result<Resolution<'a>> {
    let matchers = patterns
        .iter()
        .map(|p| {
            glob::Pattern::new(p)
                .map(|pat| (p.clone(), pat))
                .map_err(|e| anyhow::anyhow!("bad subvolume pattern {p:?}: {e}"))
        })
        .collect::<Result<Vec<_>>>()?;

    let matches = |m: &BtrfsMount| {
        matchers.iter().any(|(raw, pat)| {
            m.mount_point.as_path() == Path::new(raw) || pat.matches_path(&m.mount_point)
        })
    };

    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let mut total = 0;
    for fs in filesystems {
        let mut selected: Vec<Selected> = Vec::new();
        for m in fs.mounts.iter().filter(|m| matches(m)) {
            if !m.is_subvol_root() {
                warnings.push(format!(
                    "{} is a bind mount of a subdirectory ({}) of subvolume {}, not a whole \
                     subvolume; skipped. Select the subvolume's own mount point instead",
                    m.mount_point.display(),
                    m.root.display(),
                    m.subvol
                ));
                continue;
            }
            // Mounts are sorted by mount point, so the first mount of a subvolume wins.
            if let Some(first) = selected.iter().find(|s| s.subvol == m.subvol) {
                warnings.push(format!(
                    "{} is the same subvolume ({}) as {}; recorded once, at {}",
                    m.mount_point.display(),
                    m.subvol,
                    first.mount_point.display(),
                    first.mount_point.display()
                ));
                continue;
            }
            selected.push(Selected {
                mount_point: m.mount_point.clone(),
                subvol: m.subvol.clone(),
                key: key_for(&m.mount_point),
            });
        }
        if selected.is_empty() {
            continue;
        }
        total += selected.len();
        let roots: Vec<PathBuf> = selected.iter().map(|s| s.mount_point.clone()).collect();
        let nested_unselected = fs
            .nested_unselected(&roots)
            .into_iter()
            .filter(|m| m.is_subvol_root())
            .map(|m| m.mount_point.clone())
            .collect();
        out.push(FilesystemSelection {
            fs,
            selected,
            nested_unselected,
        });
    }
    if total == 0 {
        let detail = if warnings.is_empty() {
            String::new()
        } else {
            format!(" ({})", warnings.join("; "))
        };
        bail!("no mounted btrfs subvolume matched any of: {patterns:?}{detail}");
    }
    Ok(Resolution {
        selections: out,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::from_mountinfo;
    use crate::mountinfo;

    const SAMPLE: &str = "\
40 1 0:34 /@ / rw - btrfs /dev/nvme0n1p2 rw,subvolid=256,subvol=/@
41 40 0:34 /@home /home rw - btrfs /dev/nvme0n1p2 rw,subvolid=257,subvol=/@home
42 40 0:34 /@home /mnt/home-again rw - btrfs /dev/nvme0n1p2 rw,subvolid=257,subvol=/@home
43 40 0:34 /@srv/data /srv/data rw - btrfs /dev/nvme0n1p2 rw,subvolid=259,subvol=/@srv
44 40 0:34 /@srv /srv rw - btrfs /dev/nvme0n1p2 rw,subvolid=259,subvol=/@srv
45 41 0:34 /@vm /home/vm rw - btrfs /dev/nvme0n1p2 rw,subvolid=270,subvol=/@vm";

    fn fss() -> Vec<BtrfsFilesystem> {
        from_mountinfo(&mountinfo::parse(SAMPLE))
    }

    fn pats(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keys_do_not_collide() {
        assert_eq!(key_for(Path::new("/var/log")), "var-log");
        assert_eq!(key_for(Path::new("/var-log")), "var%2dlog");
        assert_ne!(
            key_for(Path::new("/var/log")),
            key_for(Path::new("/var-log"))
        );
        assert_eq!(key_for(Path::new("/")), "rootfs");
        assert_ne!(key_for(Path::new("/")), key_for(Path::new("/rootfs")));
        assert_ne!(key_for(Path::new("/a%2db")), key_for(Path::new("/a-b")));
    }

    #[test]
    fn exact_and_glob_patterns() {
        let f = fss();
        let r = resolve(&f, &pats(&["/home", "/srv"])).unwrap();
        let keys: Vec<_> = r.selections[0]
            .selected
            .iter()
            .map(|s| s.key.as_str())
            .collect();
        assert_eq!(keys, ["home", "srv"]);
        let r = resolve(&f, &pats(&["/home/*"])).unwrap();
        assert_eq!(
            r.selections[0].selected[0].mount_point,
            PathBuf::from("/home/vm")
        );
    }

    #[test]
    fn bind_mounts_are_skipped_with_a_warning() {
        let f = fss();
        let r = resolve(&f, &pats(&["/srv/data", "/srv"])).unwrap();
        let mps: Vec<_> = r.selections[0]
            .selected
            .iter()
            .map(|s| s.mount_point.clone())
            .collect();
        assert_eq!(mps, [PathBuf::from("/srv")]);
        assert!(r
            .warnings
            .iter()
            .any(|w| w.contains("/srv/data") && w.contains("bind mount")));
    }

    #[test]
    fn same_subvolume_mounted_twice_is_recorded_once() {
        let f = fss();
        let r = resolve(&f, &pats(&["/home", "/mnt/home-again"])).unwrap();
        assert_eq!(r.selections[0].selected.len(), 1);
        assert_eq!(
            r.selections[0].selected[0].mount_point,
            PathBuf::from("/home")
        );
        assert!(r.warnings.iter().any(|w| w.contains("/mnt/home-again")));
    }

    #[test]
    fn nested_mounted_subvolume_is_flagged() {
        let f = fss();
        let r = resolve(&f, &pats(&["/home"])).unwrap();
        assert_eq!(
            r.selections[0].nested_unselected,
            [PathBuf::from("/home/vm")]
        );
    }

    #[test]
    fn no_match_is_an_error() {
        assert!(resolve(&fss(), &pats(&["/nope"])).is_err());
    }
}
