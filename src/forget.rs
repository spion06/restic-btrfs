//! `forget`: apply repository-side retention to what rbtrfs wrote.
//!
//! Two kinds of snapshots live in the repository (see docs/architecture.md):
//!
//! - **merged** snapshots (label `rbtrfs`) are the user-facing backups. They are
//!   thinned by the profile's `[retention]` policy, per host. Only the profile's own
//!   snapshots are considered (see [`PROFILE_TAG_PREFIX`]).
//! - **part** snapshots (tag `rbtrfs:part`) only exist so the next run finds a
//!   parent. Only those of the newest run are needed; the rest are forgotten.
//!
//! Snapshots rbtrfs did not create (other labels) are never touched. The command
//! holds the same host-wide lock as `backup`, so it cannot overlap one.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use rustic_core::repofile::{SnapshotFile, SnapshotId};
use rustic_core::PruneOptions;

use crate::config::Profile;
use crate::repo::RepoHandle;
use crate::restore::PART_TAG;

const MERGED_LABEL: &str = "rbtrfs";
const RUN_TAG_PREFIX: &str = "rbtrfs:run=";
/// Tag naming the profile that made a snapshot. Snapshots from before it existed
/// belong to the profile called `default`.
pub const PROFILE_TAG_PREFIX: &str = "rbtrfs:profile=";

fn profile_of(s: &SnapshotFile) -> &str {
    s.tags
        .iter()
        .find_map(|t| t.strip_prefix(PROFILE_TAG_PREFIX))
        .unwrap_or("default")
}

fn run_of(s: &SnapshotFile) -> Option<String> {
    s.tags
        .iter()
        .find_map(|t| t.strip_prefix(RUN_TAG_PREFIX).map(str::to_string))
}

fn is_part(s: &SnapshotFile) -> bool {
    s.tags.iter().any(|t| t == PART_TAG)
}

/// What to do with the repository's rbtrfs snapshots.
#[derive(Debug, Default)]
pub struct Plan {
    /// Merged snapshots to forget, with the reason they are not kept.
    pub forget_merged: Vec<SnapshotId>,
    /// Part snapshots no future run can use as a parent.
    pub forget_parts: Vec<SnapshotId>,
    pub kept_merged: usize,
    pub kept_parts: usize,
}

pub fn plan(
    snaps: Vec<SnapshotFile>,
    keep: &rustic_core::KeepOptions,
    profile: &str,
) -> Result<Plan> {
    let now = SnapshotFile::default().time;
    let mut plan = Plan::default();

    // merged snapshots: restic-style retention, per host
    let mut merged: BTreeMap<String, Vec<SnapshotFile>> = BTreeMap::new();
    let mut parts: BTreeMap<String, Vec<SnapshotFile>> = BTreeMap::new();
    for s in snaps.into_iter().filter(|s| profile_of(s) == profile) {
        if is_part(&s) {
            if s.label.starts_with("rbtrfs-part:") {
                parts.entry(s.hostname.clone()).or_default().push(s);
            }
        } else if s.label == MERGED_LABEL {
            merged.entry(s.hostname.clone()).or_default().push(s);
        }
    }

    let mut newest_run: BTreeMap<String, String> = BTreeMap::new();
    for (host, group) in merged {
        if let Some(run) = group.iter().filter_map(run_of).max() {
            newest_run.insert(host.clone(), run);
        }
        for f in keep.apply(group, &now).context("applying retention")? {
            if f.keep {
                plan.kept_merged += 1;
            } else {
                plan.forget_merged.push(f.snapshot.id);
            }
        }
    }

    // parts: keep those from the newest merged run onwards (a newer run whose
    // merge failed still leaves useful parents); with no merged run yet, keep all.
    for (host, group) in parts {
        let floor = newest_run.get(&host);
        for s in group {
            let needed = match (floor, run_of(&s)) {
                (Some(floor), Some(run)) => &run >= floor,
                _ => true,
            };
            if needed {
                plan.kept_parts += 1;
            } else {
                plan.forget_parts.push(s.id);
            }
        }
    }
    Ok(plan)
}

