//! Pre/post snapshot hook execution.

use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::{HookFailure, Hooks};
use crate::signals::Deferred;

/// Run `body` (the snapshot burst) between the pre- and post-hooks.
///
/// Post-hooks ALWAYS run, even if a pre-hook failed part-way or `body` errored:
/// an earlier pre-hook may already have held something back that only a post-hook
/// releases. Termination signals are deferred for the whole window and surface as an
/// error once the post-hooks have finished.
pub fn window<T>(hooks: &Hooks, body: impl FnOnce() -> Result<T>) -> Result<T> {
    let guard = Deferred::install().context("deferring termination signals")?;

    let pre = run_pre(hooks).context("pre-hooks");
    let result = match pre {
        Ok(()) if guard.interrupted() => {
            Err(anyhow::anyhow!("interrupted by signal before snapshot"))
        }
        Ok(()) => body(),
        Err(e) => Err(e),
    };
    let post = run_post(hooks).context("post-hooks");

    match (result, post) {
        (Ok(v), Ok(())) if guard.interrupted() => {
            drop(v);
            bail!("interrupted by signal after snapshot")
        }
        (Ok(v), Ok(())) => Ok(v),
        (Err(e), Ok(())) => Err(e),
        (Ok(_), Err(e)) => Err(e),
        (Err(e), Err(post_err)) => {
            Err(e.context(format!("(post-hooks also failed: {post_err:#})")))
        }
    }
}

pub fn run_pre(hooks: &Hooks) -> Result<()> {
    run_all("pre", &hooks.pre, hooks.on_failure)
}

/// Post hooks always attempt to run every command (so a release is not skipped
/// because an earlier post hook failed); failures still follow `on_failure`.
pub fn run_post(hooks: &Hooks) -> Result<()> {
    let mut first_err = None;
    for cmd in &hooks.post {
        if let Err(e) = run_one("post", cmd) {
            eprintln!("rbtrfs: post-hook failed: {e:#}");
            first_err.get_or_insert(e);
        }
    }
    match (first_err, hooks.on_failure) {
        (Some(e), HookFailure::Abort) => Err(e),
        _ => Ok(()),
    }
}

fn run_all(kind: &str, cmds: &[String], on_failure: HookFailure) -> Result<()> {
    for cmd in cmds {
        match (run_one(kind, cmd), on_failure) {
            (Ok(()), _) => {}
            (Err(e), HookFailure::Abort) => return Err(e),
            (Err(e), HookFailure::Warn) => {
                eprintln!("rbtrfs: {kind}-hook failed (continuing): {e:#}")
            }
        }
    }
    Ok(())
}

fn run_one(kind: &str, cmd: &str) -> Result<()> {
    let status = Command::new("sh").arg("-c").arg(cmd).status()?;
    if !status.success() {
        bail!("{kind}-hook `{cmd}` exited with {status}");
    }
    Ok(())
}
