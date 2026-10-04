//! Staging area management, the tight snapshot burst, and local-snapshot GC.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::btrfs::BtrfsOps;
use crate::config::{Profile, Staging};
use crate::discover::BtrfsFilesystem;
use crate::ns::TransientMount;
use crate::runid;
use crate::select::Selected;

/// Where read-only snapshots for one filesystem are staged.
pub struct StagingArea {
    /// Directories whose children are `<key>/<runid>` snapshot subvolumes: one
    /// (`<top-level>/<staging_name>`) in top-level mode, one per selected
    /// subvolume (`<mountpoint>/<staging_name>`) in in-subvolume mode.
    gc_roots: Vec<PathBuf>,
    /// Top-level mode only: the transient `subvolid=5` mount, held for the
    /// lifetime of the area.
    top: Option<TransientMount>,
}

impl StagingArea {
    /// Prepare the staging area for the `selected` subvolumes of `fs` per the
    /// profile. In top-level mode this transiently mounts `subvolid=5` (a private
    /// namespace is assumed).
    pub fn prepare(fs: &BtrfsFilesystem, profile: &Profile, selected: &[Selected]) -> Result<Self> {
        match profile.staging {
            Staging::TopLevel => {
                let target = crate::ns::mount_target(&fs.dev);
                let mount = TransientMount::top_level(&fs.source, &target)?;
                let root = mount.path().join(&profile.staging_name);
                Ok(Self {
                    gc_roots: vec![root],
                    top: Some(mount),
                })
            }
            Staging::InSubvolume => Ok(Self {
                gc_roots: selected
                    .iter()
                    .map(|s| s.mount_point.join(&profile.staging_name))
                    .collect(),
                top: None,
            }),
        }
    }

    /// Build the snapshot jobs for `selected` for the given run id. `src` is the
    /// live subvolume, `dest` the read-only snapshot that gets backed up.
    pub fn plan(&self, selected: &[Selected], run_id: &str, profile: &Profile) -> Vec<SnapJob> {
        selected
            .iter()
            .map(|s| match &self.top {
                Some(mount) => SnapJob {
                    key: s.key.clone(),
                    record_path: s.mount_point.clone(),
                    // top-level mount root + subvol path (strip leading '/')
                    src: mount.path().join(s.subvol.trim_start_matches('/')),
                    dest: self.gc_roots[0].join(&s.key).join(run_id),
                    staging_in_snapshot: None,
                },
                None => {
                    let dest = s
                        .mount_point
                        .join(&profile.staging_name)
                        .join(&s.key)
                        .join(run_id);
                    SnapJob {
                        key: s.key.clone(),
                        record_path: s.mount_point.clone(),
                        src: s.mount_point.clone(),
                        // The snapshot of the subvolume contains the staging dir that
                        // lives inside it; it must not be backed up.
                        staging_in_snapshot: Some(dest.join(&profile.staging_name)),
                        dest,
                    }
                }
            })
            .collect()
    }

    pub fn gc_roots(&self) -> &[PathBuf] {
        &self.gc_roots
    }
}

/// One subvolume to snapshot.
#[derive(Debug, Clone)]
pub struct SnapJob {
    pub key: String,
    /// Path to record in the restic snapshot (the original mount point).
    pub record_path: PathBuf,
    /// Live subvolume to snapshot.
    pub src: PathBuf,
    /// The read-only snapshot to create, and later back up.
    pub dest: PathBuf,
    /// In-subvolume mode: the staging directory as it appears inside `dest`, which
    /// must be excluded from the backup.
    pub staging_in_snapshot: Option<PathBuf>,
}

/// Create all read-only snapshots back-to-back. Directory creation and option
/// setup happen first; the final loop does nothing but issue the snapshot calls,
/// to keep cross-subvolume skew minimal (no logging, no stat; libbtrfsutil's own
/// per-call path conversion is the only extra work).
pub fn burst(btrfs: &dyn BtrfsOps, jobs: &[SnapJob]) -> Result<()> {
    for j in jobs {
        let parent = j.dest.parent().expect("dest has parent");
        // `<staging>/<key>`; its parent is the staging directory itself
        if let Some(staging) = parent.parent() {
            private_dir(staging)?;
        }
        private_dir(parent)?;
        if j.dest.exists() {
            anyhow::bail!("snapshot destination already exists: {}", j.dest.display());
        }
    }

    // --- hot loop ---
    for j in jobs {
        btrfs.snapshot_readonly(&j.src, &j.dest)?;
    }
    Ok(())
}

