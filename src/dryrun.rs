//! What `backup --dry-run` reports about files: walk the live paths with the same
//! exclude matcher the backup uses, and say what would be skipped and what would
//! be stored. Reads metadata only, never file contents.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use rustic_core::Excludes;

/// A path that matched an exclude pattern, with how much it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    pub path: PathBuf,
    pub files: u64,
    pub bytes: u64,
    /// `None` for an `exclude` pattern; otherwise the marker that caused it, such as
    /// `contains CACHEDIR.TAG` or `xattr user.nobackup`.
    pub reason: Option<String>,
}

/// Marker files and extended attributes that exclude what they sit in or on.
#[derive(Debug, Clone, Copy, Default)]
pub struct Markers<'a> {
    pub present: &'a [String],
    pub xattr: &'a [String],
}

impl Markers<'_> {
    /// Why `path` is excluded by a marker, if it is.
    fn reason(&self, path: &Path, is_dir: bool) -> Option<String> {
        if is_dir {
            if let Some(m) = self.present.iter().find(|m| path.join(m).exists()) {
                return Some(format!("contains {m}"));
            }
        }
        self.xattr
            .iter()
            .find(|x| has_xattr(path, x))
            .map(|x| format!("xattr {x}"))
    }
}

fn has_xattr(path: &Path, name: &str) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (Ok(p), Ok(n)) = (
        CString::new(path.as_os_str().as_bytes()),
        CString::new(name),
    ) else {
        return false;
    };
    // SAFETY: valid NUL-terminated strings; a null buffer with size 0 only queries the size.
    unsafe { nix::libc::lgetxattr(p.as_ptr(), n.as_ptr(), std::ptr::null_mut(), 0) >= 0 }
}

#[derive(Debug, Default)]
pub struct Scan {
    /// Files that would be stored, and their total size.
    pub files: u64,
    pub bytes: u64,
    /// Excluded paths (a directory counts once, with everything below it), largest first.
    pub excluded: Vec<Excluded>,
    /// Directories that could not be read (run as root to see everything).
    pub unreadable: u64,
}

impl Scan {
    pub fn excluded_bytes(&self) -> u64 {
        self.excluded.iter().map(|e| e.bytes).sum()
    }
}

/// Walk `root`, applying `globs` (already in rustic's form, see `excludes::translate`).
/// Other filesystems and nested subvolumes below `root` are not entered, matching what
/// a backup of `root` would store.
pub fn scan(root: &Path, globs: Vec<String>, markers: &Markers<'_>) -> Result<Scan> {
    let matcher = Excludes::default()
        .globs(globs)
        .as_override()
        .map_err(|e| anyhow!("invalid exclude patterns: {e}"))?;
    let root_dev = std::fs::symlink_metadata(root)?.dev();

    let mut out = Scan::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            out.unreadable += 1;
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(md) = entry.metadata() else { continue }; // does not follow symlinks
            let reason = if matcher.matched(&path, md.is_dir()).is_ignore() {
                Some(None)
            } else {
                markers.reason(&path, md.is_dir()).map(Some)
            };
            if let Some(reason) = reason {
                let (files, bytes) = measure(&path, &md, &mut out.unreadable);
                out.excluded.push(Excluded {
                    path,
                    files,
                    bytes,
                    reason,
                });
            } else if md.is_dir() {
                if md.dev() == root_dev {
                    stack.push(path);
                }
            } else {
                out.files += 1;
                out.bytes += md.len();
            }
        }
    }
    out.excluded
        .sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.path.cmp(&b.path)));
    Ok(out)
}

/// Number of files and apparent bytes at `path` (a file, or everything below a directory).
fn measure(path: &Path, md: &std::fs::Metadata, unreadable: &mut u64) -> (u64, u64) {
    if !md.is_dir() {
        return (1, md.len());
    }
    let dev = md.dev();
    let (mut files, mut bytes) = (0, 0);
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            *unreadable += 1;
            continue;
        };
        for e in entries.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                if m.dev() == dev {
                    stack.push(e.path());
                }
            } else {
                files += 1;
                bytes += m.len();
            }
        }
    }
    (files, bytes)
}

