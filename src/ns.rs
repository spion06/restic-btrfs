//! Private mount namespace for the transient top-level-subvolume mount.
//!
//! MUST run while the process is single-threaded — before rustic_core's thread
//! pool, any async runtime, or rayon. `CLONE_NEWNS` moves only the calling thread;
//! threads spawned afterwards inherit the new namespace (verified in
//! `spikes/mount_ns.rs`), threads spawned before do not.

use std::path::Path;

use anyhow::{Context, Result};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};

/// Enter a new mount namespace and make the whole tree private so nothing we
/// mount can propagate back to the host.
pub fn enter_private_namespace() -> Result<()> {
    unshare(CloneFlags::CLONE_NEWNS).context("unshare(CLONE_NEWNS)")?;
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .context("make / private in new namespace")?;
    Ok(())
}

/// A btrfs mount that unmounts itself on drop. Only meaningful inside a private
/// namespace; the kernel also tears it down when the process exits.
pub struct TransientMount {
    target: std::path::PathBuf,
}

/// Mount target for the top-level subvolume of the filesystem identified by
/// mountinfo `dev` (`major:minor`), under rbtrfs' runtime dir.
pub fn mount_target(dev: &str) -> std::path::PathBuf {
    crate::lock::run_dir().join("mnt").join(dev.replace(':', "_"))
}

impl TransientMount {
    /// Mount the top-level subvolume (`subvolid=5`) of `source` at `target`.
    /// `target` is created if missing.
    pub fn top_level(source: &str, target: &Path) -> Result<Self> {
        std::fs::create_dir_all(target)
            .with_context(|| format!("creating mount target {}", target.display()))?;
        mount(
            Some(source),
            target,
            Some("btrfs"),
            MsFlags::empty(),
            Some("subvolid=5"),
        )
        .with_context(|| format!("mount {source} subvolid=5 at {}", target.display()))?;
        Ok(Self { target: target.to_path_buf() })
    }

    pub fn path(&self) -> &Path {
        &self.target
    }
}

impl Drop for TransientMount {
    fn drop(&mut self) {
        let _ = umount2(&self.target, MntFlags::MNT_DETACH);
        // Only succeeds once the (private) mount is gone; leaves no stray dir in /run.
        let _ = std::fs::remove_dir(&self.target);
    }
}

/// The configured repository filesystem, mounted for the lifetime of the value.
/// Like [`TransientMount`] it only makes sense in a private namespace, and the
/// kernel tears it down with the namespace even on SIGKILL.
pub struct RepositoryMount {
    target: std::path::PathBuf,
}

impl RepositoryMount {
    /// Run `mount -t <type> [-o <options>] <source> <target>`. The `mount(8)`
    /// binary (not `mount(2)`) is used so helpers such as `mount.nfs`, which
    /// resolve hostnames and negotiate options, are available.
    pub fn mount(spec: &crate::config::RepositoryMount) -> Result<Self> {
        std::fs::create_dir_all(&spec.target)
            .with_context(|| format!("creating mount target {}", spec.target.display()))?;
        let mut cmd = std::process::Command::new("mount");
        cmd.arg("-t").arg(&spec.fstype);
        if let Some(o) = spec.options.as_deref().filter(|o| !o.is_empty()) {
            cmd.arg("-o").arg(o);
        }
        cmd.arg(&spec.source).arg(&spec.target);
        let out = cmd.output().context("running mount(8) (is util-linux installed?)")?;
        if !out.status.success() {
            anyhow::bail!(
                "mounting {} ({}) at {} failed: {}",
                spec.source,
                spec.fstype,
                spec.target.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Self { target: spec.target.clone() })
    }
}

impl Drop for RepositoryMount {
    fn drop(&mut self) {
        let _ = umount2(&self.target, MntFlags::MNT_DETACH);
        let _ = std::fs::remove_dir(&self.target);
    }
}
