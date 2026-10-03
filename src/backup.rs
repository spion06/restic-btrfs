//! The `backup` run: select → stage → burst → per-subvol backup → merge → GC.

use std::cmp::Ordering;
use std::path::Path;

use anyhow::{Context, Result};
use rustic_core::{
    repofile::{Node, SnapshotFile},
    BackupOptions, Excludes, ParentOptions, PathList, SnapshotOptions,
};

use crate::btrfs::{BtrfsOps, LibBtrfsUtil};
use crate::config::Profile;
use crate::discover;
use crate::excludes;
use crate::hooks;
use crate::lock;
use crate::repo::RepoHandle;
use crate::runid;
use crate::select::{self, FilesystemSelection};
use crate::snapshot::{self, GcReport, LocalRetention, SnapJob, StagingArea};

pub struct RunOutcome {
    pub run_id: String,
    pub merged: SnapshotFile,
    pub parts: usize,
    pub gc_deleted: usize,
}

pub fn run(profile: &Profile, dry_run: bool) -> Result<RunOutcome> {
    let run_id = runid::now();
    let filesystems = discover::discover()?;
    let resolution = select::resolve(&filesystems, &profile.subvolumes)?;
    let btrfs = LibBtrfsUtil;

    for w in &resolution.warnings {
        eprintln!("rbtrfs: warning: {w}");
    }
    for sel in &resolution.selections {
        warn_nested(sel, profile, &btrfs);
    }

    if dry_run {
        return dry_run_report(profile, &resolution.selections, run_id);
    }

    let _lock = lock::acquire()?;

    // Prepare staging for every filesystem, then plan every snapshot job, so the
    // burst covers all subvolumes with no I/O interleaved.
    let mut staged: Vec<(StagingArea, Vec<SnapJob>)> = Vec::new();
    for sel in &resolution.selections {
        let area = StagingArea::prepare(sel.fs, profile, &sel.selected)
            .with_context(|| format!("preparing staging for {}", sel.fs.source))?;
        let jobs = area.plan(&sel.selected, &run_id, profile);
        staged.push((area, jobs));
    }
    let all_jobs: Vec<SnapJob> = staged.iter().flat_map(|(_, j)| j.clone()).collect();

    // Fail on a bad repository/password/excludes BEFORE quiescing anything.
    let handle = RepoHandle::from_profile(profile)?;
    handle.open_or_init()?;
    let job_excludes: Vec<Vec<String>> = all_jobs
        .iter()
        .map(|j| job_excludes(profile, j))
        .collect::<Result<_>>()?;

    println!(
        "run {run_id}: {} subvolume(s) across {} filesystem(s)",
        all_jobs.len(),
        staged.len()
    );

    // --- consistency window ---
    // Post-hooks always run, and termination signals are deferred until they have.
    let burst_time = hooks::window(&profile.hooks, || {
        snapshot::burst(&btrfs, &all_jobs).context("snapshot burst")?;
        // The instant the data represents. `SnapshotFile::default()` stamps "now".
        Ok(SnapshotFile::default().time)
    })?;
    println!("snapshotted {} subvolume(s)", all_jobs.len());

    // --- back up each read-only snapshot, recording the real mount point ---
    let mut parts = Vec::with_capacity(all_jobs.len());
    for (job, excl) in all_jobs.iter().zip(job_excludes) {
        // Re-open per part: the in-memory index must see the trees written so far.
        let repo = handle.open()?.to_indexed_ids().context("indexing repository")?;
        let mut snap = SnapshotOptions::default()
            .label(format!("rbtrfs-part:{}", job.key))
            .add_tags("rbtrfs:part")?
            .add_tags(&format!("rbtrfs:run={run_id}"))?
            .to_snapshot()?;
        snap.hostname = crate::hostname();
        snap.time = burst_time.clone();

        let opts = BackupOptions::default()
            .as_path(job.record_path.clone())
            // btrfs snapshots present files under a fresh subvolume: inode
            // numbers are stable within a snapshot but the device is not.
            .parent_opts(ParentOptions::default().ignore_inode(true))
            .excludes(Excludes::default().globs(excl));
        // Read from the SNAPSHOT (dest), never the live subvolume (src).
        let source = PathList::from_string(&job.dest.to_string_lossy())?.sanitize()?;
        let part = repo
            .backup(&opts, &source, snap)
            .with_context(|| format!("backing up {}", job.record_path.display()))?;
        println!("  backed up {} ({})", job.record_path.display(), part.id);
        parts.push(part);
    }

    // --- merge into one snapshot (re-open so the index sees the new trees) ---
    let repo = handle.open()?.to_indexed().context("indexing for merge")?;
    let merged_opts = {
        let mut o = SnapshotOptions::default().label("rbtrfs".to_string());
        for tag in &profile.tags {
            o = o.add_tags(tag)?;
        }
        o.add_tags(&format!("rbtrfs:run={run_id}"))?
    };
    let mut merged_snap = merged_opts.to_snapshot()?;
    merged_snap.hostname = crate::hostname();
    merged_snap.time = burst_time;
    // Lineage for listings: point at this host's previous merged snapshot. (Only the
    // parts drive incremental parent detection; this is informational.)
    merged_snap.parent = repo
        .get_all_snapshots()
        .context("listing snapshots")?
        .into_iter()
        .filter(|s| {
            s.label == "rbtrfs"
                && s.hostname == merged_snap.hostname
                && !s.tags.iter().any(|t| t == crate::restore::PART_TAG)
        })
        .max_by(|a, b| a.time.cmp(&b.time))
        .map(|s| s.id);
    let merged = repo
        .merge_snapshots(&parts, &newest_wins, merged_snap)
        .context("merging part snapshots")?;
    println!("merged snapshot {} paths={:?}", merged.id, merged.paths);

    // --- GC local snapshot sets (this profile's subvolumes only) ---
    let mut report = GcReport::default();
    for ((area, jobs), sel) in staged.iter().zip(&resolution.selections) {
        let keys: Vec<String> = jobs.iter().map(|j| j.key.clone()).collect();
        debug_assert_eq!(jobs.len(), sel.selected.len());
        for root in area.gc_roots() {
            match snapshot::gc(&btrfs, root, Some(&keys), &LocalRetention::new(profile.keep_local, profile.keep_local_days)) {
                Ok(r) => report.merge(r),
                Err(e) => eprintln!("rbtrfs: warning: gc of {} failed: {e:#}", root.display()),
            }
        }
    }
    report.warn();
    let gc_deleted = report.deleted.len();
    if gc_deleted > 0 {
        println!("gc: deleted {gc_deleted} old local snapshot(s)");
    }

    Ok(RunOutcome { run_id, merged, parts: parts.len(), gc_deleted })
}

