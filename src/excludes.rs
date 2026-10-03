//! Translate user `exclude` patterns into the glob form rustic_core wants.
//!
//! rustic_core feeds `Excludes::globs` to the `ignore` crate's override matcher,
//! where a bare glob *whitelists* (only matches are kept) and a `!` prefix
//! excludes. Matching also happens against the path the data is *read* from (the
//! staging snapshot), not the path it is *recorded* as. This module hides both
//! quirks so patterns behave like restic's `--exclude`:
//!
//! - A pattern starting with `/` is an absolute path as recorded, e.g.
//!   `/home/alice/Downloads`. It applies to the subvolume whose mount point is a
//!   prefix of it, and is re-rooted onto that subvolume's snapshot. Everything
//!   after the mount point may use glob syntax.
//! - Any other pattern matches at any depth, like a restic pattern: `*.tmp`,
//!   `.cache`, `**/node_modules` and `alice/.cache` all work.
//! - A trailing `/` restricts a pattern to directories.
//! - A leading `!` is rejected: patterns are exclusions already.

use std::path::Path;

use anyhow::{bail, Result};

/// Globs (already `!`-prefixed) for the backup of one subvolume.
///
/// `record_path` is the subvolume's mount point as recorded in the repository;
/// `snapshot_root` is where its read-only snapshot is read from.
pub fn translate(
    patterns: &[String],
    record_path: &Path,
    snapshot_root: &Path,
    extra_abs: &[&Path],
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for raw in patterns {
        let p = raw.trim();
        if p.is_empty() {
            continue;
        }
        if p.starts_with('!') {
            bail!("exclude pattern {raw:?}: patterns are exclusions already, drop the leading `!`");
        }
        if p.starts_with('/') {
            let dir_only = p.len() > 1 && p.ends_with('/');
            if let Ok(rel) = Path::new(p).strip_prefix(record_path) {
                let mut g = format!("!{}", escape(&snapshot_root.to_string_lossy()));
                let rel = rel.to_string_lossy();
                if !rel.is_empty() {
                    g.push('/');
                    g.push_str(&rel);
                }
                if dir_only {
                    g.push('/');
                }
                out.push(g);
            }
            // Not under this subvolume: it belongs to another job.
        } else {
            let body = p.trim_end_matches('/');
            let anchored_any = if body.contains('/') && !body.starts_with("**/") {
                format!("**/{body}")
            } else {
                body.to_string()
            };
            out.push(format!("!{anchored_any}{}", if p.ends_with('/') { "/" } else { "" }));
        }
    }
    for abs in extra_abs {
        out.push(format!("!{}", escape(&abs.to_string_lossy())));
    }
    Ok(out)
}

/// Escape glob metacharacters in a literal path.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']' | '{' | '}' | '!') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustic_core::Excludes;

    fn pats(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    /// Would the backup walker skip `path`?
    fn excluded(globs: Vec<String>, path: &str, is_dir: bool) -> bool {
        let ov = Excludes::default().globs(globs).as_override().unwrap();
        ov.matched(Path::new(path), is_dir).is_ignore()
    }

    const ROOT: &str = "/run/rbtrfs/mnt/0_34/.rbtrfs-snapshots/home/20260101T000000Z";

    fn t(p: &[&str]) -> Vec<String> {
        translate(&pats(p), Path::new("/home"), Path::new(ROOT), &[]).unwrap()
    }

    #[test]
    fn bare_glob_would_whitelist_so_we_prefix_bang() {
        assert_eq!(t(&["*.tmp"]), ["!*.tmp"]);
    }

    #[test]
    fn relative_patterns_match_at_any_depth() {
        let g = t(&["**/.cache", "*.tmp", "alice/.cache", "node_modules/"]);
        assert!(excluded(g.clone(), &format!("{ROOT}/alice/.cache"), true));
        assert!(excluded(g.clone(), &format!("{ROOT}/bob/x/.cache"), true));
        assert!(excluded(g.clone(), &format!("{ROOT}/bob/a.tmp"), false));
        assert!(excluded(g.clone(), &format!("{ROOT}/p/node_modules"), true));
        assert!(!excluded(g.clone(), &format!("{ROOT}/p/node_modules"), false));
        assert!(!excluded(g.clone(), &format!("{ROOT}/bob/keep.txt"), false));
        assert!(!excluded(g, &format!("{ROOT}/bob/.config"), true));
    }

    #[test]
    fn absolute_patterns_are_rerooted_onto_the_snapshot() {
        let g = t(&["/home/alice/Downloads", "/home/*/tmp"]);
        assert!(excluded(g.clone(), &format!("{ROOT}/alice/Downloads"), true));
        assert!(excluded(g.clone(), &format!("{ROOT}/bob/tmp"), true));
        assert!(!excluded(g.clone(), &format!("{ROOT}/bob/Downloads"), true));
        assert!(!excluded(g, &format!("{ROOT}/alice/Documents"), true));
    }

    #[test]
    fn absolute_patterns_for_other_subvolumes_are_ignored() {
        assert!(t(&["/srv/data"]).is_empty());
        let root = translate(&pats(&["/home/alice"]), Path::new("/"), Path::new(ROOT), &[]).unwrap();
        assert!(excluded(root, &format!("{ROOT}/home/alice"), true));
    }

    #[test]
    fn snapshot_path_metacharacters_are_escaped() {
        let g = translate(&pats(&["/home/x"]), Path::new("/home"), Path::new("/s/we[ir]d*"), &[]).unwrap();
        assert!(excluded(g.clone(), "/s/we[ir]d*/x", false));
        assert!(!excluded(g, "/s/weid/x", false));
    }

    #[test]
    fn extra_absolute_excludes_are_literal() {
        let g = translate(&[], Path::new("/home"), Path::new(ROOT), &[Path::new(&format!("{ROOT}/.rbtrfs-snapshots"))]).unwrap();
        assert!(excluded(g, &format!("{ROOT}/.rbtrfs-snapshots"), true));
    }

    #[test]
    fn leading_bang_is_rejected() {
        assert!(translate(&pats(&["!x"]), Path::new("/"), Path::new(ROOT), &[]).is_err());
    }
}
