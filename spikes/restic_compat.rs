//! Milestone 0 spike: does the official `restic` CLI accept a repo written by
//! rustic_core (including a `merge_snapshots` result)?
//!
//! Writes a password-protected repo to $RBTRFS_SPIKE_REPO (default /tmp/rbtrfs-compat-repo),
//! password "spikepw". Then run manually:
//!
//!   export RESTIC_REPOSITORY=/tmp/rbtrfs-compat-repo RESTIC_PASSWORD=spikepw
//!   restic snapshots
//!   restic check --read-data
//!   restic restore latest --target /tmp/rbtrfs-compat-restore
//!
//! Run: `cargo run --example spike_restic_compat`

use std::cmp::Ordering;
use std::fs;
use std::io::Write;

use anyhow::Result;
use rustic_backend::BackendOptions;
use rustic_core::{
    repofile::Node, BackupOptions, ConfigOptions, Credentials, KeyOptions, PathList, Repository,
    RepositoryOptions, SnapshotOptions,
};

fn main() -> Result<()> {
    let repo_dir =
        std::env::var("RBTRFS_SPIKE_REPO").unwrap_or_else(|_| "/tmp/rbtrfs-compat-repo".into());
    let _ = fs::remove_dir_all(&repo_dir);
    let tmp = tempfile::tempdir()?;
    let stage_home = tmp.path().join("home");
    let stage_srv = tmp.path().join("srv");
    fs::create_dir_all(stage_home.join("user"))?;
    fs::create_dir_all(&stage_srv)?;
    write(&stage_home.join("user/hello.txt"), b"hello from home")?;
    write(&stage_srv.join("data.bin"), &vec![0xabu8; 200_000])?;

    let repo_opts = RepositoryOptions::default();
    let credentials = Credentials::password("spikepw");
    let backends = BackendOptions::default()
        .repository(&repo_dir)
        .to_backends()?;

    Repository::new(&repo_opts, &backends)?.init(
        &credentials,
        &KeyOptions::default(),
        &ConfigOptions::default(),
    )?;

    let mut parts = Vec::new();
    for (stage, as_path, key) in [(&stage_home, "/home", "home"), (&stage_srv, "/srv", "srv")] {
        let repo = Repository::new(&repo_opts, &backends)?
            .open(&credentials)?
            .to_indexed_ids()?;
        let snap = SnapshotOptions::default()
            .add_tags(&format!("rbtrfs:part,rbtrfs:subvol={key}"))?
            .to_snapshot()?;
        let opts = BackupOptions::default().as_path(std::path::PathBuf::from(as_path));
        let source = PathList::from_string(stage.to_str().unwrap())?.sanitize()?;
        let snap = repo.backup(&opts, &source, snap)?;
        println!("part {key}: {}", snap.id);
        parts.push(snap);
    }

    let repo = Repository::new(&repo_opts, &backends)?
        .open(&credentials)?
        .to_indexed()?;
    let cmp = |a: &Node, b: &Node| -> Ordering { a.meta.mtime.cmp(&b.meta.mtime) };
    let merged = repo.merge_snapshots(
        &parts,
        &cmp,
        SnapshotOptions::default()
            .add_tags("rbtrfs:merged")?
            .to_snapshot()?,
    )?;
    println!("merged: {} paths={:?}", merged.id, merged.paths);

    println!("\nrepo: {repo_dir}  password: spikepw");
    Ok(())
}

fn write(p: &std::path::Path, b: &[u8]) -> Result<()> {
    fs::File::create(p)?.write_all(b)?;
    Ok(())
}