/// Delete the snapshots of a run that failed part-way. Missing ones are fine (the
/// burst may not have reached them); anything else is reported and left for `gc`.
pub fn discard(btrfs: &dyn BtrfsOps, jobs: &[SnapJob]) {
    for j in jobs {
        if !j.dest.exists() {
            continue;
        }
        if let Err(e) = btrfs.delete_subvolume(&j.dest) {
            eprintln!(
                "rbtrfs: warning: could not remove {} after the failed run: {e:#}",
                j.dest.display()
            );
        }
    }
}

/// Make sure `dir` is a real directory owned by the user we run as, creating it
/// (mode 0700) if it does not exist. A symlink or a directory owned by someone else
/// could point where snapshots are made, or which subvolumes `gc` deletes, to a place
/// of that user's choosing; `in-subvolume` staging lives somewhere users may write.
fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::symlink_metadata(dir) {
        Ok(_) => trusted_dir(dir),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
            let m = std::fs::symlink_metadata(dir)?;
            anyhow::ensure!(
                m.uid() == nix::unistd::geteuid().as_raw(),
                "{} is not ours",
                dir.display()
            );
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("checking {}", dir.display())),
    }
}

/// `dir` exists, is not a symlink, is a directory and belongs to us.
fn trusted_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m =
        std::fs::symlink_metadata(dir).with_context(|| format!("checking {}", dir.display()))?;
    anyhow::ensure!(
        m.is_dir() && !m.file_type().is_symlink(),
        "{} must be a plain directory, not a symlink or file",
        dir.display()
    );
    anyhow::ensure!(
        m.uid() == nix::unistd::geteuid().as_raw(),
        "{} is owned by uid {}, not by us; refusing to use it",
        dir.display(),
        m.uid()
    );
    Ok(())
}

/// Which local snapshot sets survive a GC.
#[derive(Debug, Clone, Copy)]
pub struct LocalRetention {
    /// Newest N sets per key are always kept.
    pub keep: usize,
    /// Sets younger than this many days are kept as well.
    pub keep_days: Option<u64>,
    /// Current unix time (a parameter so tests need no clock).
    pub now: u64,
}

impl LocalRetention {
    pub fn new(keep: usize, keep_days: Option<u64>) -> Self {
        Self {
            keep,
            keep_days,
            now: runid::now_unix(),
        }
    }

    fn young(&self, run_secs: u64) -> bool {
        self.keep_days
            .is_some_and(|d| self.now.saturating_sub(run_secs) < d.saturating_mul(86_400))
    }
}

/// Delete local snapshot sets that fall outside `retention`, per key, under
/// `staging_root`. Only directories whose name parses as one of our run ids and
/// that really are subvolumes are touched — foreign entries (snapper etc.) are
/// ignored. `keys` limits GC to those keys (a profile only owns its own); `None`
/// means every key. A failed delete is reported and does not stop the sweep.
pub fn gc(
    btrfs: &dyn BtrfsOps,
    staging_root: &Path,
    keys: Option<&[String]>,
    retention: &LocalRetention,
) -> Result<GcReport> {
    let mut report = GcReport::default();
    if staging_root.symlink_metadata().is_ok() {
        trusted_dir(staging_root)?;
    }
    let key_dirs = match std::fs::read_dir(staging_root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(e).context(format!("reading {}", staging_root.display())),
    };

    for key_entry in key_dirs {
        let key_dir = key_entry?.path();
        let owned = match (keys, key_dir.file_name().and_then(|n| n.to_str())) {
            (None, _) => true,
            (Some(keys), Some(name)) => keys.iter().any(|k| k == name),
            (Some(_), None) => false,
        };
        if !owned || !key_dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            continue;
        }
        if let Err(e) = trusted_dir(&key_dir) {
            eprintln!("rbtrfs: warning: gc: skipping {e:#}");
            continue;
        }
        let mut runs: Vec<(u64, PathBuf)> = std::fs::read_dir(&key_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| !t.is_symlink()))
            .filter_map(|e| {
                let name = e.file_name();
                let secs = runid::to_unix(name.to_str()?)?;
                Some((secs, e.path()))
            })
            .collect();
        runs.sort_by_key(|(secs, _)| std::cmp::Reverse(*secs)); // newest first

        for (secs, path) in runs.into_iter().skip(retention.keep) {
            if retention.young(secs) {
                continue;
            }
            match btrfs.is_subvolume(&path) {
                Ok(true) => {}
                Ok(false) => {
                    report.skipped.push(path);
                    continue;
                }
                Err(e) => {
                    report.failed.push((path, format!("{e:#}")));
                    continue;
                }
            }
            match btrfs.delete_subvolume(&path) {
                Ok(()) => report.deleted.push(path),
                Err(e) => report.failed.push((path, format!("{e:#}"))),
            }
        }
    }
    Ok(report)
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub deleted: Vec<PathBuf>,
    /// Run-id-named entries that are not subvolumes; left alone.
    pub skipped: Vec<PathBuf>,
    /// Entries that could not be deleted, with the error.
    pub failed: Vec<(PathBuf, String)>,
}