/// `1.5 GB`, `340 MB`, `12 kB`.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::excludes::translate;
    use std::fs;

    fn write(root: &Path, rel: &str, len: usize) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'x'; len]).unwrap();
    }

    fn globs(root: &Path, patterns: &[&str]) -> Vec<String> {
        let pats: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        translate(&pats, Path::new("/data"), root, &[]).unwrap()
    }

    #[test]
    fn reports_included_and_excluded_with_sizes() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        write(r, "keep.txt", 10);
        write(r, "docs/readme.md", 20);
        write(r, "alice/.cache/blob", 1000);
        write(r, "alice/.cache/sub/blob2", 500);
        write(r, "proj/target/debug/big", 4000);
        write(r, "proj/src/main.rs", 30);
        write(r, "skip.tmp", 7);

        let s = scan(
            r,
            globs(r, &["*.tmp", ".cache", "/data/proj/target"]),
            &Markers::default(),
        )
        .unwrap();
        assert_eq!((s.files, s.bytes), (3, 60), "keep.txt, readme.md, main.rs");
        let got: Vec<(String, u64, u64)> = s
            .excluded
            .iter()
            .map(|e| {
                (
                    e.path
                        .strip_prefix(r)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    e.files,
                    e.bytes,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("proj/target".to_string(), 1, 4000),
                ("alice/.cache".to_string(), 2, 1500),
                ("skip.tmp".to_string(), 1, 7),
            ],
            "largest first, a directory counted once with everything below it"
        );
        assert_eq!(s.excluded_bytes(), 5507);
        assert_eq!(s.unreadable, 0);
    }

    #[test]
    fn marker_files_exclude_their_directory_and_say_why() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        write(r, "keep/f", 10);
        write(r, "target/CACHEDIR.TAG", 5);
        write(r, "target/debug/big", 900);
        write(r, "work/.nobackup", 1);
        write(r, "work/data", 100);
        let present = vec!["CACHEDIR.TAG".to_string(), ".nobackup".to_string()];
        let s = scan(
            r,
            vec![],
            &Markers {
                present: &present,
                xattr: &[],
            },
        )
        .unwrap();
        assert_eq!((s.files, s.bytes), (1, 10), "only keep/f remains");
        let got: Vec<(String, Option<String>)> = s
            .excluded
            .iter()
            .map(|e| {
                (
                    e.path
                        .strip_prefix(r)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    e.reason.clone(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "target".to_string(),
                    Some("contains CACHEDIR.TAG".to_string())
                ),
                ("work".to_string(), Some("contains .nobackup".to_string())),
            ]
        );
        // no markers configured: everything is counted
        assert_eq!(scan(r, vec![], &Markers::default()).unwrap().files, 5);
    }

    #[test]
    fn xattr_markers_exclude_files_and_directories() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        write(r, "plain", 3);
        write(r, "tagged/inside", 50);
        write(r, "tagged_file", 7);
        let set = |p: &Path| {
            use std::os::unix::ffi::OsStrExt;
            let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
            let n = std::ffi::CString::new("user.nobackup").unwrap();
            // SAFETY: valid NUL-terminated strings and a one-byte value.
            unsafe { nix::libc::lsetxattr(c.as_ptr(), n.as_ptr(), b"1".as_ptr().cast(), 1, 0) == 0 }
        };
        if !(set(&r.join("tagged")) && set(&r.join("tagged_file"))) {
            eprintln!("skipping: this filesystem does not support user xattrs");
            return;
        }
        let xattr = vec!["user.nobackup".to_string()];
        let s = scan(
            r,
            vec![],
            &Markers {
                present: &[],
                xattr: &xattr,
            },
        )
        .unwrap();
        assert_eq!((s.files, s.bytes), (1, 3));
        assert!(s
            .excluded
            .iter()
            .all(|e| e.reason.as_deref() == Some("xattr user.nobackup")));
        assert_eq!(s.excluded.len(), 2);
    }

    #[test]
    fn nothing_excluded_means_everything_is_counted() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), "a/b/c.txt", 5);
        let s = scan(t.path(), vec![], &Markers::default()).unwrap();
        assert_eq!((s.files, s.bytes, s.excluded.len()), (1, 5, 0));
    }

    #[test]
    fn symlinks_are_not_followed() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), "real/file", 100);
        std::os::unix::fs::symlink(t.path().join("real"), t.path().join("link")).unwrap();
        let s = scan(t.path(), vec![], &Markers::default()).unwrap();
        assert_eq!(
            s.files, 2,
            "the file plus the symlink itself, not its target twice"
        );
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1500), "1.5 kB");
        assert_eq!(human(61_300_000_000), "61.3 GB");
    }
}
