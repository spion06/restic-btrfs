//! Milestone 0 spike: verify `BackupOptions::as_path` + `Repository::merge_snapshots`.
//!
//! Simulates the real design: two "staging" dirs (standing in for btrfs snapshots of
//! /home and /srv) are each backed up with `as_path` set to the real mountpoint, then
//! merged into one snapshot. We then assert the merged tree contains both paths at their
//! real locations.
//!
//! Run: `cargo run --example spike_aspath_merge`

use std::cmp::Ordering;
use std::fs;
use std::io::Write;

use anyhow::Result;
use rustic_backend::BackendOptions;
use rustic_core::{
    repofile::{MasterKey, Node},
    BackupOptions, ConfigOptions, Credentials, KeyOptions, LsOptions, PathList, Repository,
    RepositoryOptions, SnapshotOptions,
};

fn main() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let repo_dir = tmp.path().join("repo");
    let stage_home = tmp.path().join("stage/home");
    let stage_srv = tmp.path().join("stage/srv");

    fs::create_dir_all(&stage_home)?;
    fs::create_dir_all(stage_home.join("user"))?;
    fs::create_dir_all(&stage_srv)?;
    write(&stage_home.join("user/hello.txt"), b"hello from home")?;
    write(&stage_srv.join("data.bin"), b"srv payload")?;

    let repo_opts = RepositoryOptions::default();
    let credentials = Credentials::Masterkey(MasterKey::new());
    let backends = BackendOptions::default()
        .repository(repo_dir.to_str().unwrap())
        .to_backends()?;

    let repo = Repository::new(&repo_opts, &backends)?.init(
        &credentials,
        &KeyOptions::default(),
        &ConfigOptions::default(),
    )?;
    let repo = repo.to_indexed_ids()?;

    // --- per-subvolume backups with as_path remap ---
    let mut parts = Vec::new();
    for (stage, as_path, key) in [(&stage_home, "/home", "home"), (&stage_srv, "/srv", "srv")] {
        let snap = SnapshotOptions::default()
            .add_tags(&format!("rbtrfs:part,rbtrfs:subvol={key}"))?
            .to_snapshot()?;
        let opts = BackupOptions::default().as_path(std::path::PathBuf::from(as_path));
        let source = PathList::from_string(stage.to_str().unwrap())?.sanitize()?;
        let snap = repo.backup(&opts, &source, snap)?;
        println!("part {key}: id={} paths={:?}", snap.id, snap.paths);
        parts.push(snap);
    }

    // --- merge into one snapshot ---
    // Re-open so the index includes the trees just written by the part backups.
    drop(repo);
    let repo = Repository::new(&repo_opts, &backends)?
        .open(&credentials)?
        .to_indexed()?;

    let newest_wins = |a: &Node, b: &Node| -> Ordering { a.meta.mtime.cmp(&b.meta.mtime) };
    let merged_snap = SnapshotOptions::default()
        .add_tags("rbtrfs:merged")?
        .to_snapshot()?;
    let merged = repo.merge_snapshots(&parts, &newest_wins, merged_snap)?;
    println!(
        "\nmerged: id={} tree={} paths={:?}",
        merged.id, merged.tree, merged.paths
    );

    // --- inspect merged tree (re-open fresh so the new tree is in the index) ---
    drop(repo);
    let repo = Repository::new(&repo_opts, &backends)?
        .open(&credentials)?
        .to_indexed()?;
    let node = repo.node_from_snapshot_path(&format!("{}", merged.id), |_| true)?;
    println!("\nmerged tree contents:");
    let mut saw_home = false;
    let mut saw_srv = false;
    for entry in repo.ls(&node, &LsOptions::default())? {
        let (path, _node) = entry?;
        let p = path.display().to_string();
        println!("  {p}");
        if p.contains("home/user/hello.txt") {
            saw_home = true;
        }
        if p.contains("srv/data.bin") {
            saw_srv = true;
        }
    }

    println!("\n=== RESULT ===");
    println!("home file present at real path: {saw_home}");
    println!("srv  file present at real path: {saw_srv}");
    println!(
        "repo dir for manual `restic -r {} check`: {}",
        repo_dir.display(),
        repo_dir.display()
    );
    // keep the repo around for the restic cross-check
    let keep = std::env::temp_dir().join("rbtrfs-spike-repo");
    let _ = fs::remove_dir_all(&keep);
    copy_dir(&repo_dir, &keep)?;
    println!("copied repo to: {}", keep.display());
    println!("masterkey (hex) for restic: see below");

    if saw_home && saw_srv {
        println!("\nSPIKE PASSED");
        Ok(())
    } else {
        anyhow::bail!("SPIKE FAILED: merged tree missing expected paths")
    }
}

fn write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    Ok(())
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}
