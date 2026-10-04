//! rbtrfs entry point.
//!
//! Ordering matters: parse args (single-threaded, spawns nothing), then — for
//! commands that need it — enter a private mount namespace BEFORE any code path
//! that could start a thread pool (rustic_core) or async runtime.

use anyhow::{Context, Result};
use clap::Parser;

use rbtrfs::cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Reading the config spawns no threads, so it is safe before the namespace.
    let profile = cli.profile()?;

    if cli.command.needs_root(profile.as_ref()) && !rbtrfs::is_root() {
        anyhow::bail!("`{}` needs root", command_name(&cli.command));
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

    rbtrfs::cli::run(cli, profile)
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
    }
}
