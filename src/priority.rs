//! Keep long-running maintenance (backup, forget, gc) from getting in the way of
//! interactive use.
//!
//! Two layers, because neither is enough on its own:
//!
//! - **nice and I/O priority**, set in-process. They are inherited by everything
//!   rbtrfs starts (rclone, hooks). `nice` only competes with processes in the same
//!   cgroup, so it does not protect, say, a game that runs in its own systemd scope.
//! - **cgroup weights** (`CPUWeight`, `IOWeight`), applied by re-executing under
//!   `systemd-run --scope`. A cgroup with a low weight yields to every other cgroup.
//!   Skipped inside a systemd service (set the weights on the unit instead), without
//!   systemd, or when not root.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{IoPriority, Profile};

/// Set by the re-executed process so it does not wrap itself a second time.
const GUARD_ENV: &str = "RBTRFS_IN_SCOPE";

/// `ioprio_set(2)` constants (see `ionice(1)`).
const IOPRIO_WHO_PROCESS: i32 = 1;
const IOPRIO_CLASS_BE: i32 = 2;
const IOPRIO_CLASS_IDLE: i32 = 3;
const IOPRIO_CLASS_SHIFT: i32 = 13;

/// The `ioprio_set` value for a setting, or `None` to leave the priority alone.
pub fn ioprio_value(p: IoPriority) -> Option<i32> {
    match p {
        IoPriority::Normal => None,
        // lowest best-effort priority: still gets served, but after everything else
        IoPriority::Low => Some((IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | 7),
        // only when the disk is otherwise idle; can starve under constant load
        IoPriority::Idle => Some(IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT),
    }
}

/// Apply nice and I/O priority to this process (and so to its children).
pub fn apply_in_process(profile: &Profile) {
    if profile.nice != 0 {
        // SAFETY: plain syscall with integer arguments.
        let r = unsafe { nix::libc::setpriority(nix::libc::PRIO_PROCESS, 0, profile.nice) };
        if r != 0 {
            eprintln!(
                "rbtrfs: warning: could not set nice {}: {}",
                profile.nice,
                std::io::Error::last_os_error()
            );
        }
    }
    if let Some(v) = ioprio_value(profile.io_priority) {
        // SAFETY: plain syscall with integer arguments.
        let r = unsafe { nix::libc::syscall(nix::libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, v) };
        if r != 0 {
            eprintln!(
                "rbtrfs: warning: could not set I/O priority: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// The `systemd-run` arguments that run `exe args...` in a scope with the given
/// weights (0 leaves that weight alone). `None` if there is nothing to set.
pub fn scope_args(
    exe: &Path,
    args: &[OsString],
    cpu_weight: u32,
    io_weight: u32,
) -> Option<Vec<OsString>> {
    if cpu_weight == 0 && io_weight == 0 {
        return None;
    }
    let mut v: Vec<OsString> = vec!["--scope".into(), "--quiet".into()];
    if cpu_weight > 0 {
        v.push("-p".into());
        v.push(format!("CPUWeight={cpu_weight}").into());
    }
    if io_weight > 0 {
        v.push("-p".into());
        v.push(format!("IOWeight={io_weight}").into());
    }
    v.push("--".into());
    v.push(exe.as_os_str().to_owned());
    v.extend(args.iter().cloned());
    Some(v)
}

/// Is a transient scope available and appropriate here?
fn can_use_scope() -> bool {
    crate::is_root()
        && std::env::var_os(GUARD_ENV).is_none()
        // already a systemd service: its own unit settings apply
        && std::env::var_os("INVOCATION_ID").is_none()
        && Path::new("/run/systemd/system").exists()
}

/// Re-execute under a low-weight systemd scope. Returns only if that was not
/// possible (the caller carries on unweighted); on success it does not return.
pub fn reexec_in_scope(profile: &Profile) -> Result<()> {
    if !can_use_scope() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("finding our own executable")?;
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(sargs) = scope_args(&exe, &args, profile.cpu_weight, profile.io_weight) else {
        return Ok(());
    };
    let err = std::process::Command::new("systemd-run")
        .args(sargs)
        .env(GUARD_ENV, "1")
        .exec(); // only returns on failure
    eprintln!("rbtrfs: warning: could not start a systemd scope for CPU/IO weights: {err}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_priority_values_match_ionice() {
        assert_eq!(ioprio_value(IoPriority::Normal), None);
        assert_eq!(ioprio_value(IoPriority::Low), Some((2 << 13) | 7));
        assert_eq!(ioprio_value(IoPriority::Idle), Some(3 << 13));
    }

    #[test]
    fn scope_command_line() {
        let exe = Path::new("/usr/bin/rbtrfs");
        let args: Vec<OsString> = vec!["backup".into(), "--profile".into(), "x".into()];
        let got = scope_args(exe, &args, 20, 30).unwrap();
        let got: Vec<_> = got.iter().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(
            got,
            [
                "--scope",
                "--quiet",
                "-p",
                "CPUWeight=20",
                "-p",
                "IOWeight=30",
                "--",
                "/usr/bin/rbtrfs",
                "backup",
                "--profile",
                "x"
            ]
        );
        // a weight of 0 leaves that knob out, and both 0 means no scope at all
        let only_cpu = scope_args(exe, &args, 5, 0).unwrap();
        assert!(!only_cpu
            .iter()
            .any(|a| a.to_str().unwrap().starts_with("IOWeight")));
        assert!(scope_args(exe, &args, 0, 0).is_none());
    }
}
