//! The `backup` run: select → stage → burst → per-subvol backup → merge → GC.

use std::cmp::Ordering;
use std::path::Path;

use anyhow::{Context, Result};
use rustic_core::{
    repofile::{Node, SnapshotFile},
    BackupOptions, Excludes, LocalSourceFilterOptions, ParentOptions, PathList, SnapshotOptions,
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

pub fn run(
    profile: &Profile,
    dry_run: bool,
    scan_files: bool,
    auto_init: bool,
) -> Result<RunOutcome> {
    let run_id = runid::now();
    let filesystems = discover::discover()?;
    let resolution = select::resolve(
        &filesystems,
        &profile.subvolumes,
        &profile.exclude_subvolumes,
    )?;
    let btrfs = LibBtrfsUtil;

    for w in &resolution.warnings {
        eprintln!("rbtrfs: warning: {w}");
    }
    for sel in &resolution.selections {
        warn_nested(sel, profile, &btrfs);
    }

    let selected: Vec<&select::Selected> = resolution
        .selections
        .iter()
        .flat_map(|s| &s.selected)
        .collect();
    let mounts = crate::mountinfo::read().context("reading /proc/self/mountinfo")?;
    let extras = crate::extra::plan(&profile.extra_paths, &mounts, &selected, |p| p.is_dir())?;
    for w in &extras.warnings {
        eprintln!("rbtrfs: warning: {w}");
    }

    if dry_run {
        return dry_run_report(
            profile,
            &resolution,
            &extras.paths,
            run_id,
            scan_files,
            auto_init,
        );
    }

    let _lock = lock::acquire()?;

    // Prepare staging for every filesystem, then plan every snapshot job, so the
    // burst covers all subvolumes with no I/O interleaved. Run ids have one-second
    // resolution: if a previous run in this very second left its snapshots behind,
    // move on to the next free second instead of colliding with them.
    let areas = resolution
        .selections
        .iter()
        .map(|sel| {
            StagingArea::prepare(sel.fs, profile, &sel.selected)
                .with_context(|| format!("preparing staging for {}", sel.fs.source))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut secs = runid::now_unix();
    let (run_id, planned) = loop {
        let id = runid::from_unix(secs);
        let planned: Vec<Vec<SnapJob>> = areas
            .iter()
            .zip(&resolution.selections)
            .map(|(area, sel)| area.plan(&sel.selected, &id, profile))
            .collect();
        if planned.iter().flatten().all(|j| !j.dest.exists()) {
            break (id, planned);
        }
        secs += 1;
    };
    let staged: Vec<(StagingArea, Vec<SnapJob>)> = areas.into_iter().zip(planned).collect();
    let all_jobs: Vec<SnapJob> = staged.iter().flat_map(|(_, j)| j.clone()).collect();

    // Fail on a bad repository/password/excludes BEFORE running any hook or taking any snapshot.
    let handle = RepoHandle::from_profile(profile)?;
    handle.open_or_init(auto_init)?;
    let job_excludes: Vec<Vec<String>> = all_jobs
        .iter()
        .map(|j| job_excludes(profile, j))
        .collect::<Result<_>>()?;

    let extra_excludes: Vec<Vec<String>> = extras
        .paths
        .iter()
        .map(|e| {
            let read = std::fs::canonicalize(&e.path).unwrap_or_else(|_| e.path.clone());
            excludes::translate(&profile.exclude, &e.path, &read, &[])
                .with_context(|| format!("exclude patterns for {}", e.path.display()))
        })
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
    let backup_part = |key: &str,
                       read_path: &Path,
                       record_path: &Path,
                       globs: Vec<String>|
     -> Result<SnapshotFile> {
        // Re-open per part: the in-memory index must see the trees written so far.
        let repo = handle
            .open()?
            .to_indexed_ids()
            .context("indexing repository")?;
        let mut snap = SnapshotOptions::default()
            .label(format!("rbtrfs-part:{key}"))
            .add_tags("rbtrfs:part")?
            .add_tags(&format!("rbtrfs:run={run_id}"))?
            .to_snapshot()?;
        snap.hostname = crate::hostname();
        snap.time = burst_time.clone();

        let opts = BackupOptions::default()
            .as_path(record_path.to_path_buf())
            // btrfs snapshots present files under a fresh subvolume: inode
            // numbers are stable within a snapshot but the device is not.
            .parent_opts(ParentOptions::default().ignore_inode(true))
            .excludes(Excludes::default().globs(globs))
            .ignore_filter_opts(
                LocalSourceFilterOptions::default()
                    .exclude_if_present(profile.exclude_if_present.clone())
                    .exclude_if_xattr(profile.exclude_if_xattr.clone()),
            );
        let source = PathList::from_string(&read_path.to_string_lossy())?.sanitize()?;
        let part = repo
            .backup(&opts, &source, snap)
            .with_context(|| format!("backing up {}", record_path.display()))?;
        println!("  backed up {} ({})", record_path.display(), part.id);
        Ok(part)
    };

    let mut parts = Vec::with_capacity(all_jobs.len() + extras.paths.len());
    for (job, excl) in all_jobs.iter().zip(job_excludes) {
        // Read from the SNAPSHOT (dest), never the live subvolume (src).
        parts.push(backup_part(&job.key, &job.dest, &job.record_path, excl)?);
    }
    // Extra paths are not snapshotted: they are read live, after the snapshots.
    for (extra, excl) in extras.paths.iter().zip(extra_excludes) {
        parts.push(backup_part(&extra.key, &extra.path, &extra.path, excl)?);
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
            match snapshot::gc(
                &btrfs,
                root,
                Some(&keys),
                &LocalRetention::new(profile.keep_local, profile.keep_local_days),
            ) {
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

    Ok(RunOutcome {
        run_id,
        merged,
        parts: parts.len(),
        gc_deleted,
    })
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
    resolution: &select::Resolution<'_>,
    extras: &[crate::extra::ExtraPath],
    run_id: String,
    scan_files: bool,
    auto_init: bool,
) -> Result<RunOutcome> {
    let selections = &resolution.selections;
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
    for e in extras {
        println!(
            "  {} (extra path, read live, not snapshotted) -> recorded as {}",
            e.path.display(),
            e.path.display()
        );
    }
    for mp in &resolution.excluded {
        println!("  skipping {} (exclude_subvolumes)", mp.display());
    }
    if scan_files {
        report_file_selection(profile, resolution, extras);
    }
    // Validate what a real run would need, without writing anything.
    let handle =
        RepoHandle::from_profile(profile).context("repository configuration / password")?;
    match handle.exists().context("checking repository")? {
        true => println!("repository {}: found, would append", profile.repository),
        false if auto_init => println!(
            "repository {}: not initialised, would be created",
            profile.repository
        ),
        false => anyhow::bail!(
            "no repository at {}; create it with `rbtrfs init`",
            profile.repository
        ),
    }
    for (name, list) in [("pre", &profile.hooks.pre), ("post", &profile.hooks.post)] {
        for cmd in list {
            println!("  {name}-hook: {cmd}");
        }
    }
    Ok(RunOutcome {
        run_id,
        merged: SnapshotFile::default(),
        parts: total + extras.len(),
        gc_deleted: 0,
    })
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
    let Ok(subvols) = sel.fs.subvolumes(btrfs) else {
        return;
    };
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
        let in_staging = sv
            .path
            .starts_with(Path::new(parent).join(&profile.staging_name))
            || sv.path.starts_with(&profile.staging_name);
        if sv.read_only
            || in_staging
            || selected.contains(&path.to_string())
            || mounted.contains(&path.to_string())
        {
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

/// Walk every selected path with the real exclude matcher and report what would be
/// stored and what would be skipped. Metadata only; reads no file contents.
fn report_file_selection(
    profile: &Profile,
    resolution: &select::Resolution<'_>,
    extras: &[crate::extra::ExtraPath],
) {
    use crate::dryrun::{human, scan};

    let mut roots: Vec<std::path::PathBuf> = resolution
        .selections
        .iter()
        .flat_map(|s| s.selected.iter().map(|s| s.mount_point.clone()))
        .collect();
    roots.extend(extras.iter().map(|e| e.path.clone()));

    println!();
    for root in roots {
        let staging = (profile.staging == crate::config::Staging::InSubvolume)
            .then(|| root.join(&profile.staging_name));
        let extra: Vec<&Path> = staging.iter().map(|p| p.as_path()).collect();
        let globs = match excludes::translate(&profile.exclude, &root, &root, &extra) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("rbtrfs: warning: {}: {e:#}", root.display());
                continue;
            }
        };
        let markers = crate::dryrun::Markers {
            present: &profile.exclude_if_present,
            xattr: &profile.exclude_if_xattr,
        };
        let s = match scan(&root, globs, &markers) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("rbtrfs: warning: could not scan {}: {e:#}", root.display());
                continue;
            }
        };
        println!(
            "{}: would store {} files ({}); excluded {} path(s) ({})",
            root.display(),
            s.files,
            human(s.bytes),
            s.excluded.len(),
            human(s.excluded_bytes())
        );
        for e in s.excluded.iter().take(15) {
            match &e.reason {
                Some(why) => println!("    {:>9}  {}  ({why})", human(e.bytes), e.path.display()),
                None => println!("    {:>9}  {}", human(e.bytes), e.path.display()),
            }
        }
        if s.excluded.len() > 15 {
            println!("    ... and {} more", s.excluded.len() - 15);
        }
        if s.unreadable > 0 {
            println!(
                "    note: {} directories could not be read; run as root to include them",
                s.unreadable
            );
        }
    }
}
