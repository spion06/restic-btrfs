//! Command-line interface.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::btrfs::LibBtrfsUtil;
use crate::config::Config;
use crate::{backup, discover, gc};

#[derive(Parser, Debug)]
#[command(name = "rbtrfs", version, about = "Consistent btrfs-snapshot backups into a restic repo")]
pub struct Cli {
    /// Config file (default: $RBTRFS_CONFIG or /etc/rbtrfs/config.toml).
    #[arg(long, short, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    fn load_profile(&self, name: &str) -> Result<crate::config::Profile> {
        let path = self.config.clone().unwrap_or_else(Config::default_path);
        let cfg = Config::load(&path)?;
        cfg.profile(name).cloned()
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Show detected btrfs filesystems and their mounted subvolumes. Read-only.
    Discover {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Snapshot the selected subvolumes and back them up.
    Backup {
        #[arg(long, default_value = "default")]
        profile: String,
        /// Show what would happen without touching anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// List backup snapshots in the repository.
    Snapshots {
        #[arg(long, default_value = "default")]
        profile: String,
        #[arg(long)]
        all: bool,
    },
    /// Restore a subvolume from a snapshot into a plain directory.
    Restore {
        #[arg(long, default_value = "default")]
        profile: String,
        /// Snapshot id, or `latest`.
        snapshot: String,
        /// Which recorded subvolume path to restore, e.g. `/home`.
        #[arg(long)]
        subvol: PathBuf,
        /// Destination directory.
        #[arg(long)]
        target: PathBuf,
        /// `latest` considers snapshots from this host; name another host (e.g. when
        /// restoring onto a rebuilt machine)...
        #[arg(long, conflicts_with = "any_host")]
        host: Option<String>,
        /// ...or from any host.
        #[arg(long)]
        any_host: bool,
        /// Create TARGET as a new btrfs subvolume (root; TARGET must be on btrfs and
        /// must not exist) instead of a plain directory.
        #[arg(long)]
        as_subvolume: bool,
    },
    /// List the contents of a snapshot (default: everything).
    Ls {
        #[arg(long, default_value = "default")]
        profile: String,
        /// Snapshot id, or `latest`.
        snapshot: String,
        /// Recorded path to list, e.g. `/home/alice`.
        #[arg(default_value = "/")]
        path: PathBuf,
        #[arg(long, conflicts_with = "any_host")]
        host: Option<String>,
        #[arg(long)]
        any_host: bool,
    },
    /// Write one file from a snapshot to stdout.
    Dump {
        #[arg(long, default_value = "default")]
        profile: String,
        snapshot: String,
        /// Recorded path of the file, e.g. `/etc/fstab`.
        path: PathBuf,
        #[arg(long, conflicts_with = "any_host")]
        host: Option<String>,
        #[arg(long)]
        any_host: bool,
    },
    /// Apply the profile's `retention` to the repository: forget old merged
    /// snapshots and the part snapshots nothing needs any more.
    Forget {
        #[arg(long, default_value = "default")]
        profile: String,
        /// Also prune: delete data no remaining snapshot references.
        #[arg(long)]
        prune: bool,
        /// With --prune: delete unreferenced files immediately instead of marking them
        /// for later deletion. Only safe if no other restic/rustic process uses the
        /// repository right now.
        #[arg(long, requires = "prune")]
        instant_delete: bool,
        /// Show what would be forgotten, change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete local btrfs snapshots left by past (or crashed) runs.
    Gc {
        #[arg(long, default_value = "default")]
        profile: String,
        /// Override the profile's `keep_local`.
        #[arg(long)]
        keep_local: Option<usize>,
        /// Override the profile's `keep_local_days`.
        #[arg(long)]
        keep_local_days: Option<u64>,
        /// Also sweep snapshot sets of subvolumes the profile no longer selects.
        #[arg(long)]
        all_keys: bool,
    },
}

impl Command {
    /// `forget` mutates the repository and must hold the run lock (in /run), so
    /// it needs root even though it touches no btrfs.
    pub fn needs_root(&self) -> bool {
        self.needs_namespace() || matches!(self, Command::Forget { .. })
    }

    /// Does this command need a private mount namespace (and thus root)?
    pub fn needs_namespace(&self) -> bool {
        match self {
            // A dry run only reads mountinfo and the repository.
            Command::Backup { dry_run, .. } => !dry_run,
            Command::Gc { .. } => true,
            _ => false,
        }
    }
}

pub fn run(cli: Cli) -> Result<()> {
    match &cli.command {
        Command::Discover { json } => discover_cmd(*json),
        Command::Backup { profile, dry_run } => {
            let p = cli.load_profile(profile)?;
            let outcome = backup::run(&p, *dry_run).context("backup run")?;
            if !*dry_run {
                println!(
                    "done: run {} — {} part(s), merged {}, {} local snapshot(s) gc'd",
                    outcome.run_id, outcome.parts, outcome.merged.id, outcome.gc_deleted
                );
            }
            Ok(())
        }
        Command::Gc { profile, keep_local, keep_local_days, all_keys } => {
            let p = cli.load_profile(profile)?;
            let retention = crate::snapshot::LocalRetention::new(
                keep_local.unwrap_or(p.keep_local),
                keep_local_days.or(p.keep_local_days),
            );
            let _lock = crate::lock::acquire()?;
            let report = gc::run(&p, &retention, *all_keys).context("gc")?;
            for path in &report.deleted {
                println!("deleted {}", path.display());
            }
            println!("gc: deleted {} local snapshot(s)", report.deleted.len());
            report.warn();
            if !report.failed.is_empty() {
                anyhow::bail!("{} snapshot(s) could not be deleted", report.failed.len());
            }
            Ok(())
        }
        Command::Snapshots { profile, all } => {
            let p = cli.load_profile(profile)?;
            crate::restore::list(&p, *all)
        }
        Command::Restore { profile, snapshot, subvol, target, host, any_host, as_subvolume } => {
            let p = cli.load_profile(profile)?;
            let host = host_filter(host, *any_host);
            crate::restore::restore(&p, snapshot, subvol, target, &host, *as_subvolume)
        }
        Command::Ls { profile, snapshot, path, host, any_host } => {
            let p = cli.load_profile(profile)?;
            crate::restore::ls(&p, snapshot, path, &host_filter(host, *any_host))
        }
        Command::Dump { profile, snapshot, path, host, any_host } => {
            let p = cli.load_profile(profile)?;
            crate::restore::dump(&p, snapshot, path, &host_filter(host, *any_host))
        }
        Command::Forget { profile, prune, instant_delete, dry_run } => {
            let p = cli.load_profile(profile)?;
            let _lock = crate::lock::acquire()?;
            crate::forget::run(&p, *prune, *instant_delete, *dry_run)
        }
    }
}

fn host_filter(host: &Option<String>, any_host: bool) -> crate::restore::HostFilter {
    use crate::restore::HostFilter;
    match (host, any_host) {
        (_, true) => HostFilter::Any,
        (Some(h), _) => HostFilter::Named(h.clone()),
        _ => HostFilter::ThisHost,
    }
}

fn discover_cmd(json: bool) -> Result<()> {
    let filesystems = discover::discover()?;
    let btrfs = LibBtrfsUtil;
    let root = crate::is_root();

    if json {
        print_json(&filesystems, &btrfs, root);
        return Ok(());
    }

    if filesystems.is_empty() {
        println!("no btrfs filesystems mounted");
        return Ok(());
    }

    for fs in &filesystems {
        println!("filesystem {} ({})", fs.source, fs.dev);
        match fs.top_level_mount() {
            Some(m) => println!("  top-level subvolume mounted at {}", m.mount_point.display()),
            None => println!("  top-level subvolume not mounted (backup will mount it transiently)"),
        }
        println!("  mounts:");
        for m in &fs.mounts {
            println!(
                "    {:<28} subvol={} subvolid={}",
                m.mount_point.display(),
                m.subvol,
                m.subvolid.map(|i| i.to_string()).unwrap_or_else(|| "?".into()),
            );
        }

        if root {
            match fs.subvolumes(&btrfs) {
                Ok(subvols) => {
                    println!("  subvolumes ({}):", subvols.len());
                    for s in &subvols {
                        println!(
                            "    id={:<6} {}{}",
                            s.id,
                            s.path.display(),
                            if s.read_only { "  [ro]" } else { "" },
                        );
                    }
                }
                Err(e) => println!("  subvolume enumeration failed: {e:#}"),
            }
        } else {
            println!("  (run as root to enumerate all subvolumes)");
        }
        println!();
    }
    Ok(())
}

fn print_json(filesystems: &[discover::BtrfsFilesystem], btrfs: &dyn crate::btrfs::BtrfsOps, root: bool) {
    use serde_json::{json, Value};

    let out: Vec<Value> = filesystems
        .iter()
        .map(|fs| {
            let mounts: Vec<Value> = fs
                .mounts
                .iter()
                .map(|m| {
                    json!({
                        "mount_point": m.mount_point.to_string_lossy(),
                        "subvol": m.subvol,
                        "subvolid": m.subvolid,
                        "whole_subvolume": m.is_subvol_root(),
                    })
                })
                .collect();
            let mut v = json!({
                "dev": fs.dev,
                "source": fs.source,
                "top_level_mounted": fs.top_level_mount().is_some(),
                "mounts": mounts,
            });
            if root {
                if let Ok(subvols) = fs.subvolumes(btrfs) {
                    v["subvolumes"] = subvols
                        .iter()
                        .map(|s| {
                            json!({
                                "id": s.id,
                                "path": s.path.to_string_lossy(),
                                "read_only": s.read_only,
                                "uuid": s.uuid,
                            })
                        })
                        .collect();
                }
            }
            v
        })
        .collect();
    println!("{}", serde_json::to_string(&out).expect("json is serialisable"));
}
