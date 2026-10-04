//! Milestone 0 spike: the whole spine on the REAL filesystem.
//!
//!   unshare(mnt ns) -> MS_PRIVATE -> mount subvolid=5 -> snapshot selected subvols
//!   read-only into a staging dir -> rustic_core backup each with as_path -> merge
//!   -> verify merged tree -> delete the btrfs snapshots -> assert host mount table clean
//!
//! Root required. Run:
//!   cargo build --example spike_full_spine && sudo ./target/debug/examples/spike_full_spine
//!
//! It discovers the btrfs device backing `/` and the subvols mounted at /home and /srv
//! from /proc/self/mountinfo. Snapshots are created read-only and deleted at the end
//! (even on failure). The restic repo is left at /tmp/rbtrfs-spine-repo (password spikepw).

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use libbtrfsutil::CreateSnapshotOptions;
use nix::mount::{mount, umount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use rustic_backend::BackendOptions;
use rustic_core::{
    repofile::Node, BackupOptions, ConfigOptions, Credentials, KeyOptions, LsOptions, PathList,
    Repository, RepositoryOptions, SnapshotOptions,
};

struct Mnt {
    mountpoint: PathBuf,
    device: String,
    subvol: String, // e.g. "/@home"
}

fn parse_mountinfo(targets: &[&str]) -> Result<Vec<Mnt>> {
    let text = fs::read_to_string("/proc/self/mountinfo")?;
    let mut out = Vec::new();
    for line in text.lines() {
        // fields: ... - <fstype> <source> <superopts>
        let (pre, post) = line
            .split_once(" - ")
            .ok_or_else(|| anyhow!("bad mountinfo"))?;
        let pf: Vec<&str> = pre.split_whitespace().collect();
        let sf: Vec<&str> = post.split_whitespace().collect();
        if sf.first() != Some(&"btrfs") {
            continue;
        }
        let mountpoint = pf[4];
        if !targets.contains(&mountpoint) {
            continue;
        }
        let device = sf[1].to_string();
        let subvol = sf[2..]
            .join(" ")
            .split(',')
            .find_map(|kv| kv.strip_prefix("subvol=").map(str::to_string))
            .ok_or_else(|| anyhow!("no subvol= for {mountpoint}"))?;
        out.push(Mnt {
            mountpoint: mountpoint.into(),
            device,
            subvol,
        });
    }
    Ok(out)
}

fn host_mounts_snapshot() -> String {
    fs::read_to_string("/proc/1/mounts").unwrap_or_default()
}

fn run() -> Result<()> {
    // small subvolumes so the spike finishes fast; mechanics are size-independent
    let mnts = parse_mountinfo(&["/var/log", "/var/tmp"])?;
    if mnts.len() < 2 {
        bail!("expected /var/log and /var/tmp to be btrfs mounts");
    }
    let device = mnts[0].device.clone();
    println!("device={device}");
    for m in &mnts {
        println!("  {} -> subvol={}", m.mountpoint.display(), m.subvol);
    }

    unshare(CloneFlags::CLONE_NEWNS).context("unshare")?;
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .context("MS_PRIVATE /")?;

    let top = PathBuf::from("/run/rbtrfs-spine-top");
    fs::create_dir_all(&top)?;
    mount(
        Some(device.as_str()),
        &top,
        Some("btrfs"),
        MsFlags::empty(),
        Some("subvolid=5"),
    )
    .context("mount subvolid=5")?;
    println!("mounted top-level subvol at {}", top.display());

    let staging = top.join(".rbtrfs-spine");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    // resolve source subvol paths under the top-level mount
    let sources: Vec<(PathBuf, PathBuf, PathBuf)> = mnts
        .iter()
        .map(|m| {
            let key = m
                .mountpoint
                .to_string_lossy()
                .trim_start_matches('/')
                .replace('/', "-");
            let src = top.join(m.subvol.trim_start_matches('/'));
            let snap = staging.join(&key);
            (m.mountpoint.clone(), src, snap)
        })
        .collect();

    // --- tight snapshot loop: no I/O between calls ---
    let mut opts = CreateSnapshotOptions::new();
    opts.readonly(true);
    for (_key, src, snap) in &sources {
        opts.create(src, snap)
            .with_context(|| format!("snapshot {src:?} -> {snap:?}"))?;
    }
    println!("created {} read-only snapshots", sources.len());

    // --- rustic backup + merge ---
    let repo_dir = "/tmp/rbtrfs-spine-repo";
    let _ = fs::remove_dir_all(repo_dir);
    let repo_opts = RepositoryOptions::default();
    let creds = Credentials::password("spikepw");
    let backends = BackendOptions::default()
        .repository(repo_dir)
        .to_backends()?;
    Repository::new(&repo_opts, &backends)?.init(
        &creds,
        &KeyOptions::default(),
        &ConfigOptions::default(),
    )?;

    let mut parts = Vec::new();
    for (mountpoint, _src, snap) in &sources {
        let repo = Repository::new(&repo_opts, &backends)?
            .open(&creds)?
            .to_indexed_ids()?;
        let s = SnapshotOptions::default()
            .add_tags("rbtrfs:part")?
            .to_snapshot()?;
        let bopts = BackupOptions::default().as_path(mountpoint.clone());
        let source = PathList::from_string(snap.to_str().unwrap())?.sanitize()?;
        parts.push(repo.backup(&bopts, &source, s)?);
    }
    let repo = Repository::new(&repo_opts, &backends)?
        .open(&creds)?
        .to_indexed()?;
    let cmp = |a: &Node, b: &Node| -> Ordering { a.meta.mtime.cmp(&b.meta.mtime) };
    let merged = repo.merge_snapshots(
        &parts,
        &cmp,
        SnapshotOptions::default()
            .add_tags("rbtrfs:merged")?
            .to_snapshot()?,
    )?;
    println!("merged {} paths={:?}", merged.id, merged.paths);

    drop(repo);
    let repo = Repository::new(&repo_opts, &backends)?
        .open(&creds)?
        .to_indexed()?;
    let node = repo.node_from_snapshot_path(&format!("{}", merged.id), |_| true)?;
    let want: Vec<String> = sources
        .iter()
        .map(|(mp, _, _)| mp.to_string_lossy().trim_start_matches('/').to_string())
        .collect();
    let mut found: Vec<String> = Vec::new();
    for e in repo.ls(&node, &LsOptions::default())? {
        let (p, _) = e?;
        let p = p.to_string_lossy().into_owned();
        if want.contains(&p) {
            found.push(p);
        }
    }

    println!("\n=== RESULT ===");
    println!("merged tree contains each source at its real path: want={want:?} found={found:?}");
    if found.len() != want.len() {
        bail!("merged tree wrong shape");
    }
    Ok(())
}

fn cleanup(host_before: &str) {
    // delete snapshots created under the (namespace-local) staging dir
    let staging = Path::new("/run/rbtrfs-spine-top/.rbtrfs-spine");
    if let Ok(rd) = fs::read_dir(staging) {
        for e in rd.flatten() {
            let _ = libbtrfsutil::delete_subvolume(e.path());
        }
    }
    let _ = fs::remove_dir_all(staging);
    let _ = umount("/run/rbtrfs-spine-top");

    // the mount ns dies with the process; assert the host table is unchanged
    let host_after = host_mounts_snapshot();
    println!(
        "host /proc/1/mounts unchanged: {}",
        host_before == host_after
    );
}

fn main() -> Result<()> {
    let host_before = host_mounts_snapshot();
    let r = run();
    cleanup(&host_before);
    match r {
        Ok(()) => {
            println!("SPIKE PASSED");
            Ok(())
        }
        Err(e) => {
            eprintln!("SPIKE FAILED: {e:#}");
            Err(e)
        }
    }
}
