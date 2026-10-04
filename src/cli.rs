//! Command-line interface.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::btrfs::LibBtrfsUtil;
use crate::config::Config;
use crate::{backup, discover, gc};

#[derive(Parser, Debug)]
#[command(
    name = "rbtrfs",
    version,
    about = "Back up btrfs subvolumes together into a restic repository",
    long_about = "rbtrfs snapshots a set of btrfs subvolumes together and stores them in a restic \
repository, with each subvolume under its real path. See https://github.com/spion06/restic-btrfs"
)]
pub struct Cli {
    /// Config file. Defaults to $RBTRFS_CONFIG, then /etc/rbtrfs/config.toml.
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

    /// The profile this command works on (`None` for `discover`). Loaded before
    /// the mount namespace decision, since a `repository_mount` makes every
    /// command that opens the repository need one.
    pub fn profile(&self) -> Result<Option<crate::config::Profile>> {
        use Command::*;
        match &self.command {
            Discover { .. } | Completions { .. } | Man | Gendocs { .. } => Ok(None),
            Backup { profile, .. }
            | Snapshots { profile, .. }
            | Restore { profile, .. }
            | Ls { profile, .. }
            | Dump { profile, .. }
            | Forget { profile, .. }
            | Gc { profile, .. } => self.load_profile(profile).map(Some),
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Show btrfs filesystems, mounts and subvolumes.
    ///
    /// Lists every mounted btrfs filesystem with its mounts, and, when run as
    /// root, all of its subvolumes. Use it to find the mount points to put in
    /// `subvolumes`. It changes nothing.
    Discover {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Snapshot the selected subvolumes and back them up.
    ///
    /// Runs the `pre` hooks, takes a read-only snapshot of every selected
    /// subvolume back to back, runs the `post` hooks, then backs the snapshots up
    /// into the repository as one snapshot with each subvolume at its real mount
    /// path. Finally it deletes local snapshots beyond `keep_local`. The
    /// repository is created on the first run.
    ///
    /// Needs root. Only one backup, gc or forget runs at a time on a machine.
    Backup {
        /// Profile to use from the config file.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Print the plan and check the repository and password, but change nothing.
        /// Does not need root.
        #[arg(long)]
        dry_run: bool,
    },
    /// List the backups in the repository.
    Snapshots {
        /// Profile to use from the config file.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Also list the internal per-subvolume snapshots that each backup is
        /// merged from.
        #[arg(long)]
        all: bool,
    },
    /// Restore one subvolume from a backup.
    ///
    /// Writes the files recorded under `--subvol` into `--target`. Run it as root
    /// to restore ownership. With `--as-subvolume` the target is created as a new
    /// btrfs subvolume; subvolumes that were nested inside the original come back
    /// as plain directories.
    Restore {
        /// Profile to use from the config file.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Snapshot id, or `latest` for the newest backup from this host.
        snapshot: String,
        /// Recorded path of the subvolume to restore, for example `/home`.
        #[arg(long)]
        subvol: PathBuf,
        /// Directory to restore into.
        #[arg(long)]
        target: PathBuf,
        /// Take `latest` from this host instead of the current one. Useful when
        /// restoring onto a rebuilt machine.
        #[arg(long, conflicts_with = "any_host")]
        host: Option<String>,
        /// Take `latest` from any host.
        #[arg(long)]
        any_host: bool,
        /// Create the target as a new btrfs subvolume. Needs root, and the target
        /// must be on btrfs and must not exist.
        #[arg(long)]
        as_subvolume: bool,
    },
    /// List the contents of a backup.
    Ls {
        /// Profile to use from the config file.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Snapshot id, or `latest` for the newest backup from this host.
        snapshot: String,
        /// Recorded path to list, for example `/home/alice`.
        #[arg(default_value = "/")]
        path: PathBuf,
        /// Take `latest` from this host instead of the current one.
        #[arg(long, conflicts_with = "any_host")]
        host: Option<String>,
        /// Take `latest` from any host.
        #[arg(long)]
        any_host: bool,
    },
    /// Write one file from a backup to standard output.
    Dump {
        /// Profile to use from the config file.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Snapshot id, or `latest` for the newest backup from this host.
        snapshot: String,
        /// Recorded path of the file, for example `/etc/fstab`.
        path: PathBuf,
        /// Take `latest` from this host instead of the current one.
        #[arg(long, conflicts_with = "any_host")]
        host: Option<String>,
        /// Take `latest` from any host.
        #[arg(long)]
        any_host: bool,
    },
    /// Apply the retention policy to the repository.
    ///
    /// Keeps merged backups according to the profile's `retention` table, per
    /// host, and removes the internal per-subvolume snapshots that no future run
    /// needs. Snapshots that rbtrfs did not create are never touched. Does nothing
    /// if the profile has no `retention` table.
    ///
    /// Needs root, because it takes the same lock as `backup`.
    Forget {
        /// Profile to use from the config file.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Also delete data that no remaining snapshot references. By default
        /// rustic only marks it and removes it on a later prune.
        #[arg(long)]
        prune: bool,
        /// With `--prune`, delete unreferenced data immediately. This skips
        /// rustic's two-phase pruning and can corrupt the repository if anything
        /// else is using it. On a terminal you are asked to confirm.
        #[arg(long, requires = "prune")]
        instant_delete: bool,
        /// Confirm `--instant-delete` without asking. Required when not running
        /// on a terminal.
        #[arg(long, requires = "instant_delete")]
        allow_unsafe: bool,
        /// Show what would be forgotten and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete local snapshots left behind by old or crashed runs.
    ///
    /// A backup removes old local snapshots itself. Use this to reclaim ones left
    /// by a run that was killed, or to apply different `keep_local` settings.
    ///
    /// Needs root.
    Gc {
        /// Profile to use from the config file.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Keep this many of the newest snapshot sets per subvolume. Overrides
        /// the profile's `keep_local`.
        #[arg(long)]
        keep_local: Option<usize>,
        /// Also keep sets younger than this many days. Overrides the profile's
        /// `keep_local_days`.
        #[arg(long)]
        keep_local_days: Option<u64>,
        /// Also clean up subvolumes that the profile no longer selects.
        #[arg(long)]
        all_keys: bool,
    },
    /// Print a shell completion script.
    Completions {
        /// Shell to generate for: bash, zsh, fish, elvish or powershell.
        shell: clap_complete::Shell,
    },
    /// Print the man page.
    Man,
    /// Write the command reference as Markdown files (used to build the docs).
    #[command(hide = true)]
    Gendocs {
        /// Directory to write into.
        dir: PathBuf,
    },
}

impl Command {
    /// Commands that only print, so a closed pipe may simply end them.
    pub fn is_read_only_output(&self) -> bool {
        matches!(
            self,
            Command::Discover { .. }
                | Command::Snapshots { .. }
                | Command::Ls { .. }
                | Command::Dump { .. }
                | Command::Completions { .. }
                | Command::Man
        )
    }

    /// `forget` mutates the repository and must hold the run lock (in /run), so
    /// it needs root even though it touches no btrfs.
    pub fn needs_root(&self, profile: Option<&crate::config::Profile>) -> bool {
        self.needs_namespace(profile) || matches!(self, Command::Forget { .. })
    }

    /// Does this command need a private mount namespace (and thus root)? Backup
    /// and gc mount the top-level subvolume; any command that opens the
    /// repository needs one when the profile mounts the repository's filesystem.
    pub fn needs_namespace(&self, profile: Option<&crate::config::Profile>) -> bool {
        let mounts_repo = profile.is_some_and(|p| p.repository_mount.is_some());
        match self {
            Command::Backup { dry_run, .. } => !dry_run || mounts_repo,
            Command::Gc { .. } => true,
            Command::Discover { .. }
            | Command::Completions { .. }
            | Command::Man
            | Command::Gendocs { .. } => false,
            _ => mounts_repo,
        }
    }
}

pub fn run(cli: Cli, loaded: Option<crate::config::Profile>) -> Result<()> {
    // Mounted (privately) before anything opens the repository; unmounted on drop.
    let _repository_mount = match loaded.as_ref().and_then(|p| p.repository_mount.as_ref()) {
        Some(spec) => Some(crate::ns::RepositoryMount::mount(spec)?),
        None => None,
    };

    match &cli.command {
        Command::Discover { json } => discover_cmd(*json),
        Command::Completions { shell } => {
            let mut cmd = <Cli as clap::CommandFactory>::command();
            let mut buf = Vec::new();
            clap_complete::generate(*shell, &mut cmd, "rbtrfs", &mut buf);
            write_stdout(&buf)
        }
        Command::Gendocs { dir } => crate::gendocs::write_all(dir),
        Command::Man => {
            let mut buf = Vec::new();
            clap_mangen::Man::new(<Cli as clap::CommandFactory>::command()).render(&mut buf)?;
            write_stdout(&buf)
        }
        Command::Backup { dry_run, .. } => {
            let p = loaded.clone().expect("profile loaded for this command");
            let outcome = backup::run(&p, *dry_run).context("backup run")?;
            if !*dry_run {
                println!(
                    "done: run {} — {} part(s), merged {}, {} local snapshot(s) gc'd",
                    outcome.run_id, outcome.parts, outcome.merged.id, outcome.gc_deleted
                );
            }
            Ok(())
        }
        Command::Gc {
            keep_local,
            keep_local_days,
            all_keys,
            ..
        } => {
            let p = loaded.clone().expect("profile loaded for this command");
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
        Command::Snapshots { all, .. } => {
            let p = loaded.clone().expect("profile loaded for this command");
            crate::restore::list(&p, *all)
        }
        Command::Restore {
            snapshot,
            subvol,
            target,
            host,
            any_host,
            as_subvolume,
            ..
        } => {
            let p = loaded.clone().expect("profile loaded for this command");
            let host = host_filter(host, *any_host);
            crate::restore::restore(&p, snapshot, subvol, target, &host, *as_subvolume)
        }
        Command::Ls {
            snapshot,
            path,
            host,
            any_host,
            ..
        } => {
            let p = loaded.clone().expect("profile loaded for this command");
            crate::restore::ls(&p, snapshot, path, &host_filter(host, *any_host))
        }
        Command::Dump {
            snapshot,
            path,
            host,
            any_host,
            ..
        } => {
            let p = loaded.clone().expect("profile loaded for this command");
            crate::restore::dump(&p, snapshot, path, &host_filter(host, *any_host))
        }
        Command::Forget {
            prune,
            instant_delete,
            allow_unsafe,
            dry_run,
            ..
        } => {
            let p = loaded.clone().expect("profile loaded for this command");
            if *instant_delete && !*allow_unsafe && !*dry_run {
                use std::io::IsTerminal;
                let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
                if !interactive {
                    anyhow::bail!(
                        "--instant-delete bypasses rustic's two-phase pruning and can corrupt the \
                         repository if anything else uses it while it runs. Not running \
                         interactively, so pass --allow-unsafe to confirm, or drop --instant-delete \
                         (space is then freed by a later prune)"
                    );
                }
                if !confirm_unsafe(&mut std::io::stdin().lock(), &mut std::io::stderr())? {
                    anyhow::bail!("aborted");
                }
            }
            let _lock = crate::lock::acquire()?;
            crate::forget::run(&p, *prune, *instant_delete, *dry_run)
        }
    }
}

const UNSAFE_PROMPT: &str = "\
--instant-delete bypasses rustic's two-phase pruning. If any other process (a backup on
another host, restic, rustic) uses this repository while it runs, the repository can be
corrupted. Type `yes` to continue: ";

/// Ask on a terminal whether to proceed; only an exact `yes` confirms.
fn confirm_unsafe(
    input: &mut impl std::io::BufRead,
    out: &mut impl std::io::Write,
) -> Result<bool> {
    out.write_all(UNSAFE_PROMPT.as_bytes())?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim() == "yes")
}

/// Write generated text to stdout; a closed pipe (`| head`) is not an error.
fn write_stdout(buf: &[u8]) -> Result<()> {
    use std::io::Write;
    match std::io::stdout().write_all(buf) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        r => r.map_err(Into::into),
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
            Some(m) => println!(
                "  top-level subvolume mounted at {}",
                m.mount_point.display()
            ),
            None => {
                println!("  top-level subvolume not mounted (backup will mount it transiently)")
            }
        }
        println!("  mounts:");
        for m in &fs.mounts {
            println!(
                "    {:<28} subvol={} subvolid={}",
                m.mount_point.display(),
                m.subvol,
                m.subvolid
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "?".into()),
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

fn print_json(
    filesystems: &[discover::BtrfsFilesystem],
    btrfs: &dyn crate::btrfs::BtrfsOps,
    root: bool,
) {
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
    println!(
        "{}",
        serde_json::to_string(&out).expect("json is serialisable")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(answer: &str) -> (bool, String) {
        let mut out = Vec::new();
        let ok = confirm_unsafe(&mut answer.as_bytes(), &mut out).unwrap();
        (ok, String::from_utf8(out).unwrap())
    }

    #[test]
    fn only_an_exact_yes_confirms() {
        assert!(ask("yes\n").0);
        assert!(ask("  yes  \n").0);
        for no in ["y\n", "Yes\n", "no\n", "\n", ""] {
            assert!(!ask(no).0, "{no:?} must not confirm");
        }
    }

    #[test]
    fn the_prompt_explains_the_risk() {
        assert!(ask("no\n").1.contains("corrupted"));
    }
}

#[cfg(test)]
mod generated_docs {
    use super::*;

    #[test]
    fn man_page_and_completions_render() {
        let mut man = Vec::new();
        clap_mangen::Man::new(<Cli as clap::CommandFactory>::command())
            .render(&mut man)
            .unwrap();
        let man = String::from_utf8(man).unwrap();
        assert!(
            man.contains("rbtrfs") && man.contains("forget"),
            "man page lists subcommands"
        );

        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
        ] {
            let mut out = Vec::new();
            clap_complete::generate(
                shell,
                &mut <Cli as clap::CommandFactory>::command(),
                "rbtrfs",
                &mut out,
            );
            assert!(
                String::from_utf8(out).unwrap().contains("restore"),
                "{shell} completion"
            );
        }
    }

    #[test]
    fn they_need_neither_config_nor_root() {
        for c in [
            Command::Man,
            Command::Completions {
                shell: clap_complete::Shell::Bash,
            },
        ] {
            assert!(!c.needs_root(None) && !c.needs_namespace(None));
        }
    }
}
