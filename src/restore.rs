//! `snapshots` (list) and `restore`.

use std::path::Path;

use anyhow::{Context, Result};
use rustic_core::{LsOptions, RestoreOptions};
use rustic_core::LocalDestination;

use crate::config::Profile;
use crate::repo::RepoHandle;

const PART_TAG: &str = "rbtrfs:part";

pub fn list(profile: &Profile, all: bool) -> Result<()> {
    let handle = RepoHandle::from_profile(profile)?;
    let repo = handle.open()?;
    let mut snaps = repo.get_all_snapshots().context("listing snapshots")?;
    snaps.sort_by(|a, b| a.time.cmp(&b.time));

    println!("{:<10}  {:<20}  {:<8}  {:<16}  TAGS / PATHS", "ID", "TIME", "KIND", "HOST");
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

pub fn restore(
    profile: &Profile,
    snapshot: &str,
    subvol: &Path,
    target: &Path,
    host: &HostFilter,
) -> Result<()> {
    let handle = RepoHandle::from_profile(profile)?;
    let repo = handle.open()?.to_indexed().context("indexing repository")?;

    // Merged snapshots record subvolumes at their real mount path; the tree path
    // is that path without the leading slash (`/var/log` -> `var/log`).
    let tree_path = subvol.to_string_lossy();
    let tree_path = tree_path.trim_start_matches('/');
    let snap_path = format!("{snapshot}:{tree_path}");

    // `latest` must mean "newest merged snapshot": a part snapshot ties with its
    // merged snapshot on time and holds only one subvolume. An explicit id is
    // taken as given (so a part can still be restored by id).
    let want_host = match host {
        HostFilter::ThisHost => Some(crate::hostname()),
        HostFilter::Named(h) => Some(h.clone()),
        HostFilter::Any => None,
    };
    let node = repo
        .node_from_snapshot_path(&snap_path, |s| {
            snapshot != "latest"
                || (!s.tags.iter().any(|t| t == PART_TAG)
                    && want_host.as_ref().is_none_or(|h| &s.hostname == h))
        })
        .with_context(|| format!("locating {tree_path} in snapshot {snapshot}"))?;

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
        .context("restoring")?;

    println!("restored {} from {snapshot} to {}", subvol.display(), target.display());
    Ok(())
}