/// Globs for one job: the profile's excludes re-rooted onto the snapshot, plus
/// the staging dir that in-subvolume mode leaves inside the snapshot.
fn job_excludes(profile: &Profile, job: &SnapJob) -> Result<Vec<String>> {
    // Absolute globs are matched against the canonical path the walker reports.
    let root = std::fs::canonicalize(&job.dest).unwrap_or_else(|_| job.dest.clone());
    let staging = job.staging_in_snapshot.as_ref().map(|p| {
        let rel = p.strip_prefix(&job.dest).unwrap_or(p);
        root.join(rel)
    });
    let extra: Vec<&Path> = staging.iter().map(|p| p.as_path()).collect();
    excludes::translate(&profile.exclude, &job.record_path, &root, &extra)
        .with_context(|| format!("exclude patterns for {}", job.record_path.display()))
}

fn dry_run_report(
    profile: &Profile,
    selections: &[FilesystemSelection<'_>],
    run_id: String,
) -> Result<RunOutcome> {
    let total: usize = selections.iter().map(|s| s.selected.len()).sum();
    println!("run {run_id}: would snapshot {total} subvolume(s):");
    for sel in selections {
        for s in &sel.selected {
            println!(
                "  {} (subvol {}) -> staging/{}/{run_id}, recorded as {}",
                s.mount_point.display(),
                s.subvol,
                s.key,
                s.mount_point.display()
            );
        }
    }
    // Validate what a real run would need, without writing anything.
    let handle = RepoHandle::from_profile(profile).context("repository configuration / password")?;
    match handle.exists().context("checking repository")? {
        true => println!("repository {}: found, would append", profile.repository),
        false => println!("repository {}: not initialised, would be created", profile.repository),
    }
    for (name, list) in [("pre", &profile.hooks.pre), ("post", &profile.hooks.post)] {
        for cmd in list {
            println!("  {name}-hook: {cmd}");
        }
    }
    Ok(RunOutcome { run_id, merged: SnapshotFile::default(), parts: total, gc_deleted: 0 })
}

/// Warn about subvolumes nested inside a selected one that will show up as empty
/// directories in the snapshot: mounted ones (from mountinfo) and, when we can
/// enumerate them (root), unmounted ones too (docker, machinectl, ...).
fn warn_nested(sel: &FilesystemSelection<'_>, profile: &Profile, btrfs: &dyn BtrfsOps) {
    for nested in &sel.nested_unselected {
        eprintln!(
            "rbtrfs: warning: {} is a mounted subvolume nested under a selected path but is \
             not itself selected — it will be an empty directory in the backup",
            nested.display()
        );
    }
    if !crate::is_root() {
        return;
    }
    let Ok(subvols) = sel.fs.subvolumes(btrfs) else { return };
    let rel = |s: &str| s.trim_start_matches('/').to_string();
    let selected: Vec<String> = sel.selected.iter().map(|s| rel(&s.subvol)).collect();
    let mounted: Vec<String> = sel.fs.mounts.iter().map(|m| rel(&m.subvol)).collect();
    for sv in &subvols {
        let path = sv.path.to_string_lossy();
        let under = selected.iter().find(|s| {
            !s.is_empty() && sv.path.starts_with(s.as_str()) && sv.path != Path::new(s.as_str())
                || s.is_empty() && !path.is_empty()
        });
        let Some(parent) = under else { continue };
        let in_staging = sv.path.starts_with(Path::new(parent).join(&profile.staging_name))
            || sv.path.starts_with(&profile.staging_name);
        if sv.read_only || in_staging || selected.contains(&path.to_string()) || mounted.contains(&path.to_string()) {
            continue; // read-only = snapshots (snapper etc.); mounted ones were warned above
        }
        eprintln!(
            "rbtrfs: warning: subvolume /{path} is nested under a selected subvolume but is not \
             selected — it will be an empty directory in the backup"
        );
    }
}

fn newest_wins(a: &Node, b: &Node) -> Ordering {
    a.meta.mtime.cmp(&b.meta.mtime)
}
