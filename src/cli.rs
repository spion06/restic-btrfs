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
    },
    /// Delete local btrfs snapshots left by past (or crashed) runs.
    Gc {
        #[arg(long, default_value = "default")]
        profile: String,
        /// Override the profile's `keep_local`.
        #[arg(long)]
        keep_local: Option<usize>,
        /// Also sweep snapshot sets of subvolumes the profile no longer selects.
        #[arg(long)]
        all_keys: bool,
    },
}

impl Command {
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
        Command::Gc { profile, keep_local, all_keys } => {
            let p = cli.load_profile(profile)?;
            let keep = keep_local.unwrap_or(p.keep_local);
            let _lock = crate::lock::acquire()?;
            let report = gc::run(&p, keep, *all_keys).context("gc")?;
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
        Command::Restore { profile, snapshot, subvol, target, host, any_host } => {
            let p = cli.load_profile(profile)?;
            let host = match (host, any_host) {
                (_, true) => crate::restore::HostFilter::Any,
                (Some(h), _) => crate::restore::HostFilter::Named(h.clone()),
                _ => crate::restore::HostFilter::ThisHost,
            };
            crate::restore::restore(&p, snapshot, subvol, target, &host)
        }
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
