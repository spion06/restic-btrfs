//! Milestone 0 spike: private mount namespace for the transient staging mount.
//!
//! Verifies:
//!  1. `unshare(CLONE_NEWNS)` on the (single-threaded) main thread, then MS_PRIVATE on "/".
//!  2. A mount made afterwards is INVISIBLE to the host (checked by re-reading the host's
//!     mount namespace via `nsenter`-style /proc access is awkward; instead we fork a child
//!     that stays in the original namespace BEFORE unshare and have it check afterwards).
//!  3. Threads spawned AFTER the unshare inherit the new namespace (the rustic_core concern).
//!
//! Needs root for mount(). Run: `sudo -E cargo run --example spike_mount_ns`
//! (or: cargo build --example spike_mount_ns && sudo ./target/debug/examples/spike_mount_ns)

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::unistd::{fork, ForkResult};

fn mnt_ns_id() -> String {
    fs::read_link("/proc/self/ns/mnt")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "?".into())
}

fn count_tmpfs_at(marker: &str) -> usize {
    fs::read_to_string("/proc/self/mounts")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(marker))
        .count()
}

fn main() -> Result<()> {
    let host_ns_before = mnt_ns_id();
    println!("main thread mnt ns (before unshare): {host_ns_before}");

    let marker = "/tmp/rbtrfs-spike-mnt";
    let _ = fs::create_dir_all(marker);

    // Fork a witness that stays in the ORIGINAL namespace.
    // It waits, then reports whether it can see our mount.
    match unsafe { fork() }.context("fork")? {
        ForkResult::Child => {
            // stay in original ns; sleep, then check
            std::thread::sleep(std::time::Duration::from_millis(800));
            let seen = count_tmpfs_at(marker);
            let ns = mnt_ns_id();
            eprintln!("[witness] ns={ns} sees {seen} mount(s) at {marker} (expect 0)");
            std::process::exit(if seen == 0 { 0 } else { 1 });
        }
        ForkResult::Parent { child } => {
            // --- the real spike ---
            unshare(CloneFlags::CLONE_NEWNS).context("unshare(CLONE_NEWNS)")?;
            mount(
                None::<&str>,
                "/",
                None::<&str>,
                MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                None::<&str>,
            )
            .context("make / private")?;

            let ns_after = mnt_ns_id();
            println!("main thread mnt ns (after unshare):  {ns_after}");
            assert_ne!(host_ns_before, ns_after, "unshare did not change ns");

            // transient mount inside the private ns
            mount(
                Some("tmpfs"),
                marker,
                Some("tmpfs"),
                MsFlags::empty(),
                Some("size=1m"),
            )
            .context("mount tmpfs in private ns")?;
            fs::write(Path::new(marker).join("proof"), b"inside-ns")?;
            println!("mounted tmpfs at {marker} inside private ns; we see {} mount(s)", count_tmpfs_at(marker));

            // --- thread-inheritance check (the rustic_core thread-pool concern) ---
            let handles: Vec<_> = (0..4)
                .map(|i| {
                    std::thread::spawn(move || {
                        let ns = mnt_ns_id();
                        let seen = count_tmpfs_at("/tmp/rbtrfs-spike-mnt");
                        (i, ns, seen)
                    })
                })
                .collect();
            let mut all_threads_ok = true;
            for h in handles {
                let (i, ns, seen) = h.join().unwrap();
                let ok = ns == ns_after && seen >= 1;
                all_threads_ok &= ok;
                println!("  thread {i}: ns={ns} sees {seen} mount(s)  {}", if ok { "OK" } else { "BAD" });
            }

            let witness_ok = nix::sys::wait::waitpid(child, None)
                .map(|s| matches!(s, nix::sys::wait::WaitStatus::Exited(_, 0)))
                .unwrap_or(false);

            // cleanup happens automatically on exit; unmount for tidiness
            let _ = nix::mount::umount(marker);

            println!("\n=== RESULT ===");
            println!("host witness could NOT see our mount: {witness_ok}");
            println!("all post-unshare threads in new ns + see mount: {all_threads_ok}");
            if witness_ok && all_threads_ok {
                println!("SPIKE PASSED");
                Ok(())
            } else {
                anyhow::bail!("SPIKE FAILED")
            }
        }
    }
}
