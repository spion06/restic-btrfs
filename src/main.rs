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

    if cli.command.needs_namespace() {
        if !rbtrfs::is_root() {
            anyhow::bail!("`{}` needs root", command_name(&cli.command));
        }
        rbtrfs::ns::enter_private_namespace().context("entering private mount namespace")?;
    }

    rbtrfs::cli::run(cli)
}

fn command_name(c: &Command) -> &'static str {
    match c {
        Command::Discover { .. } => "discover",
        Command::Backup { .. } => "backup",
        Command::Snapshots { .. } => "snapshots",
        Command::Restore { .. } => "restore",
        Command::Gc { .. } => "gc",
    }
}
