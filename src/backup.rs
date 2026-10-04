//! The `backup` run: select → stage → burst → per-subvol backup → merge → GC.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustic_core::{
    repofile::{Node, SnapshotFile},
    BackupOptions, Excludes, LocalSourceFilterOptions, PathList, SnapshotOptions,
};

use crate::btrfs::{BtrfsOps, LibBtrfsUtil};
use crate::config::Profile;
use crate::discover;
use crate::excludes;
use crate::hooks;
use crate::lock;
use crate::repo::RepoHandle;
use crate::rootmeta;
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
    report_excluded: bool,
) -> Result<RunOutcome> {
    let run_id = runid::now();
    let filesystems = discover::discover()?;
    // A btrfs share that `repository_mount` mounts is where the backup goes, not
    // something to back up (it matters for `subvolumes = "all"`).
    let mut skip = profile.exclude_subvolumes.clone();
    if let Some(m) = &profile.repository_mount {
        skip.push(m.target.to_string_lossy().into_owned());
    }
    let resolution = select::resolve(&filesystems, &profile.subvolumes, &skip)?;
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
            let skip: Vec<PathBuf> = repo_exclusion(profile, &e.path, &read)
                .into_iter()
                .collect();
            let skip: Vec<&Path> = skip.iter().map(|p| p.as_path()).collect();
            excludes::translate(&profile.exclude, &e.path, &read, &skip)
                .with_context(|| format!("exclude patterns for {}", e.path.display()))
        })
        .collect::<Result<_>>()?;

    println!(
        "run {run_id}: {} subvolume(s) across {} filesystem(s)",
        all_jobs.len(),
        staged.len()
    );

    // Everything from the snapshot window to the merge. If any of it fails, this run's
    // snapshots are of no use (nothing in the repository refers to them): remove them
    // instead of leaving them for `rbtrfs gc`.
    let profile_tag = format!("{}{}", crate::forget::PROFILE_TAG_PREFIX, profile.name);
    let upload = || -> Result<(Vec<SnapshotFile>, SnapshotFile)> {
        // --- consistency window ---
        // Post-hooks always run, and termination signals are deferred until they have.
        let burst_time = hooks::window(&profile.hooks, || {
            snapshot::burst(&btrfs, &all_jobs).context("snapshot burst")?;
            // The instant the data represents. `SnapshotFile::default()` stamps "now".
            Ok(SnapshotFile::default().time)
        })?;
        println!("snapshotted {} subvolume(s)", all_jobs.len());
        if report_excluded || profile.report_excluded {
            let sources = all_jobs
                .iter()
                .zip(&job_excludes)
                .map(|(j, g)| (&j.dest, &j.record_path, g))
                .chain(
                    extras
                        .paths
                        .iter()
                        .zip(&extra_excludes)
                        .map(|(e, g)| (&e.path, &e.path, g)),
                );
            report_marker_exclusions(profile, sources);
        }

        // --- back up each read-only snapshot, recording the real mount point ---
        let backup_part = |key: &str,
                           read_path: &Path,
                           record_path: &Path,
                           globs: Vec<String>,
                           one_file_system: bool|
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
                .add_tags(&profile_tag)?
                .to_snapshot()?;
            snap.hostname = crate::hostname();
            snap.time = burst_time.clone();

            let opts = BackupOptions::default()
                .as_path(record_path.to_path_buf())
                // No parent_opts on purpose. rustic_core's `ignore_inode` works backwards
                // (`true` makes it compare inodes); the default ignores them, which is what
                // a fresh snapshot of the same files needs.
                .excludes(Excludes::default().globs(globs))
                .ignore_filter_opts(
                    LocalSourceFilterOptions::default()
                        .exclude_if_present(profile.exclude_if_present.clone())
                        .exclude_if_xattr(profile.exclude_if_xattr.clone())
                        .one_file_system(one_file_system),
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
            parts.push(backup_part(
                &job.key,
                &job.dest,
                &job.record_path,
                excl,
                false,
            )?);
        }
        // Extra paths are not snapshotted: they are read live, after the snapshots.
        // They stay on their own filesystem, like the dry run does.
        for (extra, excl) in extras.paths.iter().zip(extra_excludes) {
            parts.push(backup_part(
                &extra.key,
                &extra.path,
                &extra.path,
                excl,
                true,
            )?);
        }

        // --- merge into one snapshot (re-open so the index sees the new trees) ---
        let repo = handle.open()?.to_indexed().context("indexing for merge")?;
        // rustic stores the backup roots with default metadata; keep the real ones.
        let roots: rootmeta::Roots = all_jobs
            .iter()
            .map(|j| (&j.dest, &j.record_path))
            .chain(extras.paths.iter().map(|e| (&e.path, &e.path)))
            .filter_map(|(read, record)| {
                rootmeta::capture(read)
                    .map_err(|e| eprintln!("rbtrfs: warning: {e:#}"))
                    .ok()
                    .map(|m| (record.to_string_lossy().into_owned(), m))
            })
            .collect();
        let merged_opts = {
            let mut o = SnapshotOptions::default()
                .label("rbtrfs".to_string())
                .description(rootmeta::encode(&roots));
            for tag in &profile.tags {
                o = o.add_tags(tag)?;
            }
            o.add_tags(&format!("rbtrfs:run={run_id}"))?
                .add_tags(&profile_tag)?
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
        Ok((parts, merged))
    };
    let (parts, merged) = match upload() {
        Ok(v) => v,
        Err(e) => {
            snapshot::discard(&btrfs, &all_jobs);
            return Err(e);
        }
    };

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
/// Where a local repository sits below `record_root`, as a path below `read_root`.
/// The repository must never be backed up into itself.
fn repo_exclusion(profile: &Profile, record_root: &Path, read_root: &Path) -> Option<PathBuf> {
    let repo = crate::repo::local_path(&profile.repository)?;
    let repo = std::fs::canonicalize(&repo).unwrap_or(repo);
    let record_root = std::fs::canonicalize(record_root).unwrap_or(record_root.to_path_buf());
    repo.strip_prefix(&record_root)
        .ok()
        .map(|rel| read_root.join(rel))
}

fn job_excludes(profile: &Profile, job: &SnapJob) -> Result<Vec<String>> {
    // Absolute globs are matched against the canonical path the walker reports.
    let root = std::fs::canonicalize(&job.dest).unwrap_or_else(|_| job.dest.clone());
    let staging = job.staging_in_snapshot.as_ref().map(|p| {
        let rel = p.strip_prefix(&job.dest).unwrap_or(p);
        root.join(rel)
    });
    let mut skip: Vec<PathBuf> = staging.into_iter().collect();
    skip.extend(repo_exclusion(profile, &job.record_path, &root));
    let extra: Vec<&Path> = skip.iter().map(|p| p.as_path()).collect();
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
        let repo = repo_exclusion(profile, &root, &root);
        let extra: Vec<&Path> = staging.iter().chain(&repo).map(|p| p.as_path()).collect();
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

/// List the directories left out because of an `exclude_if_present` or
/// `exclude_if_xattr` marker, as paths on the running system. Anyone who can write into
/// a directory can add a marker to it, so this is how you notice.
fn report_marker_exclusions<'a>(
    profile: &Profile,
    sources: impl Iterator<Item = (&'a PathBuf, &'a PathBuf, &'a Vec<String>)>,
) {
    let markers = crate::dryrun::Markers {
        present: &profile.exclude_if_present,
        xattr: &profile.exclude_if_xattr,
    };
    if markers.present.is_empty() && markers.xattr.is_empty() {
        return;
    }
    let mut left_out = Vec::new();
    for (read, record, globs) in sources {
        // the walker reports canonical paths, the globs were built for them too
        let root = std::fs::canonicalize(read).unwrap_or_else(|_| read.clone());
        match crate::dryrun::scan(&root, globs.clone(), &markers) {
            Ok(s) => {
                for e in s.excluded {
                    if let Some(why) = e.reason {
                        let shown = e
                            .path
                            .strip_prefix(&root)
                            .map_or(e.path.clone(), |rel| record.join(rel));
                        left_out.push((e.bytes, shown, why));
                    }
                }
            }
            Err(e) => eprintln!("rbtrfs: warning: could not scan {}: {e:#}", read.display()),
        }
    }
    left_out.sort_by_key(|a| std::cmp::Reverse(a.0));
    println!(
        "left out because of a marker: {} director(ies)",
        left_out.len()
    );
    for (bytes, path, why) in &left_out {
        println!(
            "    {:>9}  {}  ({why})",
            crate::dryrun::human(*bytes),
            path.display()
        );
    }
}
