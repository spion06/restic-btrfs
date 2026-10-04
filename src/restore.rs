//! `snapshots` (list), `ls`, `dump` and `restore`.

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rustic_core::repofile::Node;
use rustic_core::LocalDestination;
use rustic_core::{IndexedTree, LsOptions, Repository, RestoreOptions};

use crate::config::Profile;
use crate::repo::RepoHandle;

pub const PART_TAG: &str = "rbtrfs:part";

pub fn list(profile: &Profile, all: bool) -> Result<()> {
    let handle = RepoHandle::from_profile(profile)?;
    let repo = handle.open()?;
    let mut snaps = repo.get_all_snapshots().context("listing snapshots")?;
    snaps.sort_by(|a, b| a.time.cmp(&b.time));

    println!(
        "{:<10}  {:<20}  {:<8}  {:<16}  TAGS / PATHS",
        "ID", "TIME", "KIND", "HOST"
    );
    for s in &snaps {
        let is_part = s.tags.iter().any(|t| t == PART_TAG);
        if is_part && !all {
            continue;
        }
        let tags: Vec<String> = s.tags.iter().map(|t| t.to_string()).collect();
        println!(
            "{:<10}  {:<20}  {:<8}  {:<16}  {}",
            s.id.to_string().chars().take(8).collect::<String>(),
            s.time.strftime("%Y-%m-%d %H:%M:%S").to_string(),
            if is_part { "part" } else { "merged" },
            s.hostname,
            tags.join(",")
        );
        for p in s.paths.iter() {
            println!("{:62}{}", "", p);
        }
    }
    Ok(())
}

/// Which snapshots `latest` may resolve to.
#[derive(Debug, Clone)]
pub enum HostFilter {
    /// Only snapshots taken on this host (the default).
    ThisHost,
    Named(String),
    Any,
}

/// Find the tree node at `path` (a recorded path such as `/var/log`, or `/` for
/// everything) in `snapshot`.
///
/// Merged snapshots record subvolumes at their real mount path; the tree path is
/// that path without the leading slash (`/var/log` -> `var/log`).
///
/// `latest` must mean "newest merged snapshot": a part snapshot ties with its
/// merged snapshot on time and holds only one subvolume. An explicit id is taken
/// as given (so a part can still be restored by id).
fn locate<S: IndexedTree>(
    repo: &Repository<S>,
    snapshot: &str,
    path: &Path,
    host: &HostFilter,
) -> Result<Node> {
    let tree_path = path.to_string_lossy();
    let tree_path = tree_path.trim_matches('/');
    let want_host = match host {
        HostFilter::ThisHost => Some(crate::hostname()),
        HostFilter::Named(h) => Some(h.clone()),
        HostFilter::Any => None,
    };
    repo.node_from_snapshot_path(&format!("{snapshot}:{tree_path}"), |s| {
        snapshot != "latest"
            || (!s.tags.iter().any(|t| t == PART_TAG)
                && want_host.as_ref().is_none_or(|h| &s.hostname == h))
    })
    .with_context(|| format!("locating /{tree_path} in snapshot {snapshot}"))
}

/// `ls`: print the entries below `path` in a snapshot.
pub fn ls(profile: &Profile, snapshot: &str, path: &Path, host: &HostFilter) -> Result<()> {
    let handle = RepoHandle::from_profile(profile)?;
    let repo = handle.open()?.to_indexed().context("indexing repository")?;
    let node = locate(&repo, snapshot, path, host)?;
    let base = path.to_string_lossy().trim_matches('/').to_string();

    if !node.is_dir() {
        println!("{} {:>12} /{base}", kind(&node), node.meta.size);
        return Ok(());
    }
    let mut out = std::io::stdout().lock();
    for entry in repo.ls(&node, &LsOptions::default()).context("listing")? {
        let (p, n) = entry.context("reading tree")?;
        let rel = p.to_string_lossy();
        if rel.is_empty() {
            continue;
        }
        let full = if base.is_empty() {
            format!("/{rel}")
        } else {
            format!("/{base}/{rel}")
        };
        writeln!(out, "{} {:>12} {full}", kind(&n), n.meta.size)?;
    }
    Ok(())
}

fn kind(n: &Node) -> char {
    if n.is_dir() {
        'd'
    } else if n.is_symlink() {
        'l'
    } else if n.is_file() {
        '-'
    } else {
        '?'
    }
}

/// `dump`: write one file's contents to stdout.
pub fn dump(profile: &Profile, snapshot: &str, path: &Path, host: &HostFilter) -> Result<()> {
    let handle = RepoHandle::from_profile(profile)?;
    let repo = handle.open()?.to_indexed().context("indexing repository")?;
    let node = locate(&repo, snapshot, path, host)?;
    if !node.is_file() {
        bail!(
            "{} is not a regular file; use `ls` or `restore`",
            path.display()
        );
    }
    let mut out = std::io::stdout().lock();
    repo.dump(&node, &mut out).context("dumping file")?;
    out.flush()?;
    Ok(())
}

/// `restore`: write `subvol` of `snapshot` into `target`. With `as_subvolume`,
/// `target` is first created as a new btrfs subvolume (root, and `target` must be
/// on btrfs and not exist yet), so the subvolume boundary survives the restore.
/// Subvolumes that were nested inside it are restored as plain directories.
pub fn restore(
    profile: &Profile,
    snapshot: &str,
    subvol: &Path,
    target: &Path,
    host: &HostFilter,
    as_subvolume: bool,
) -> Result<()> {
    use crate::btrfs::{BtrfsOps, LibBtrfsUtil};

    let handle = RepoHandle::from_profile(profile)?;
    let repo = handle.open()?.to_indexed().context("indexing repository")?;
    let node = locate(&repo, snapshot, subvol, host)?;

    let created = if as_subvolume {
        if !node.is_dir() {
            bail!(
                "--as-subvolume needs a directory to restore, {} is not one",
                subvol.display()
            );
        }
        if !crate::is_root() {
            bail!("--as-subvolume needs root");
        }
        if target.exists() {
            bail!("--as-subvolume: {} already exists", target.display());
        }
        LibBtrfsUtil.create_subvolume(target)?;
        true
    } else {
        false
    };

    let result = (|| -> Result<()> {
        let streamer = repo
            .ls(&node, &LsOptions::default())
            .context("streaming snapshot contents")?;
        let dest = LocalDestination::new(&target.to_string_lossy(), true, !node.is_dir())
            .with_context(|| format!("preparing destination {}", target.display()))?;
        let opts = RestoreOptions::default();
        let plan = repo
            .prepare_restore(&opts, streamer.clone(), &dest, false)
            .context("preparing restore")?;
        repo.restore(plan, &opts, streamer, &dest)
            .context("restoring")
    })();

    if let Err(e) = result {
        if created {
            // Don't leave a half-restored subvolume we created ourselves behind.
            let _ = LibBtrfsUtil.delete_subvolume(target);
        }
        return Err(e);
    }
    println!(
        "restored {} from {snapshot} to {}",
        subvol.display(),
        target.display()
    );
    Ok(())
}