pub fn run(profile: &Profile, prune: bool, instant_delete: bool, dry_run: bool) -> Result<()> {
    let Some(retention) = profile.retention.as_ref().filter(|r| !r.is_empty()) else {
        bail!("profile has no [retention] policy; refusing to forget anything without one");
    };
    let keep = retention.to_keep_options()?;

    let handle = RepoHandle::from_profile(profile)?;
    let repo = handle.open()?;
    let snaps = repo.get_all_snapshots().context("listing snapshots")?;
    let plan = plan(snaps, &keep, &profile.name)?;

    println!(
        "merged: keep {}, forget {}; parts: keep {}, forget {}",
        plan.kept_merged,
        plan.forget_merged.len(),
        plan.kept_parts,
        plan.forget_parts.len()
    );
    if dry_run {
        for id in plan.forget_merged.iter().chain(&plan.forget_parts) {
            println!("  would forget {id}");
        }
        return Ok(());
    }

    let ids: Vec<SnapshotId> = plan
        .forget_merged
        .into_iter()
        .chain(plan.forget_parts)
        .collect();
    if !ids.is_empty() {
        repo.delete_snapshots(&ids).context("removing snapshots")?;
    }
    if prune {
        if instant_delete {
            eprintln!(
                "rbtrfs: warning: --instant-delete skips rustic's two-phase pruning; make sure no \
                 other process (backup, restic, rustic) is using this repository"
            );
        }
        let opts = PruneOptions::default().instant_delete(instant_delete);
        let prune_plan = repo.prune_plan(&opts).context("planning prune")?;
        repo.prune(&opts, prune_plan).context("pruning")?;
        println!("pruned");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Retention;
    use rustic_core::SnapshotOptions;

    /// A snapshot `hours_ago` hours old.
    fn snap(host: &str, label: &str, run: &str, part: bool, hours_ago: i64) -> SnapshotFile {
        let mut o = SnapshotOptions::default()
            .label(label.to_string())
            .add_tags(&format!("{RUN_TAG_PREFIX}{run}"))
            .unwrap();
        if part {
            o = o.add_tags(PART_TAG).unwrap();
        }
        let mut s = o.to_snapshot().unwrap();
        s.hostname = host.into();
        // unsaved snapshots all carry the zero id; make them distinguishable
        s.id = rustic_core::Id::random().into();
        s.time = s
            .time
            .checked_sub(jiff::Span::new().hours(hours_ago))
            .unwrap();
        s
    }

    fn keep_last(n: u32) -> rustic_core::KeepOptions {
        Retention {
            keep_last: Some(n),
            ..Default::default()
        }
        .to_keep_options()
        .unwrap()
    }

    /// Three runs for host `h`: each a merged snapshot plus parts for `a` and `b`.
    fn three_runs(host: &str) -> Vec<SnapshotFile> {
        let mut v = Vec::new();
        for (run, ago) in [
            ("20260101T000000Z", 48),
            ("20260102T000000Z", 24),
            ("20260103T000000Z", 0),
        ] {
            v.push(snap(host, "rbtrfs", run, false, ago));
            v.push(snap(host, "rbtrfs-part:a", run, true, ago));
            v.push(snap(host, "rbtrfs-part:b", run, true, ago));
        }
        v
    }

    #[test]
    fn keeps_newest_merged_and_only_the_newest_runs_parts() {
        let snaps = three_runs("h");
        let p = plan(snaps.clone(), &keep_last(2), "default").unwrap();
        assert_eq!((p.kept_merged, p.forget_merged.len()), (2, 1));
        assert_eq!((p.kept_parts, p.forget_parts.len()), (2, 4));
        // the forgotten merged snapshot is the oldest one
        let oldest = snaps
            .iter()
            .find(|s| run_of(s).as_deref() == Some("20260101T000000Z") && !is_part(s))
            .unwrap();
        assert_eq!(p.forget_merged, vec![oldest.id]);
    }

    #[test]
    fn hosts_are_thinned_independently() {
        let mut snaps = three_runs("h1");
        snaps.extend(three_runs("h2"));
        let p = plan(snaps, &keep_last(1), "default").unwrap();
        assert_eq!(p.kept_merged, 2);
        assert_eq!(p.forget_merged.len(), 4);
        assert_eq!(p.kept_parts, 4);
    }

    #[test]
    fn foreign_snapshots_are_never_touched() {
        let mut snaps = three_runs("h");
        snaps.push(snap("h", "my-own-restic-job", "x", false, 500));
        snaps.push(snap("h", "other", "x", true, 500));
        let p = plan(snaps.clone(), &keep_last(1), "default").unwrap();
        let foreign: Vec<_> = snaps
            .iter()
            .filter(|s| s.label == "my-own-restic-job" || s.label == "other")
            .map(|s| s.id)
            .collect();
        assert!(foreign
            .iter()
            .all(|id| !p.forget_merged.contains(id) && !p.forget_parts.contains(id)));
    }

    #[test]
    fn parts_of_a_newer_unmerged_run_are_kept() {
        let mut snaps = three_runs("h");
        // a 4th run died before its merge: its parts are the best parents
        snaps.push(snap("h", "rbtrfs-part:a", "20260104T000000Z", true, 0));
        let p = plan(snaps, &keep_last(5), "default").unwrap();
        assert_eq!(
            p.kept_parts, 3,
            "newest run's two parts + the unmerged run's part"
        );
    }

    #[test]
    fn no_merged_yet_keeps_all_parts() {
        let snaps = vec![snap("h", "rbtrfs-part:a", "20260101T000000Z", true, 0)];
        let p = plan(snaps, &keep_last(1), "default").unwrap();
        assert!(p.forget_parts.is_empty());
    }

    #[test]
    fn profiles_sharing_a_repository_are_thinned_separately() {
        let tagged = |profile: &str, run: &str, ago: i64| {
            let mut s = snap("h", "rbtrfs", run, false, ago);
            s.tags.add(format!("{PROFILE_TAG_PREFIX}{profile}"));
            s
        };
        let snaps = vec![
            tagged("work", "20260101T000000Z", 48),
            tagged("work", "20260102T000000Z", 24),
            tagged("games", "20260101T000000Z", 48),
            // from before profile tags: belongs to "default"
            snap("h", "rbtrfs", "20260101T000000Z", false, 48),
        ];
        let work = plan(snaps.clone(), &keep_last(1), "work").unwrap();
        assert_eq!((work.kept_merged, work.forget_merged.len()), (1, 1));
        let games = plan(snaps.clone(), &keep_last(1), "games").unwrap();
        assert_eq!((games.kept_merged, games.forget_merged.len()), (1, 0));
        let default = plan(snaps, &keep_last(1), "default").unwrap();
        assert_eq!((default.kept_merged, default.forget_merged.len()), (1, 0));
    }
}
