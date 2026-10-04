//! Resolve configured subvolume patterns against currently-mounted btrfs subvolumes.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::config::Subvolumes;
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
    /// Mounts that matched `subvolumes` but were removed by `exclude_subvolumes`.
    pub excluded: Vec<PathBuf>,
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

/// Compile mount-point patterns (exact paths or globs).
fn compile(patterns: &[String], what: &str) -> Result<Vec<(String, glob::Pattern)>> {
    patterns
        .iter()
        .map(|p| {
            glob::Pattern::new(p)
                .map(|pat| (p.clone(), pat))
                .map_err(|e| anyhow::anyhow!("bad {what} pattern {p:?}: {e}"))
        })
        .collect()
}

/// `*` and `?` stop at `/` (`/home/*` is one level), `**` crosses directories.
const MATCH: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

fn any_match(matchers: &[(String, glob::Pattern)], m: &BtrfsMount) -> bool {
    matchers.iter().any(|(raw, pat)| {
        m.mount_point.as_path() == Path::new(raw) || pat.matches_path_with(&m.mount_point, MATCH)
    })
}

/// Match `select` (exact paths, globs, or everything) against every mounted
/// subvolume across `filesystems`, drop those matching `exclude`, and return one
/// [`FilesystemSelection`] per filesystem that has at least one subvolume left.
///
/// With [`Subvolumes::All`] the bind-mount and duplicate-mount notes are
/// suppressed: skipping them is the expected outcome there, not a surprise.
pub fn resolve<'a>(
    filesystems: &'a [BtrfsFilesystem],
    select: &Subvolumes,
    exclude: &[String],
) -> Result<Resolution<'a>> {
    let all = matches!(select, Subvolumes::All);
    let patterns: Vec<String> = match select {
        Subvolumes::All => vec!["/**".to_string()],
        Subvolumes::List(v) => v.clone(),
    };
    let matchers = compile(&patterns, "subvolume")?;
    let excluders = compile(exclude, "exclude_subvolumes")?;
    let matches = |m: &BtrfsMount| any_match(&matchers, m);

    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let mut excluded = Vec::new();
    let mut total = 0;
    for fs in filesystems {
        let mut selected: Vec<Selected> = Vec::new();
        for m in fs.mounts.iter().filter(|m| matches(m)) {
            if any_match(&excluders, m) {
                excluded.push(m.mount_point.clone());
                continue;
            }
            if !m.is_subvol_root() {
                if all {
                    continue;
                }
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
                if all {
                    continue;
                }
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
            .filter(|m| m.is_subvol_root() && !any_match(&excluders, m))
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
        if all {
            bail!("no btrfs subvolume is mounted (or all were excluded by exclude_subvolumes)");
        }
        bail!("no mounted btrfs subvolume matched any of: {patterns:?}{detail}");
    }
    Ok(Resolution {
        selections: out,
        warnings,
        excluded,
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

    fn list(p: &[&str]) -> Subvolumes {
        Subvolumes::List(pats(p))
    }

    #[test]
    fn single_star_does_not_cross_directories() {
        let fss = fss();
        let r = resolve(&fss, &list(&["/*"]), &[]).unwrap();
        let mps: Vec<_> = r
            .selections
            .iter()
            .flat_map(|s| s.selected.iter().map(|m| m.mount_point.clone()))
            .collect();
        assert!(mps.contains(&PathBuf::from("/home")));
        assert!(mps.contains(&PathBuf::from("/srv")));
        assert!(!mps.contains(&PathBuf::from("/home/vm")), "{mps:?}");
        assert!(!mps.contains(&PathBuf::from("/srv/data")), "{mps:?}");
        // `**` still crosses directories
        let r = resolve(&fss, &list(&["/home/**"]), &[]).unwrap();
        assert!(r
            .selections
            .iter()
            .flat_map(|s| &s.selected)
            .any(|m| m.mount_point.as_path() == Path::new("/home/vm")));
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
        let r = resolve(&f, &list(&["/home", "/srv"]), &[]).unwrap();
        let keys: Vec<_> = r.selections[0]
            .selected
            .iter()
            .map(|s| s.key.as_str())
            .collect();
        assert_eq!(keys, ["home", "srv"]);
        let r = resolve(&f, &list(&["/home/*"]), &[]).unwrap();
        assert_eq!(
            r.selections[0].selected[0].mount_point,
            PathBuf::from("/home/vm")
        );
    }

    #[test]
    fn bind_mounts_are_skipped_with_a_warning() {
        let f = fss();
        let r = resolve(&f, &list(&["/srv/data", "/srv"]), &[]).unwrap();
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
        let r = resolve(&f, &list(&["/home", "/mnt/home-again"]), &[]).unwrap();
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
        let r = resolve(&f, &list(&["/home"]), &[]).unwrap();
        assert_eq!(
            r.selections[0].nested_unselected,
            [PathBuf::from("/home/vm")]
        );
    }

    #[test]
    fn all_selects_every_whole_subvolume_quietly() {
        let f = fss();
        let r = resolve(&f, &Subvolumes::All, &[]).unwrap();
        let mps: Vec<_> = r.selections[0]
            .selected
            .iter()
            .map(|s| s.mount_point.to_str().unwrap())
            .collect();
        // the bind mount (/srv/data) and the second mount of @home are skipped
        assert_eq!(mps, ["/", "/home", "/home/vm", "/srv"]);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn exclude_subvolumes_removes_matches_and_is_reported() {
        let f = fss();
        let r = resolve(&f, &Subvolumes::All, &pats(&["/srv", "/home/*"])).unwrap();
        let mps: Vec<_> = r.selections[0]
            .selected
            .iter()
            .map(|s| s.mount_point.to_str().unwrap())
            .collect();
        assert_eq!(mps, ["/", "/home"]);
        assert!(r.excluded.contains(&PathBuf::from("/srv")));
        assert!(r.excluded.contains(&PathBuf::from("/home/vm")));
    }

    #[test]
    fn excluding_a_nested_mount_silences_its_warning() {
        let f = fss();
        let warned = resolve(&f, &list(&["/home"]), &[]).unwrap();
        assert_eq!(
            warned.selections[0].nested_unselected,
            [PathBuf::from("/home/vm")]
        );
        let quiet = resolve(&f, &list(&["/home"]), &pats(&["/home/vm"])).unwrap();
        assert!(quiet.selections[0].nested_unselected.is_empty());
    }

    #[test]
    fn excluding_everything_is_an_error() {
        assert!(resolve(&fss(), &Subvolumes::All, &pats(&["/**"])).is_err());
    }

    #[test]
    fn no_match_is_an_error() {
        assert!(resolve(&fss(), &list(&["/nope"]), &[]).is_err());
    }
}
