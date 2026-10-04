//! rbtrfs entry point.
//!
//! Ordering matters: parse args (single-threaded, spawns nothing), then — for
//! commands that need it — enter a private mount namespace BEFORE any code path
//! that could start a thread pool (rustic_core) or async runtime.

use anyhow::{Context, Result};
use clap::Parser;
use std::process::ExitCode;

use rbtrfs::cli::{Cli, Command};

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    // Reading the config spawns no threads, so it is safe before the namespace.
    let profile = cli.profile()?;

    if cli.command.needs_root(profile.as_ref()) && !rbtrfs::is_root() {
        anyhow::bail!("`{}` needs root", command_name(&cli.command));
    }
    if let Some(p) = profile
        .as_ref()
        .filter(|_| cli.command.is_background_work())
    {
        // Before the namespace and any threads: a re-exec replaces the process.
        rbtrfs::priority::reexec_in_scope(p)?;
        rbtrfs::priority::apply_in_process(p);
    }
    if cli.command.needs_namespace(profile.as_ref()) {
        rbtrfs::ns::enter_private_namespace().context("entering private mount namespace")?;
    }

    if cli.command.is_read_only_output() {
        // Reading commands behave like other unix filters: `rbtrfs ls | head` ends
        // quietly when the reader closes the pipe. Not done for backup/gc/forget,
        // which must never be killed half-way by a closed stdout.
        // SAFETY: restoring the default disposition of SIGPIPE; no handler is involved.
        let _ = unsafe {
            nix::sys::signal::signal(
                nix::sys::signal::Signal::SIGPIPE,
                nix::sys::signal::SigHandler::SigDfl,
            )
        };
    }

    rbtrfs::logging::init();
    rbtrfs::cli::run(cli, profile)?;
    Ok(exit_code())
}

/// 0, unless the backup engine skipped entries. Files that vanished while a live
/// directory was read give 3, like restic. Anything else that could not be read means
/// the backup is incomplete, which is a failure.
fn exit_code() -> ExitCode {
    let (gone, bad) = (rbtrfs::logging::vanished(), rbtrfs::logging::unreadable());
    if bad > 0 {
        eprintln!("rbtrfs: error: {bad} file(s) could not be read; the backup is incomplete");
        return ExitCode::from(1);
    }
    if gone > 0 {
        eprintln!(
            "rbtrfs: {gone} file(s) vanished while they were being read; the rest was backed up"
        );
        return ExitCode::from(3);
    }
    ExitCode::SUCCESS
}

fn command_name(c: &Command) -> &'static str {
    match c {
        Command::Discover { .. } => "discover",
        Command::Completions { .. } => "completions",
        Command::Man => "man",
        Command::Gendocs { .. } => "gendocs",
        Command::Backup { .. } => "backup",
        Command::Snapshots { .. } => "snapshots",
        Command::Restore { .. } => "restore",
        Command::Ls { .. } => "ls",
        Command::Dump { .. } => "dump",
        Command::Forget { .. } => "forget",
        Command::Gc { .. } => "gc",
        Command::Init { .. } => "init",
    }
}