impl GcReport {
    pub fn merge(&mut self, other: GcReport) {
        self.deleted.extend(other.deleted);
        self.skipped.extend(other.skipped);
        self.failed.extend(other.failed);
    }

    /// Print skipped/failed entries as warnings.
    pub fn warn(&self) {
        for p in &self.skipped {
            eprintln!(
                "rbtrfs: warning: gc: {} is not a subvolume; left alone",
                p.display()
            );
        }
        for (p, e) in &self.failed {
            eprintln!("rbtrfs: warning: gc: could not delete {}: {e}", p.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs::Subvolume;
    use std::cell::RefCell;

    /// Fake backend: directories are "subvolumes" unless listed as plain; deletes
    /// are recorded, and can be made to fail.
    #[derive(Default)]
    struct Fake {
        plain: Vec<PathBuf>,
        fail_delete: Vec<PathBuf>,
        deleted: RefCell<Vec<PathBuf>>,
        snapshots: RefCell<Vec<(PathBuf, PathBuf)>>,
    }

    impl BtrfsOps for Fake {
        fn is_subvolume(&self, p: &Path) -> Result<bool> {
            Ok(!self.plain.iter().any(|x| x == p))
        }
        fn list_subvolumes(&self, _: &Path) -> Result<Vec<Subvolume>> {
            Ok(vec![])
        }
        fn snapshot_readonly(&self, src: &Path, dest: &Path) -> Result<()> {
            self.snapshots.borrow_mut().push((src.into(), dest.into()));
            Ok(())
        }
        fn create_subvolume(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        fn delete_subvolume(&self, p: &Path) -> Result<()> {
            if self.fail_delete.iter().any(|x| x == p) {
                anyhow::bail!("boom");
            }
            self.deleted.borrow_mut().push(p.into());
            Ok(())
        }
    }

    fn staging(runs: &[(&str, &[&str])]) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        for (key, ids) in runs {
            for id in *ids {
                std::fs::create_dir_all(t.path().join(key).join(id)).unwrap();
            }
        }
        t
    }

    /// Retention evaluated "now" = 2026-01-03T12:00:00Z.
    fn keep(n: usize) -> LocalRetention {
        LocalRetention {
            keep: n,
            keep_days: None,
            now: runid::to_unix("20260103T120000Z").unwrap(),
        }
    }

    const R1: &str = "20260101T000000Z";
    const R2: &str = "20260102T000000Z";
    const R3: &str = "20260103T000000Z";

    #[test]
    fn keeps_newest_n_per_key() {
        let t = staging(&[("home", &[R1, R2, R3]), ("srv", &[R1, R2])]);
        let fake = Fake::default();
        let r = gc(&fake, t.path(), None, &keep(2)).unwrap();
        let mut got = r.deleted.clone();
        got.sort();
        assert_eq!(got, vec![t.path().join("home").join(R1)]);
        let r = gc(&fake, t.path(), None, &keep(0)).unwrap();
        assert_eq!(r.deleted.len(), 5);
    }

    #[test]
    fn young_sets_survive_beyond_the_count() {
        let t = staging(&[("home", &[R1, R2, R3])]);
        let fake = Fake::default();
        // now = Jan 3 12:00; R3 is 12h old, R2 1.5d, R1 2.5d. keep 1 + younger than 2 days
        let ret = LocalRetention {
            keep_days: Some(2),
            ..keep(1)
        };
        let r = gc(&fake, t.path(), None, &ret).unwrap();
        assert_eq!(r.deleted, vec![t.path().join("home").join(R1)]);
    }

    #[test]
    fn only_touches_owned_keys_and_run_ids() {
        let t = staging(&[
            ("home", &[R1, R2]),
            ("other", &[R1, R2]),
            ("home", &["snapper-1"]),
        ]);
        let fake = Fake::default();
        let r = gc(&fake, t.path(), Some(&["home".to_string()]), &keep(1)).unwrap();
        assert_eq!(r.deleted, vec![t.path().join("home").join(R1)]);
    }

    #[test]
    fn plain_dirs_are_skipped_and_failures_do_not_abort() {
        let t = staging(&[("a", &[R1, R2]), ("b", &[R1, R2])]);
        let fake = Fake {
            plain: vec![t.path().join("a").join(R1)],
            fail_delete: vec![t.path().join("b").join(R1)],
            ..Default::default()
        };
        let r = gc(&fake, t.path(), None, &keep(1)).unwrap();
        assert!(r.deleted.is_empty());
        assert_eq!(r.skipped, vec![t.path().join("a").join(R1)]);
        assert_eq!(r.failed.len(), 1);
    }

    #[test]
    fn missing_staging_root_is_fine() {
        let t = tempfile::tempdir().unwrap();
        let r = gc(&Fake::default(), &t.path().join("nope"), None, &keep(1)).unwrap();
        assert!(r.deleted.is_empty());
    }

    #[test]
    fn burst_snapshots_every_job_in_order_and_refuses_existing_dest() {
        let t = tempfile::tempdir().unwrap();
        let job = |k: &str| SnapJob {
            key: k.into(),
            record_path: format!("/{k}").into(),
            src: format!("/live/{k}").into(),
            dest: t.path().join(k).join(R1),
            staging_in_snapshot: None,
        };
        let fake = Fake::default();
        burst(&fake, &[job("a"), job("b")]).unwrap();
        let snaps = fake.snapshots.borrow();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].0, PathBuf::from("/live/a"));
        assert_eq!(snaps[1].1, t.path().join("b").join(R1));
        drop(snaps);

        std::fs::create_dir_all(t.path().join("c").join(R1)).unwrap();
        let fake = Fake::default();
        assert!(burst(&fake, &[job("a"), job("c")]).is_err());
        assert!(
            fake.snapshots.borrow().is_empty(),
            "nothing snapshotted if any dest is bad"
        );
    }

    #[test]
    fn gc_does_not_follow_symlinked_key_directories_or_runs() {
        let elsewhere = staging(&[("victim", &[R1, R2, R3])]);
        let t = tempfile::tempdir().unwrap();
        // a key directory that is a link to somewhere else
        std::os::unix::fs::symlink(elsewhere.path().join("victim"), t.path().join("home")).unwrap();
        // a real key directory holding a run that is a link
        std::fs::create_dir(t.path().join("srv")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path().join("victim").join(R1),
            t.path().join("srv").join(R1),
        )
        .unwrap();
        let fake = Fake::default();
        let r = gc(&fake, t.path(), None, &keep(0)).unwrap();
        assert!(r.deleted.is_empty(), "{:?}", r.deleted);
        assert!(fake.deleted.borrow().is_empty());
    }

    #[test]
    fn gc_refuses_a_symlinked_staging_directory() {
        let elsewhere = staging(&[("home", &[R1])]);
        let t = tempfile::tempdir().unwrap();
        let link = t.path().join("staging");
        std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();
        let fake = Fake::default();
        assert!(gc(&fake, &link, None, &keep(0)).is_err());
        assert!(fake.deleted.borrow().is_empty());
    }

    #[test]
    fn burst_refuses_a_symlinked_key_directory() {
        let elsewhere = tempfile::tempdir().unwrap();
        let t = tempfile::tempdir().unwrap();
        let staging = t.path().join("stage");
        std::fs::create_dir(&staging).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), staging.join("home")).unwrap();
        let job = SnapJob {
            key: "home".into(),
            record_path: "/home".into(),
            src: "/home".into(),
            dest: staging.join("home").join(R1),
            staging_in_snapshot: None,
        };
        let fake = Fake::default();
        assert!(burst(&fake, &[job]).is_err());
        assert!(fake.snapshots.borrow().is_empty());
        assert!(std::fs::read_dir(elsewhere.path())
            .unwrap()
            .next()
            .is_none());
    }
}
