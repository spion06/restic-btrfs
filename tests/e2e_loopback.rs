//! End-to-end tests on throwaway loopback btrfs filesystems.
//!
//! Plain `cargo test` runs them. They need root (loop devices, mounts, snapshot
//! ioctls), so when the test process is not root each test re-runs ITSELF inside
//! a privileged container (see `container`): no sudo, and every test gets its own
//! mount namespace and `/run`, so they run in parallel. Needs access to a Docker
//! API socket (user in the `docker` group, or `DOCKER_HOST`). When run as root
//! (CI) the tests run directly, serialised, and need `mkfs.btrfs`, `losetup`,
//! `setfattr`/`getfattr` and the official `restic` on PATH.
//!
//! `RBTRFS_E2E_REQUIRED=1` turns "no container runtime" from a skip into a failure.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

mod container {
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::Duration;

    use testcontainers::core::{wait::ExitWaitStrategy, ImageExt, Mount, WaitFor};
    use testcontainers::runners::{SyncBuilder, SyncRunner};
    use testcontainers::{GenericBuildableImage, GenericImage};

    const IMAGE: &str = "rbtrfs-e2e";
    const TAG: &str = "1";

    /// Everything the tests shell out to, on the same rolling glibc as the host
    /// toolchains this project targets (the test binary is bind-mounted in).
    const DOCKERFILE: &str = "FROM archlinux:latest\n\
        RUN pacman -Syu --noconfirm --needed btrfs-progs restic attr util-linux \
        && pacman -Scc --noconfirm\n";

    pub fn is_root() -> bool {
        nix::unistd::Uid::effective().is_root()
    }

    /// Build the image once per test process (Docker's layer cache makes later
    /// processes instant). `Err` means no usable container runtime.
    fn image() -> &'static Result<(), String> {
        static BUILT: OnceLock<Result<(), String>> = OnceLock::new();
        BUILT.get_or_init(|| {
            GenericBuildableImage::new(IMAGE, TAG)
                .with_dockerfile_string(DOCKERFILE)
                .build_image()
                .map(|_| ())
                .map_err(|e| format!("{e}"))
        })
    }

    /// Deepest directory containing both paths.
    fn common_dir(a: &Path, b: &Path) -> PathBuf {
        a.ancestors()
            .find(|d| b.starts_with(d))
            .unwrap_or(Path::new("/"))
            .to_path_buf()
    }

    /// Run test `name` of this test binary as root inside a privileged container.
    /// Returns `true` if the test was handled (passed, or skipped with a notice);
    /// panics if the test failed in the container.
    pub fn delegated(type_name: &str) -> bool {
        if is_root() {
            return false;
        }
        // "e2e_loopback::excludes_exclude::f" -> "excludes_exclude"
        let name = type_name
            .trim_end_matches("::f")
            .rsplit("::")
            .next()
            .unwrap();

        let exe = std::env::current_exe().unwrap();
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_rbtrfs"));
        let root = common_dir(&exe, &bin);
        let required = std::env::var_os("RBTRFS_E2E_REQUIRED").is_some();

        if let Err(e) = image() {
            if required {
                panic!("no usable container runtime (RBTRFS_E2E_REQUIRED set): {e}");
            }
            eprintln!(
                "SKIPPING {name}: not root and no usable container runtime ({e}).\n\
                 Add yourself to the `docker` group, set DOCKER_HOST, or run as root."
            );
            return true;
        }

        let container = GenericImage::new(IMAGE, TAG)
            .with_wait_for(WaitFor::exit(ExitWaitStrategy::new()))
            .with_privileged(true)
            // loop devices are created on demand by the host kernel; a bind-mounted
            // /dev is the only way their nodes show up inside the container
            .with_mount(Mount::bind_mount("/dev", "/dev"))
            .with_mount(Mount::bind_mount(
                root.to_string_lossy(),
                root.to_string_lossy(),
            ))
            // scratch images live in RAM: fast, and gone with the container
            .with_mount(Mount::tmpfs_mount("/tmp"))
            .with_env_var("RUST_BACKTRACE", "1")
            .with_startup_timeout(Duration::from_secs(900))
            .with_cmd([
                exe.to_string_lossy().into_owned(),
                "--exact".into(),
                name.into(),
                "--nocapture".into(),
            ])
            .start()
            .unwrap_or_else(|e| panic!("starting container for {name}: {e}"));

        let code = container.exit_code().unwrap();
        if code != Some(0) {
            let out = String::from_utf8_lossy(&container.stdout_to_vec().unwrap_or_default())
                .into_owned();
            let err = String::from_utf8_lossy(&container.stderr_to_vec().unwrap_or_default())
                .into_owned();
            panic!("{name} failed in container (exit {code:?})\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        }
        true
    }
}

/// First line of every e2e test: re-run in a container unless we are root.
macro_rules! e2e {
    () => {{
        fn f() {}
        if container::delegated(std::any::type_name_of_val(&f)) {
            return;
        }
    }};
}

/// Directly-run tests share /run/rbtrfs and the host-wide run lock: serialise.
fn serial() -> MutexGuard<'static, ()> {
    static M: Mutex<()> = Mutex::new(());
    M.lock().unwrap_or_else(|e| e.into_inner())
}

fn sh(cmd: &str) -> String {
    let out = Command::new("sh").arg("-c").arg(cmd).output().unwrap();
    assert!(
        out.status.success(),
        "command failed: {cmd}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn try_sh(cmd: &str) {
    let _ = Command::new("sh").arg("-c").arg(cmd).status();
}

fn text(out: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A 512M btrfs image on a loop device, mounted at `<base>/mnt` (the top level).
struct LoopFs {
    base: PathBuf,
    dev: String,
    _serial: MutexGuard<'static, ()>,
}

impl LoopFs {
    fn new(tag: &str) -> Self {
        assert!(
            container::is_root(),
            "e2e fixtures need root (e2e!() delegates to a container)"
        );
        let serial = serial();
        let base = std::env::temp_dir().join(format!("rbtrfs-it-{tag}"));
        Self::teardown(&base);
        fs::create_dir_all(&base).unwrap();
        let img = base.join("fs.img");
        sh(&format!("truncate -s 512M {}", img.display()));
        sh(&format!("mkfs.btrfs -qf {}", img.display()));
        let dev = sh(&format!("losetup --find --show {}", img.display()))
            .trim()
            .to_string();
        let mnt = base.join("mnt");
        fs::create_dir_all(&mnt).unwrap();
        sh(&format!("mount {dev} {}", mnt.display()));
        Self {
            base,
            dev,
            _serial: serial,
        }
    }

    /// Best-effort removal of everything under `base`, also from aborted runs.
    fn teardown(base: &Path) {
        // unmount deepest first (the top level plus every subvolume mount)
        try_sh(&format!(
            "for m in $(mount | awk '$3 ~ \"^{}\" {{print $3}}' | sort -r); do umount \"$m\"; done",
            base.display()
        ));
        for img in ["fs.img", "repo.img"] {
            try_sh(&format!(
                "losetup -j {}/{img} | cut -d: -f1 | xargs -r losetup -d",
                base.display()
            ));
        }
        let _ = fs::remove_dir_all(base);
    }
}

impl Drop for LoopFs {
    fn drop(&mut self) {
        Self::teardown(&self.base);
    }
}

/// A filesystem with some subvolumes mounted at "real" paths, plus an rbtrfs config.
struct Fx {
    fs: LoopFs,
    cfg: PathBuf,
    /// Mount points of the created subvolumes, in creation order.
    mounts: Vec<PathBuf>,
}

impl Fx {
    /// `subvols` are btrfs subvolume names; each is mounted at `<base>/s<i>`.
    fn new(tag: &str, subvols: &[&str]) -> Self {
        let fs = LoopFs::new(tag);
        let mut mounts = Vec::new();
        for (i, name) in subvols.iter().enumerate() {
            sh(&format!(
                "btrfs subvolume create {}/mnt/'{name}'",
                fs.base.display()
            ));
            let mp = fs.base.join(format!("s{i}"));
            fs::create_dir_all(&mp).unwrap();
            sh(&format!(
                "mount -o subvol='{name}' {} {}",
                fs.dev,
                mp.display()
            ));
            mounts.push(mp);
        }
        let cfg = fs.base.join("config.toml");
        let fx = Self { fs, cfg, mounts };
        fx.write_cfg(&fx.mounts.clone(), "");
        fx
    }

    fn base(&self) -> &Path {
        &self.fs.base
    }

    fn repo(&self) -> PathBuf {
        self.base().join("repo")
    }

    /// Write the config. `extra` is raw TOML appended to `[profile.default]`
    /// (before any `[profile.default.hooks]` table it contains).
    fn write_cfg(&self, subvolumes: &[PathBuf], extra: &str) {
        let list = subvolumes
            .iter()
            .map(|p| format!("\"{}\"", p.display()))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            &self.cfg,
            format!(
                "[profile.default]\nrepository = \"{}\"\npassword = \"pw\"\nsubvolumes = [{list}]\n{extra}\n",
                self.repo().display()
            ),
        )
        .unwrap();
    }

    /// Write `base` (a complete profile) followed by `extra` raw TOML.
    fn write_cfg_keep_repo_mount(&self, base: &str, extra: &str) {
        fs::write(&self.cfg, format!("{base}{extra}")).unwrap();
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_rbtrfs"));
        c.env("RBTRFS_CONFIG", &self.cfg).args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "rbtrfs {args:?} failed:\n{}",
            text(&out)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn restore(&self, snapshot: &str, subvol: &Path, name: &str) -> PathBuf {
        let target = self.base().join(name);
        self.ok(&[
            "restore",
            snapshot,
            "--subvol",
            &subvol.to_string_lossy(),
            "--target",
            &target.to_string_lossy(),
        ]);
        target
    }

    /// Run-id directories of local snapshots kept for `subvol_mount`, top-level staging.
    fn local_runs(&self, subvol_mount: &Path) -> Vec<String> {
        let key = rbtrfs::select::key_for(subvol_mount);
        list_dir(&self.base().join("mnt/.rbtrfs-snapshots").join(key))
    }

    /// Official restic against the repo rbtrfs wrote.
    fn restic(&self, args: &[&str]) -> String {
        let out = Command::new("restic")
            .env("RESTIC_PASSWORD", "pw")
            .arg("-r")
            .arg(self.repo())
            .args(args)
            .output()
            .expect("restic binary");
        assert!(
            out.status.success(),
            "restic {args:?} failed:\n{}",
            text(&out)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn restic_check(&self) {
        self.restic(&["check", "--read-data"]);
    }
}

/// Poll `cond` for up to `secs` seconds.
fn wait_until(secs: u64, what: &str, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn kill(sig: &str, pid: u32) {
    sh(&format!("kill -{sig} {pid}"));
}

fn list_dir(p: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Everything about a tree that a faithful backup must preserve.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    File {
        content: Vec<u8>,
        mode: u32,
        uid: u32,
        gid: u32,
        mtime: i64,
    },
    Dir {
        mode: u32,
        uid: u32,
        gid: u32,
    },
    Symlink(PathBuf),
}

fn walk(root: &Path) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let md = fs::symlink_metadata(&p).unwrap();
            let e = if md.file_type().is_symlink() {
                Entry::Symlink(fs::read_link(&p).unwrap())
            } else if md.is_dir() {
                stack.push(p);
                Entry::Dir {
                    mode: md.mode() & 0o7777,
                    uid: md.uid(),
                    gid: md.gid(),
                }
            } else {
                Entry::File {
                    content: fs::read(&p).unwrap(),
                    mode: md.mode() & 0o7777,
                    uid: md.uid(),
                    gid: md.gid(),
                    mtime: md.mtime(),
                }
            };
            out.insert(rel, e);
        }
    }
    out
}

fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    walk(root)
        .into_iter()
        .filter_map(|(k, v)| match v {
            Entry::File { content, .. } => Some((k, content)),
            _ => None,
        })
        .collect()
}

fn host_mounts() -> String {
    fs::read_to_string("/proc/self/mountinfo").unwrap()
}

// ---------------------------------------------------------------------------

#[test]
fn roundtrip_metadata_gc_and_cross_tool_check() {
    e2e!();
    // Awkward subvolume name; three subvolumes so `latest` must pick the merged
    // snapshot rather than whichever part ties with it.
    let fx = Fx::new("roundtrip", &["@we ird", "@data", "@third"]);
    let (a, b, c) = (&fx.mounts[0], &fx.mounts[1], &fx.mounts[2]);

    fs::create_dir_all(a.join("sub")).unwrap();
    fs::write(a.join("sub/hello.txt"), b"hello world").unwrap();
    fs::write(a.join("big.bin"), vec![7u8; 300_000]).unwrap();
    fs::write(b.join("data.json"), br#"{"k":1}"#).unwrap();
    fs::write(c.join("c.txt"), b"third").unwrap();

    // metadata that must survive: mode, ownership, mtime, symlink, xattr
    fs::write(a.join("private.txt"), b"secret").unwrap();
    sh(&format!(
        "chmod 640 '{0}/private.txt' && chown 1234:5678 '{0}/private.txt'",
        a.display()
    ));
    sh(&format!(
        "touch -d @1700000000 '{}/private.txt'",
        a.display()
    ));
    symlink("sub/hello.txt", a.join("link")).unwrap();
    sh(&format!(
        "setfattr -n user.rbtrfs -v marker '{}/sub/hello.txt'",
        a.display()
    ));

    let before = (walk(a), walk(b), walk(c));
    let mounts_before = host_mounts();

    fx.ok(&["discover"]);
    let json = fx.ok(&["discover", "--json"]);
    serde_json::from_str::<serde_json::Value>(&json).expect("discover --json is valid JSON");

    fx.ok(&["backup"]);
    fx.ok(&["backup"]); // second run: parent detection + GC

    assert_eq!(
        mounts_before,
        host_mounts(),
        "host mount table changed (namespace leak)"
    );
    // keep_local defaults to 1: exactly one local snapshot set per subvolume remains
    for mp in &fx.mounts {
        assert_eq!(
            fx.local_runs(mp).len(),
            1,
            "local sets for {}",
            mp.display()
        );
    }

    for (i, mp) in fx.mounts.iter().enumerate() {
        let restored = fx.restore("latest", mp, &format!("restore-{i}"));
        let want = [&before.0, &before.1, &before.2][i];
        assert_eq!(
            want,
            &walk(&restored),
            "subvolume {} did not round-trip",
            mp.display()
        );
    }
    let xattr = sh(&format!(
        "getfattr -n user.rbtrfs --only-values '{}/restore-0/sub/hello.txt'",
        fx.base().display()
    ));
    assert_eq!(xattr.trim(), "marker", "xattr lost");

    // the repository is plain restic
    fx.restic_check();
    let snaps = fx.restic(&["snapshots", "--json"]);
    assert!(snaps.contains("rbtrfs:part"), "parts present in repo");
}

#[test]
fn backup_reads_the_snapshot_not_the_live_subvolume() {
    e2e!();
    let fx = Fx::new("isolation", &["@a"]);
    let a = &fx.mounts[0];
    fs::write(a.join("f.txt"), b"ORIGINAL").unwrap();

    // The post-hook runs right after the snapshot burst and before the backup
    // reads anything: whatever it writes must NOT be in the backup.
    fx.write_cfg(
        &fx.mounts.clone(),
        &format!(
            "[profile.default.hooks]\npost = [\"echo CHANGED > '{0}/f.txt'; echo NEW > '{0}/new.txt'\"]\n",
            a.display()
        ),
    );
    fx.ok(&["backup"]);

    assert_eq!(
        fs::read_to_string(a.join("f.txt")).unwrap().trim(),
        "CHANGED",
        "hook ran"
    );
    let restored = fx.restore("latest", a, "restored");
    assert_eq!(fs::read(restored.join("f.txt")).unwrap(), b"ORIGINAL");
    assert!(
        !restored.join("new.txt").exists(),
        "post-snapshot file leaked into the backup"
    );
}

#[test]
fn excludes_exclude() {
    e2e!();
    let fx = Fx::new("excludes", &["@a"]);
    let a = &fx.mounts[0];
    for d in [
        ".cache",
        "u/.cache",
        "Downloads",
        "sub",
        "keepdir/node_modules",
    ] {
        fs::create_dir_all(a.join(d)).unwrap();
    }
    for f in [
        "keep.txt",
        "skip.tmp",
        ".cache/x",
        "u/.cache/y",
        "u/keep.txt",
        "Downloads/z",
        "sub/keep2",
        "sub/deep.tmp",
        "keepdir/node_modules/m",
        "keepdir/k",
    ] {
        fs::write(a.join(f), f.as_bytes()).unwrap();
    }
    fx.write_cfg(
        &fx.mounts.clone(),
        &format!(
            "exclude = [\"*.tmp\", \"**/.cache\", \"{}/Downloads\", \"node_modules/\"]\n",
            a.display()
        ),
    );
    // the dry run reports what the patterns skip, using the same matcher
    let plan = fx.ok(&["backup", "--dry-run"]);
    assert!(plan.contains("would store 4 files"), "{plan}");
    assert!(plan.contains(&format!("{}/.cache", a.display())), "{plan}");
    assert!(
        plan.contains(&format!("{}/Downloads", a.display())),
        "{plan}"
    );
    assert!(!fx
        .ok(&["backup", "--dry-run", "--no-scan"])
        .contains("would store"));
    fx.ok(&["backup"]);

    let restored = files(&fx.restore("latest", a, "restored"));
    let got: Vec<&str> = restored.keys().map(|s| s.as_str()).collect();
    assert_eq!(got, ["keep.txt", "keepdir/k", "sub/keep2", "u/keep.txt"]);
}

#[test]
fn second_run_is_incremental() {
    e2e!();
    let fx = Fx::new("incremental", &["@a"]);
    fs::write(fx.mounts[0].join("f.bin"), vec![3u8; 200_000]).unwrap();
    fx.ok(&["backup"]);
    fx.ok(&["backup"]);

    let snaps: serde_json::Value =
        serde_json::from_str(&fx.restic(&["snapshots", "--json"])).unwrap();
    let mut parts: Vec<&serde_json::Value> = snaps
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| {
            s["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "rbtrfs:part")
        })
        .collect();
    parts.sort_by_key(|s| s["time"].as_str().unwrap().to_string());
    assert_eq!(parts.len(), 2);
    assert!(
        parts[1]["parent"].is_string(),
        "second part has a parent: {}",
        parts[1]
    );
    assert_eq!(
        parts[1]["parent"], parts[0]["id"],
        "chained to the previous run's part"
    );
    let added = parts[1]["summary"]["data_added"].as_u64().unwrap();
    assert_eq!(added, 0, "unchanged data was re-read/re-added");

    // merged snapshots form a chain too
    let mut merged: Vec<&serde_json::Value> = snaps
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| {
            !s["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "rbtrfs:part")
        })
        .collect();
    merged.sort_by_key(|s| s["time"].as_str().unwrap().to_string());
    assert_eq!(merged.len(), 2);
    assert!(
        merged[0]["parent"].is_null(),
        "first merged snapshot has no parent"
    );
    assert_eq!(
        merged[1]["parent"], merged[0]["id"],
        "merged snapshots are chained"
    );
}

#[test]
fn failing_pre_hook_still_runs_post_hooks() {
    e2e!();
    let fx = Fx::new("hookfail", &["@a"]);
    let marker = fx.base().join("post-ran");
    fx.write_cfg(
        &fx.mounts.clone(),
        &format!(
            "[profile.default.hooks]\npre = [\"true\", \"false\"]\npost = [\"touch '{}'\"]\n",
            marker.display()
        ),
    );
    let out = fx.run(&["backup"]);
    assert!(
        !out.status.success(),
        "backup must fail when a pre-hook fails"
    );
    assert!(
        marker.exists(),
        "post-hook must run after a failed pre-hook:\n{}",
        text(&out)
    );
    assert!(fx.local_runs(&fx.mounts[0]).is_empty(), "no snapshot taken");
}

#[test]
fn nested_selected_subvolumes_merge_with_content_at_both_paths() {
    e2e!();
    let fx = Fx::new("nested", &["@outer", "@inner"]);
    let (outer, inner_src) = (&fx.mounts[0], &fx.mounts[1]);
    // mount @inner *inside* @outer
    sh(&format!("umount {}", inner_src.display()));
    let inner = outer.join("inner");
    fs::create_dir_all(&inner).unwrap();
    sh(&format!(
        "mount -o subvol=@inner {} {}",
        fx.fs.dev,
        inner.display()
    ));
    fs::write(outer.join("o.txt"), b"outer").unwrap();
    fs::write(inner.join("i.txt"), b"inner").unwrap();

    fx.write_cfg(&[outer.clone(), inner.clone()], "");
    let out = fx.run(&["backup"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("nested under a selected"),
        "no nested warning when both selected"
    );

    let restored = files(&fx.restore("latest", outer, "restored"));
    assert_eq!(
        restored.get("o.txt").map(|v| v.as_slice()),
        Some(&b"outer"[..])
    );
    assert_eq!(
        restored.get("inner/i.txt").map(|v| v.as_slice()),
        Some(&b"inner"[..])
    );

    // selecting only the outer one must warn that the inner one will be empty
    fx.write_cfg(std::slice::from_ref(outer), "");
    let out = fx.run(&["backup", "--dry-run"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nested under a selected"),
        "{}",
        text(&out)
    );
}

#[test]
fn in_subvolume_staging_backs_up_snapshots_and_excludes_staging_dir() {
    e2e!();
    let fx = Fx::new("insubvol", &["@a", "@b"]);
    let (a, b) = (&fx.mounts[0], &fx.mounts[1]);
    fs::write(a.join("f.txt"), b"ORIGINAL").unwrap();
    fs::write(b.join("g.txt"), b"bee").unwrap();
    fx.write_cfg(
        &fx.mounts.clone(),
        &format!(
            "staging = \"in-subvolume\"\nkeep_local = 1\n\
             [profile.default.hooks]\npost = [\"echo CHANGED > '{}/f.txt'\"]\n",
            a.display()
        ),
    );
    fx.ok(&["backup"]);
    fx.ok(&["backup"]);

    // snapshot isolation holds in this mode too (run 2 saw CHANGED as its baseline,
    // then the hook changed it again after the burst)
    let restored = fx.restore("latest", a, "restored-a");
    assert_eq!(
        fs::read_to_string(restored.join("f.txt")).unwrap().trim(),
        "CHANGED"
    );
    assert!(
        !restored.join(".rbtrfs-snapshots").exists(),
        "staging dir leaked into the backup: {:?}",
        list_dir(&restored)
    );
    let restored_b = fx.restore("latest", b, "restored-b");
    assert_eq!(files(&restored_b).keys().collect::<Vec<_>>(), ["g.txt"]);

    // backup-time GC works: exactly one local set per subvolume
    for mp in &fx.mounts {
        let key = rbtrfs::select::key_for(mp);
        let runs = list_dir(&mp.join(".rbtrfs-snapshots").join(key));
        assert_eq!(
            runs.len(),
            1,
            "in-subvolume GC left {runs:?} for {}",
            mp.display()
        );
    }
    fx.restic_check();
}

#[test]
fn standalone_gc_reclaims_old_sets() {
    e2e!();
    let fx = Fx::new("gc", &["@a"]);
    let a = &fx.mounts[0];
    fx.write_cfg(&fx.mounts.clone(), "keep_local = 3\n");
    for _ in 0..3 {
        fx.ok(&["backup"]);
        std::thread::sleep(Duration::from_millis(1100)); // run ids have 1s resolution
    }
    assert_eq!(fx.local_runs(a).len(), 3);
    fx.ok(&["gc", "--keep-local", "1"]);
    assert_eq!(fx.local_runs(a).len(), 1);
}

#[test]
fn concurrent_runs_are_refused() {
    e2e!();
    let fx = Fx::new("lock", &["@a"]);
    fx.write_cfg(
        &fx.mounts.clone(),
        "[profile.default.hooks]\npre = [\"sleep 4\"]\n",
    );
    let mut first = fx.cmd(&["backup"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let second = fx.run(&["backup"]);
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("another rbtrfs run"),
        "{}",
        text(&second)
    );
    assert!(first.wait().unwrap().success(), "first run unaffected");
}

#[test]
fn wrong_password_is_an_error_not_a_reinit() {
    e2e!();
    let fx = Fx::new("badpw", &["@a"]);
    fs::write(fx.mounts[0].join("f"), b"x").unwrap();
    fx.ok(&["backup"]);
    let config_before = fs::read(fx.repo().join("config")).unwrap();
    let keys_before = list_dir(&fx.repo().join("keys"));

    let cfg = fs::read_to_string(&fx.cfg)
        .unwrap()
        .replace("password = \"pw\"", "password = \"nope\"");
    fs::write(&fx.cfg, cfg).unwrap();
    let out = fx.run(&["backup"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("initializing repository"),
        "must not try to init: {err}"
    );
    assert_eq!(config_before, fs::read(fx.repo().join("config")).unwrap());
    assert_eq!(keys_before, list_dir(&fx.repo().join("keys")));
    assert!(
        fx.local_runs(&fx.mounts[0]).len() <= 1,
        "no extra snapshot taken before failing"
    );
}

#[test]
fn latest_ignores_other_hosts_unless_asked() {
    e2e!();
    let fx = Fx::new("hosts", &["@a"]);
    let a = &fx.mounts[0];
    fs::write(a.join("f"), b"x").unwrap();
    fx.ok(&["backup"]);
    // pretend the repo was written by another machine
    fx.restic(&["rewrite", "--forget", "--new-host", "other-machine"]);

    let target = fx.base().join("r1");
    let out = fx.run(&[
        "restore",
        "latest",
        "--subvol",
        &a.to_string_lossy(),
        "--target",
        &target.to_string_lossy(),
    ]);
    assert!(
        !out.status.success(),
        "latest must not pick another host's snapshot"
    );

    fx.ok(&[
        "restore",
        "latest",
        "--any-host",
        "--subvol",
        &a.to_string_lossy(),
        "--target",
        &fx.base().join("r2").to_string_lossy(),
    ]);
    fx.ok(&[
        "restore",
        "latest",
        "--host",
        "other-machine",
        "--subvol",
        &a.to_string_lossy(),
        "--target",
        &fx.base().join("r3").to_string_lossy(),
    ]);
    assert_eq!(fs::read(fx.base().join("r3/f")).unwrap(), b"x");
}

#[test]
fn unmounted_nested_subvolume_is_warned_about() {
    e2e!();
    let fx = Fx::new("nestedwarn", &["@outer"]);
    let outer = &fx.mounts[0];
    // a nested subvolume that is not mounted anywhere (docker, machinectl, ...)
    sh(&format!("btrfs subvolume create '{}/vm'", outer.display()));
    // a read-only nested snapshot (snapper style) must NOT warn
    sh(&format!(
        "btrfs subvolume snapshot -r '{0}' '{0}/.snap'",
        outer.display()
    ));

    let out = fx.run(&["backup", "--dry-run"]);
    assert!(out.status.success(), "{}", text(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("subvolume /@outer/vm is nested under a selected"),
        "{}",
        text(&out)
    );
    assert!(
        !err.contains(".snap"),
        "read-only snapshots are not warned about:\n{err}"
    );
}

// ---------------------------------------------------------------------------
// ls / dump / restore --as-subvolume

#[test]
fn ls_and_dump_read_a_snapshot() {
    e2e!();
    let fx = Fx::new("lsdump", &["@a", "@b"]);
    let (a, b) = (&fx.mounts[0], &fx.mounts[1]);
    fs::create_dir_all(a.join("sub")).unwrap();
    fs::write(a.join("sub/hello.txt"), b"hello dump").unwrap();
    fs::write(b.join("other"), b"x").unwrap();
    fx.ok(&["backup"]);

    let all = fx.ok(&["ls", "latest"]);
    for want in [
        format!("{}/sub/hello.txt", a.display()),
        format!("{}/other", b.display()),
    ] {
        assert!(
            all.lines().any(|l| l.ends_with(&want)),
            "ls / lacks {want}:\n{all}"
        );
    }
    let sub = fx.ok(&["ls", "latest", &a.to_string_lossy()]);
    assert!(
        sub.contains(&format!("{}/sub/hello.txt", a.display())),
        "{sub}"
    );
    assert!(
        !sub.contains(&b.to_string_lossy().to_string()),
        "ls <path> is scoped:\n{sub}"
    );
    let line = sub
        .lines()
        .find(|l| l.ends_with("/sub"))
        .expect("sub dir listed");
    assert!(line.starts_with('d'), "{line}");

    let dumped = Command::new(env!("CARGO_BIN_EXE_rbtrfs"))
        .env("RBTRFS_CONFIG", &fx.cfg)
        .args(["dump", "latest", &format!("{}/sub/hello.txt", a.display())])
        .output()
        .unwrap();
    assert!(dumped.status.success(), "{}", text(&dumped));
    assert_eq!(dumped.stdout, b"hello dump");

    let dir = fx.run(&["dump", "latest", &a.to_string_lossy()]);
    assert!(!dir.status.success(), "dumping a directory is an error");

    // a reader that closes the pipe early ends the command quietly
    let piped = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{} ls latest | head -1",
            env!("CARGO_BIN_EXE_rbtrfs")
        ))
        .env("RBTRFS_CONFIG", &fx.cfg)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&piped.stderr);
    assert!(
        !err.contains("Broken pipe") && !err.contains("panicked"),
        "{}",
        text(&piped)
    );
    assert!(!piped.stdout.is_empty());
}

#[test]
fn restore_as_subvolume_creates_a_real_subvolume() {
    e2e!();
    let fx = Fx::new("assubvol", &["@a"]);
    let a = &fx.mounts[0];
    fs::create_dir_all(a.join("sub")).unwrap();
    fs::write(a.join("sub/f.txt"), b"payload").unwrap();
    fx.ok(&["backup"]);

    // the top level of the test filesystem is mounted at <base>/mnt (btrfs)
    let target = fx.base().join("mnt/restored-sv");
    fx.ok(&[
        "restore",
        "latest",
        "--as-subvolume",
        "--subvol",
        &a.to_string_lossy(),
        "--target",
        &target.to_string_lossy(),
    ]);
    sh(&format!("btrfs subvolume show '{}'", target.display())); // fails if not a subvolume
    assert_eq!(fs::read(target.join("sub/f.txt")).unwrap(), b"payload");

    // refuses to overwrite, and a non-btrfs target fails without leaving anything
    let again = fx.run(&[
        "restore",
        "latest",
        "--as-subvolume",
        "--subvol",
        &a.to_string_lossy(),
        "--target",
        &target.to_string_lossy(),
    ]);
    assert!(!again.status.success());
    let plain = std::env::temp_dir().join("rbtrfs-it-assubvol-not-btrfs");
    let _ = fs::remove_dir_all(&plain);
    let out = fx.run(&[
        "restore",
        "latest",
        "--as-subvolume",
        "--subvol",
        &a.to_string_lossy(),
        "--target",
        &plain.to_string_lossy(),
    ]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(!plain.exists(), "no leftover after a failed --as-subvolume");
}

// ---------------------------------------------------------------------------
// repository retention

#[test]
fn forget_applies_retention_and_prunes_without_breaking_the_repo() {
    e2e!();
    let fx = Fx::new("forget", &["@a"]);
    let a = &fx.mounts[0];
    for i in 0..3 {
        fs::write(a.join(format!("gen{i}.bin")), vec![i as u8 + 1; 100_000]).unwrap();
        fx.ok(&["backup"]);
        std::thread::sleep(Duration::from_millis(1100)); // run ids have 1s resolution
    }
    let count = |fx: &Fx, part: bool| -> usize {
        let v: serde_json::Value =
            serde_json::from_str(&fx.restic(&["snapshots", "--json"])).unwrap();
        v.as_array()
            .unwrap()
            .iter()
            .filter(|s| {
                s["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t == "rbtrfs:part")
                    == part
            })
            .count()
    };
    assert_eq!((count(&fx, false), count(&fx, true)), (3, 3));

    // no policy configured: refuse
    let out = fx.run(&["forget"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("retention"),
        "{}",
        text(&out)
    );

    fx.write_cfg(
        &fx.mounts.clone(),
        "keep_local = 1\n[profile.default.retention]\nkeep_last = 1\n",
    );
    // dry run changes nothing
    let dry = fx.ok(&["forget", "--dry-run"]);
    assert!(dry.contains("forget 2"), "{dry}");
    assert_eq!((count(&fx, false), count(&fx, true)), (3, 3));

    // default prune: rustic only *marks* unneeded packs (a rustic-specific index
    // field) and deletes them on a later prune; official restic must still accept it
    fx.ok(&["forget", "--prune"]);
    assert_eq!(
        (count(&fx, false), count(&fx, true)),
        (1, 1),
        "newest merged + its parts"
    );
    fx.restic_check();
    fx.restic(&["snapshots"]);
    let restored = fx.restore("latest", a, "restored-marked");
    assert_eq!(files(&restored).len(), 3);
    // and instant delete really frees them
    let refused = fx.run(&["forget", "--prune", "--instant-delete"]);
    assert!(
        !refused.status.success(),
        "--instant-delete must require --allow-unsafe"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--allow-unsafe"),
        "{}",
        text(&refused)
    );
    fx.ok(&["forget", "--prune", "--instant-delete", "--allow-unsafe"]);
    fx.restic_check();

    // the newest backup is intact, and the next run still chains onto the kept parts
    let restored = fx.restore("latest", a, "restored");
    assert_eq!(files(&restored).len(), 3);
    std::thread::sleep(Duration::from_millis(1100));
    fx.ok(&["backup"]);
    let v: serde_json::Value = serde_json::from_str(&fx.restic(&["snapshots", "--json"])).unwrap();
    let newest_part = v
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| {
            s["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "rbtrfs:part")
        })
        .max_by_key(|s| s["time"].as_str().unwrap().to_string())
        .unwrap();
    assert!(
        newest_part["parent"].is_string(),
        "incremental chain survived forget"
    );
    fx.restic_check();
}

#[test]
fn local_retention_by_age() {
    e2e!();
    let fx = Fx::new("keepdays", &["@a"]);
    let a = &fx.mounts[0];
    fx.write_cfg(&fx.mounts.clone(), "keep_local = 1\nkeep_local_days = 1\n");
    for _ in 0..3 {
        fx.ok(&["backup"]);
        std::thread::sleep(Duration::from_millis(1100));
    }
    // all three are younger than a day: nothing is collected despite keep_local = 1
    assert_eq!(fx.local_runs(a).len(), 3);
    fx.ok(&["gc"]);
    assert_eq!(fx.local_runs(a).len(), 3);
    // overriding the age to 0 days falls back to the count
    fx.ok(&["gc", "--keep-local-days", "0"]);
    assert_eq!(fx.local_runs(a).len(), 1);
}

// ---------------------------------------------------------------------------
// signals, crashes, concurrent writers

#[test]
fn termination_signals_wait_for_post_hooks() {
    e2e!();
    let fx = Fx::new("signals", &["@a"]);
    let a = &fx.mounts[0];
    let spawn = |fx: &Fx| {
        fx.cmd(&["backup"])
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    };

    // (1) SIGTERM during a slow pre-hook: no snapshot, but the post-hook still runs
    let marker1 = fx.base().join("post-ran-1");
    fx.write_cfg(
        &fx.mounts.clone(),
        &format!(
            "[profile.default.hooks]\npre = [\"sleep 3\"]\npost = [\"touch '{}'\"]\n",
            marker1.display()
        ),
    );
    let child = spawn(&fx);
    std::thread::sleep(Duration::from_millis(1200));
    kill("TERM", child.id());
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("interrupted"),
        "{}",
        text(&out)
    );
    assert!(
        marker1.exists(),
        "post-hook must run although we were signalled"
    );
    assert!(
        fx.local_runs(a).is_empty(),
        "no snapshot after an interrupt before the burst"
    );

    // (2) SIGTERM during a slow post-hook: every post-hook still runs
    let marker2 = fx.base().join("post-ran-2");
    fx.write_cfg(
        &fx.mounts.clone(),
        &format!(
            "[profile.default.hooks]\npost = [\"sleep 3\", \"touch '{}'\"]\n",
            marker2.display()
        ),
    );
    let child = spawn(&fx);
    wait_until(20, "the snapshot", || !fx.local_runs(a).is_empty());
    kill("TERM", child.id());
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(
        marker2.exists(),
        "later post-hooks must still run:\n{}",
        text(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("after snapshot"),
        "{}",
        text(&out)
    );
}

#[test]
fn sigkill_mid_run_leaks_no_mounts_and_gc_reclaims_the_snapshots() {
    e2e!();
    let fx = Fx::new("sigkill", &["@a"]);
    let a = &fx.mounts[0];
    // a slow post-hook keeps the run alive after the burst
    fx.write_cfg(
        &fx.mounts.clone(),
        "[profile.default.hooks]\npost = [\"sleep 3\"]\n",
    );
    let mounts_before = host_mounts();

    let mut child = fx
        .cmd(&["backup"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(20, "the snapshot", || !fx.local_runs(a).is_empty());
    child.kill().unwrap(); // SIGKILL
    child.wait().unwrap();

    assert_eq!(
        mounts_before,
        host_mounts(),
        "SIGKILL leaked a mount into the host namespace"
    );
    assert_eq!(
        fx.local_runs(a).len(),
        1,
        "the orphaned snapshot persists on disk"
    );
    // the orphaned hook keeps the (private) namespace alive for its remaining
    // seconds; let it finish so teardown can unmount cleanly
    std::thread::sleep(Duration::from_millis(3500));

    // the run lock died with the process, and gc reclaims the orphan
    fx.ok(&["gc", "--keep-local", "0"]);
    assert!(fx.local_runs(a).is_empty());
    assert_eq!(mounts_before, host_mounts());
}

/// A file whose content proves its own integrity: `<len>:<fnv64>\n<body>`.
fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}

fn write_valid(path: &Path, seed: u64) {
    let body: Vec<u8> = (0..300_000u64)
        .map(|i| (i.wrapping_mul(seed | 1) >> 3) as u8)
        .collect();
    let mut content = format!("{}:{:016x}\n", body.len(), fnv(&body)).into_bytes();
    content.extend_from_slice(&body);
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    fs::write(&tmp, content).unwrap();
    fs::rename(&tmp, path).unwrap(); // readers (and snapshots) see old or new, never half
}

fn is_valid(bytes: &[u8]) -> bool {
    let Some(nl) = bytes.iter().position(|b| *b == b'\n') else {
        return false;
    };
    let Ok(header) = std::str::from_utf8(&bytes[..nl]) else {
        return false;
    };
    let Some((len, sum)) = header.split_once(':') else {
        return false;
    };
    let body = &bytes[nl + 1..];
    len.parse::<usize>().ok() == Some(body.len())
        && u64::from_str_radix(sum, 16).ok() == Some(fnv(body))
}

#[test]
fn files_are_individually_intact_while_writers_churn_two_subvolumes() {
    e2e!();
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    let fx = Fx::new("churn", &["@a", "@b"]);
    let dirs = [fx.mounts[0].clone(), fx.mounts[1].clone()];
    for d in &dirs {
        for f in 0..8 {
            write_valid(&d.join(format!("f{f}")), 1);
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let rounds = Arc::new(AtomicU64::new(0));
    let writer = {
        let (stop, rounds, dirs) = (stop.clone(), rounds.clone(), dirs.clone());
        std::thread::spawn(move || {
            let mut i = 2;
            while !stop.load(Ordering::Relaxed) {
                // alternate between the two subvolumes as fast as possible
                for f in 0..8 {
                    for d in &dirs {
                        write_valid(&d.join(format!("f{f}")), i);
                    }
                }
                i += 1;
                rounds.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    for _ in 0..3 {
        fx.ok(&["backup"]);
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(
        rounds.load(Ordering::Relaxed) > 3,
        "the writer must actually have raced the backups"
    );

    // Per-file integrity is what btrfs snapshots guarantee. Which *generation* of
    // each file lands in each subvolume is NOT asserted: the two snapshots are
    // taken microseconds apart, not atomically. In-flight `.tmp` files may be
    // captured part-written and are ignored; final names must be whole.
    for (i, d) in dirs.iter().enumerate() {
        let restored = files(&fx.restore("latest", d, &format!("restored-{i}")));
        let finals: Vec<_> = restored
            .iter()
            .filter(|(k, _)| !k.starts_with('.'))
            .collect();
        assert_eq!(finals.len(), 8, "every file present in {}", d.display());
        for (name, bytes) in finals {
            assert!(
                is_valid(bytes),
                "{name} in {} is torn or corrupt",
                d.display()
            );
        }
    }
    fx.restic_check();
}

/// What an *external* rustic user does: forget all but the newest run, then prune
/// with default (two-phase, delayed-deletion) options, in a loop. It uses the
/// library directly, so it does NOT take rbtrfs' run lock.
fn external_rustic_forget_and_prune(
    repo: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<(u32, Vec<String>)> {
    use rustic_core::{ConfigOptions, Credentials, PruneOptions, Repository, RepositoryOptions};
    std::thread::spawn(move || {
        let backends = rustic_backend::BackendOptions::default()
            .repository(repo.to_string_lossy())
            .to_backends()
            .unwrap();
        let creds = Credentials::password("pw");
        let keep = rbtrfs::config::Retention {
            keep_last: Some(1),
            ..Default::default()
        }
        .to_keep_options()
        .unwrap();
        let _ = ConfigOptions::default();
        let (mut rounds, mut errors) = (0, Vec::new());
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let attempt = || -> Result<(), String> {
                let repo = Repository::new(&RepositoryOptions::default(), &backends)
                    .map_err(|e| e.to_string())?
                    .open(&creds)
                    .map_err(|e| e.to_string())?;
                let snaps = repo.get_all_snapshots().map_err(|e| e.to_string())?;
                let plan = rbtrfs::forget::plan(snaps, &keep).map_err(|e| format!("{e:#}"))?;
                let ids: Vec<_> = plan
                    .forget_merged
                    .into_iter()
                    .chain(plan.forget_parts)
                    .collect();
                if !ids.is_empty() {
                    repo.delete_snapshots(&ids).map_err(|e| e.to_string())?;
                }
                let opts = PruneOptions::default();
                let pp = repo.prune_plan(&opts).map_err(|e| e.to_string())?;
                repo.prune(&opts, pp).map_err(|e| e.to_string())
            };
            match attempt() {
                Ok(()) => rounds += 1,
                Err(e) => errors.push(e),
            }
        }
        (rounds, errors)
    })
}

#[test]
fn external_rustic_forget_and_prune_during_backups_does_not_corrupt() {
    e2e!();
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let fx = Fx::new("extprune", &["@a", "@b"]);
    let dirs = [fx.mounts[0].clone(), fx.mounts[1].clone()];
    let churn = |round: u64| {
        for d in &dirs {
            for f in 0..6 {
                write_valid(&d.join(format!("f{f}")), round * 7 + f);
            }
        }
    };
    churn(1);
    fx.ok(&["backup"]); // repository exists before the pruner starts

    let stop = Arc::new(AtomicBool::new(false));
    let pruner = external_rustic_forget_and_prune(fx.repo(), stop.clone());
    for round in 2..8 {
        churn(round);
        fx.ok(&["backup"]); // must keep succeeding while the pruner hammers the repo
        std::thread::sleep(Duration::from_millis(1100));
    }
    stop.store(true, Ordering::Relaxed);
    let (rounds, errors) = pruner.join().unwrap();
    eprintln!(
        "external pruner: {rounds} successful rounds, {} errors: {errors:#?}",
        errors.len()
    );
    assert!(rounds >= 3, "the pruner must really have raced the backups");

    // The repository is intact for official restic, and the newest backup restores.
    fx.restic_check();
    for (i, d) in dirs.iter().enumerate() {
        let restored = files(&fx.restore("latest", d, &format!("restored-{i}")));
        let finals: Vec<_> = restored
            .iter()
            .filter(|(k, _)| !k.starts_with('.'))
            .collect();
        assert_eq!(finals.len(), 6);
        assert!(
            finals.iter().all(|(_, b)| is_valid(b)),
            "restored data corrupt after concurrent prune"
        );
    }
}

#[test]
fn official_restic_backups_during_rustic_prune_stay_intact() {
    e2e!();
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let fx = Fx::new("resticprune", &["@a"]);
    fs::write(fx.mounts[0].join("seed"), b"x").unwrap();
    fx.ok(&["backup"]); // creates the repository

    let data = fx.base().join("restic-data");
    fs::create_dir_all(&data).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let pruner = external_rustic_forget_and_prune(fx.repo(), stop.clone());
    for round in 1..7 {
        for f in 0..6 {
            write_valid(&data.join(format!("f{f}")), round * 11 + f);
        }
        fx.restic(&["backup", &data.to_string_lossy()]);
    }
    stop.store(true, Ordering::Relaxed);
    let (rounds, errors) = pruner.join().unwrap();
    eprintln!(
        "rustic pruner: {rounds} rounds, {} errors: {errors:#?}",
        errors.len()
    );
    assert!(rounds >= 3);

    fx.restic_check();
    let out = fx.base().join("restic-restore");
    fx.restic(&[
        "restore",
        "latest",
        "--target",
        &out.to_string_lossy(),
        "--path",
        &data.to_string_lossy(),
    ]);
}

#[test]
fn repository_can_live_on_a_privately_mounted_filesystem() {
    e2e!();
    // A second filesystem that the host never mounts stands in for an NFS export:
    // rbtrfs must mount it itself, inside its namespace, before opening the repo.
    let fx = Fx::new("repomount", &["@a"]);
    let a = &fx.mounts[0];
    fs::write(a.join("f.txt"), b"on the remote repo").unwrap();

    let img = fx.base().join("repo.img");
    sh(&format!(
        "truncate -s 256M '{0}' && mkfs.btrfs -qf '{0}'",
        img.display()
    ));
    let dev = sh(&format!("losetup --find --show '{}'", img.display()))
        .trim()
        .to_string();
    let target = fx.base().join("repo-mnt");
    let cfg = |source: &str| {
        format!(
            "[profile.default]\nrepository = \"restic/box\"\npassword = \"pw\"\n\
             subvolumes = [\"{a}\"]\n\
             [profile.default.repository_mount]\ntype = \"btrfs\"\nsource = \"{source}\"\n\
             target = \"{t}\"\n",
            t = target.display(),
            a = a.display()
        )
    };
    let mounts_before = host_mounts();

    // a mount that fails stops the run before anything is snapshotted
    fs::write(&fx.cfg, cfg("/dev/does-not-exist")).unwrap();
    let bad = fx.run(&["backup"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("mounting"),
        "{}",
        text(&bad)
    );
    assert!(fx.local_runs(a).is_empty());

    fs::write(&fx.cfg, cfg(&dev)).unwrap();
    fx.ok(&["backup", "--dry-run"]);
    fx.ok(&["backup"]);
    assert!(fx.ok(&["snapshots"]).contains("merged"));
    assert!(fx.ok(&["ls", "latest"]).contains("f.txt"));
    let restored = fx.restore("latest", a, "restored");
    assert_eq!(
        fs::read(restored.join("f.txt")).unwrap(),
        b"on the remote repo"
    );
    fx.ok(&["backup"]);
    fx.write_cfg_keep_repo_mount(&cfg(&dev), "[profile.default.retention]\nkeep_last = 1\n");
    fx.ok(&["forget"]);

    // the mount was private: the host never saw it, and left nothing behind
    assert_eq!(
        mounts_before,
        host_mounts(),
        "repository mount leaked into the host namespace"
    );
    assert!(
        !target.join("restic").exists(),
        "repository must not exist on the host side"
    );

    // ...yet the repository really is on that device, and official restic reads it
    let peek = fx.base().join("peek");
    fs::create_dir_all(&peek).unwrap();
    sh(&format!("mount '{dev}' '{}'", peek.display()));
    let out = Command::new("restic")
        .env("RESTIC_PASSWORD", "pw")
        .arg("-r")
        .arg(peek.join("restic/box"))
        .args(["check", "--read-data"])
        .output()
        .unwrap();
    sh(&format!("umount '{}'", peek.display()));
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn backend_options_reach_the_storage_backend() {
    e2e!();
    // opendal's built-in `fs` service needs a `root` option and nothing else, so it
    // proves the options table is passed through without needing a network backend.
    let fx = Fx::new("backendopts", &["@a"]);
    let a = &fx.mounts[0];
    fs::write(a.join("f.txt"), b"through opendal").unwrap();
    let root = fx.base().join("opendal-root");
    fs::create_dir_all(&root).unwrap();
    let cfg = |options: &str| {
        format!(
            "[profile.default]\nrepository = \"opendal:fs\"\npassword = \"pw\"\n\
             subvolumes = [\"{}\"]\n{options}",
            a.display()
        )
    };

    // without the option the backend refuses to start
    fs::write(&fx.cfg, cfg("")).unwrap();
    let bad = fx.run(&["backup", "--dry-run"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("root is not specified"),
        "{}",
        text(&bad)
    );

    fs::write(
        &fx.cfg,
        cfg(&format!(
            "[profile.default.backend_options]\nroot = \"{}\"\n",
            root.display()
        )),
    )
    .unwrap();
    fx.ok(&["backup"]);
    assert!(fx.ok(&["snapshots"]).contains("merged"));
    let restored = fx.restore("latest", a, "restored");
    assert_eq!(
        fs::read(restored.join("f.txt")).unwrap(),
        b"through opendal"
    );

    // the data is in a plain restic repository under `root`
    let out = Command::new("restic")
        .env("RESTIC_PASSWORD", "pw")
        .arg("-r")
        .arg(&root)
        .args(["check", "--read-data", "--no-lock"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn subvolumes_all_and_exclude_subvolumes_shape_the_plan() {
    e2e!();
    // Dry run only: "all" on a real machine would snapshot its own subvolumes.
    let fx = Fx::new("selectall", &["@a", "@b"]);
    let top = fx.base().join("mnt"); // the top-level subvolume is mounted too
    let cfg = |extra: &str| {
        format!(
            "[profile.default]\nrepository = \"{}\"\npassword = \"pw\"\nsubvolumes = \"all\"\n{extra}",
            fx.repo().display()
        )
    };

    fs::write(&fx.cfg, cfg("")).unwrap();
    let plan = fx.ok(&["backup", "--dry-run"]);
    for mp in [&fx.mounts[0], &fx.mounts[1], &top] {
        assert!(
            plan.contains(&format!("recorded as {}", mp.display())),
            "all selects {}:\n{plan}",
            mp.display()
        );
    }

    fs::write(
        &fx.cfg,
        cfg(&format!("exclude_subvolumes = [\"{}\"]\n", top.display())),
    )
    .unwrap();
    let plan = fx.ok(&["backup", "--dry-run"]);
    assert!(
        plan.contains(&format!("recorded as {}", fx.mounts[0].display())),
        "{plan}"
    );
    assert!(
        !plan.contains(&format!("recorded as {}", top.display())),
        "top level excluded:\n{plan}"
    );
    assert!(
        plan.contains(&format!("skipping {} (exclude_subvolumes)", top.display())),
        "{plan}"
    );

    // a wrong keyword is a config error
    fs::write(&fx.cfg, cfg("").replace("\"all\"", "\"everything\"")).unwrap();
    let bad = fx.run(&["backup", "--dry-run"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("\"all\""),
        "{}",
        text(&bad)
    );
}

#[test]
fn filesystems_mounted_inside_a_subvolume_are_not_backed_up() {
    e2e!();
    let fx = Fx::new("otherfs", &["@a"]);
    let a = &fx.mounts[0];
    fs::write(a.join("keep.txt"), b"in the subvolume").unwrap();
    // a different filesystem mounted on a directory inside the selected subvolume
    fs::create_dir_all(a.join("other")).unwrap();
    sh(&format!(
        "mount -t tmpfs tmpfs '{}'",
        a.join("other").display()
    ));
    fs::write(a.join("other/secret.txt"), b"not on btrfs").unwrap();

    fx.ok(&["backup"]);

    let restored = fx.restore("latest", a, "restored");
    assert_eq!(
        fs::read(restored.join("keep.txt")).unwrap(),
        b"in the subvolume"
    );
    assert!(
        restored.join("other").is_dir(),
        "the mount point is just an empty directory"
    );
    assert!(
        !restored.join("other/secret.txt").exists(),
        "data of another filesystem leaked in"
    );
    assert!(!fx.ok(&["ls", "latest"]).contains("secret.txt"));
}

#[test]
fn extra_paths_are_backed_up_live_next_to_the_subvolumes() {
    e2e!();
    let fx = Fx::new("extrapaths", &["@a"]);
    let a = &fx.mounts[0];
    fs::write(a.join("keep.txt"), b"snapshotted").unwrap();

    // two non-btrfs directories: one mounted inside the subvolume, one elsewhere
    let inner = a.join("other");
    let outer = fx.base().join("ext");
    for d in [&inner, &outer] {
        fs::create_dir_all(d).unwrap();
        sh(&format!("mount -t tmpfs tmpfs '{}'", d.display()));
    }
    fs::write(inner.join("secret.txt"), b"inner live").unwrap();
    fs::write(inner.join("skip.log"), b"excluded").unwrap();
    fs::write(outer.join("data.txt"), b"outer live").unwrap();

    fx.write_cfg(
        &fx.mounts.clone(),
        &format!(
            "exclude = [\"*.log\"]\nextra_paths = [\"{}\", \"{}/\"]\n",
            inner.display(),
            outer.display()
        ),
    );
    let plan = fx.ok(&["backup", "--dry-run"]);
    assert!(
        plan.contains(&format!("{} (extra path", outer.display())),
        "{plan}"
    );

    fx.ok(&["backup"]);
    std::thread::sleep(Duration::from_millis(1100));
    fx.ok(&["backup"]);

    // the subvolume's backup includes the live contents of the tmpfs inside it
    let restored = fx.restore("latest", a, "restored-a");
    assert_eq!(fs::read(restored.join("keep.txt")).unwrap(), b"snapshotted");
    assert_eq!(
        fs::read(restored.join("other/secret.txt")).unwrap(),
        b"inner live"
    );
    assert!(
        !restored.join("other/skip.log").exists(),
        "exclude applies to extra paths"
    );
    // and the outside directory is restorable at its real path
    let restored_ext = fx.restore("latest", &outer, "restored-ext");
    assert_eq!(
        fs::read(restored_ext.join("data.txt")).unwrap(),
        b"outer live"
    );
    assert!(fx
        .ok(&["ls", "latest"])
        .contains(&format!("{}/data.txt", outer.display())));

    // extra paths chain to the previous run like subvolumes do, and the repo is valid
    let v: serde_json::Value = serde_json::from_str(&fx.restic(&["snapshots", "--json"])).unwrap();
    let label = format!("rbtrfs-part:{}", rbtrfs::select::key_for(&outer));
    let mut parts: Vec<&serde_json::Value> = v
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| {
            s["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "rbtrfs:part")
        })
        .filter(|s| s["paths"][0] == outer.to_str().unwrap())
        .collect();
    parts.sort_by_key(|s| s["time"].as_str().unwrap().to_string());
    assert_eq!(parts.len(), 2, "one part per run for {label}");
    assert_eq!(parts[1]["parent"], parts[0]["id"]);
    fx.restic_check();
}

#[test]
fn bad_extra_paths_stop_the_run_before_anything_is_snapshotted() {
    e2e!();
    let fx = Fx::new("extrabad", &["@a"]);
    let a = &fx.mounts[0];
    fs::create_dir_all(a.join("plain")).unwrap();
    fs::write(a.join("file.txt"), b"x").unwrap();

    let try_path = |path: &str, want: &str| {
        fx.write_cfg(&fx.mounts.clone(), &format!("extra_paths = [\"{path}\"]\n"));
        let out = fx.run(&["backup"]);
        assert!(!out.status.success(), "{path} should be rejected");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(want),
            "{path}: {}",
            text(&out)
        );
        assert!(fx.local_runs(a).is_empty(), "no snapshot for {path}");
    };
    // already covered by the snapshot of the selected subvolume
    try_path(
        &a.join("plain").to_string_lossy(),
        "inside the selected subvolume",
    );
    try_path(&a.to_string_lossy(), "already selected");
    // not usable at all
    try_path(&a.join("file.txt").to_string_lossy(), "not a directory");
    try_path("/does/not/exist", "not a directory");
    try_path("relative/dir", "not an absolute path");
}

fn dir_bytes(p: &Path) -> u64 {
    fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => dir_bytes(&e.path()),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}

#[test]
fn compression_level_is_applied_at_init_and_to_existing_repositories() {
    e2e!();
    let fx = Fx::new("compression", &["@a"]);
    let a = &fx.mounts[0];
    // 8 MB that compresses to almost nothing
    let squashy = |tag: &str| {
        format!("{tag} squashes well. ")
            .repeat(8_000_000 / 22)
            .into_bytes()
    };
    let input_len = squashy("one").len() as u64;
    fs::write(a.join("one.txt"), squashy("one")).unwrap();

    // level 0: stored uncompressed, so the data is about as big as the input
    fx.write_cfg(&fx.mounts.clone(), "compression = 0\n");
    fx.ok(&["backup"]);
    let uncompressed = dir_bytes(&fx.repo().join("data"));
    assert!(
        uncompressed > input_len * 9 / 10,
        "level 0 should not compress: {uncompressed} bytes stored for {input_len}"
    );

    // switch an existing repository to zstd level 3: only NEW data is compressed
    fx.write_cfg(&fx.mounts.clone(), "compression = 3\n");
    fs::write(a.join("two.txt"), squashy("two")).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let out = fx.ok(&["backup"]);
    assert!(out.contains("compression set to level 3"), "{out}");
    let grown = dir_bytes(&fx.repo().join("data")) - uncompressed;
    assert!(
        grown < 1_000_000,
        "new data should be compressed, repository grew by {grown} bytes"
    );

    // an unchanged setting is not re-applied, and everything restores and verifies
    std::thread::sleep(Duration::from_millis(1100));
    assert!(!fx.ok(&["backup"]).contains("compression set"));
    let restored = fx.restore("latest", a, "restored");
    assert_eq!(fs::read(restored.join("one.txt")).unwrap(), squashy("one"));
    assert_eq!(fs::read(restored.join("two.txt")).unwrap(), squashy("two"));
    fx.restic_check();

    // out-of-range levels are a config error
    fx.write_cfg(&fx.mounts.clone(), "compression = 99\n");
    let bad = fx.run(&["backup", "--dry-run"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("compression must be between"),
        "{}",
        text(&bad)
    );
}

#[test]
fn background_commands_run_with_low_priority_settings() {
    e2e!();
    let fx = Fx::new("priority", &["@a"]);
    fs::write(fx.mounts[0].join("f"), b"x").unwrap();
    let out = fx.base().join("prio");
    fs::create_dir_all(&out).unwrap();

    // Hooks inherit what rbtrfs sets on itself, so they can report it back.
    let run = |settings: &str, tag: &str| -> (String, String, String) {
        let (n, i, w) = (
            out.join(format!("{tag}.nice")),
            out.join(format!("{tag}.io")),
            out.join(format!("{tag}.weight")),
        );
        fx.write_cfg(
            &fx.mounts.clone(),
            &format!(
                "{settings}[profile.default.hooks]\npre = [\"nice > '{}'\", \"ionice -p $$ > '{}'\", \
                 \"cat /sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)/cpu.weight > '{}' 2>/dev/null || true\"]\n",
                n.display(),
                i.display(),
                w.display()
            ),
        );
        std::thread::sleep(Duration::from_millis(1100));
        fx.ok(&["backup"]);
        let read = |p: &Path| fs::read_to_string(p).unwrap_or_default().trim().to_string();
        (read(&n), read(&i), read(&w))
    };

    // defaults: nice 10, lowest best-effort I/O class
    let (nice, io, weight) = run("", "default");
    assert_eq!(nice, "10");
    assert_eq!(io, "best-effort: prio 7");
    if Path::new("/run/systemd/system").exists() {
        assert_eq!(
            weight, "20",
            "default cgroup CPUWeight when systemd is available"
        );
    }

    // tuned: everything can be changed or switched off
    let (nice, io, _) = run(
        "nice = 3\nio_priority = \"idle\"\ncpu_weight = 0\nio_weight = 0\n",
        "tuned",
    );
    assert_eq!(nice, "3");
    assert_eq!(io, "idle");

    // `nice = 0` and `io_priority = "normal"` leave what the caller had alone
    let inherited = sh("nice").trim().to_string();
    let (nice, io, _) = run(
        "nice = 0\nio_priority = \"normal\"\ncpu_weight = 0\nio_weight = 0\n",
        "off",
    );
    assert_eq!(nice, inherited);
    assert!(
        io.starts_with("none") || io.starts_with("best-effort"),
        "{io}"
    );
}

#[test]
fn marker_files_and_xattrs_exclude_directories() {
    e2e!();
    let fx = Fx::new("markers", &["@a"]);
    let a = &fx.mounts[0];
    for (d, files) in [
        ("keep", &["f.txt"][..]),
        ("cache", &["CACHEDIR.TAG", "blob"][..]),
        ("custom", &[".nobackup", "data"][..]),
        ("tagged", &["inside"][..]),
    ] {
        fs::create_dir_all(a.join(d)).unwrap();
        for f in files {
            fs::write(a.join(d).join(f), b"x").unwrap();
        }
    }
    sh(&format!(
        "setfattr -n user.nobackup -v 1 '{}'",
        a.join("tagged").display()
    ));

    let restore = |name: &str| -> Vec<String> {
        let r = fx.restore("latest", a, name);
        let mut dirs: Vec<String> = fs::read_dir(&r)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        dirs.sort();
        dirs
    };

    // default: only CACHEDIR.TAG directories are skipped, and the dry run says why
    fx.write_cfg(&fx.mounts.clone(), "");
    let plan = fx.ok(&["backup", "--dry-run"]);
    assert!(
        plan.contains(&format!("{}/cache  (contains CACHEDIR.TAG)", a.display())),
        "{plan}"
    );
    fx.ok(&["backup"]);
    assert_eq!(restore("r1"), ["custom", "keep", "tagged"]);

    // any marker file name and extended attribute can be added
    std::thread::sleep(Duration::from_millis(1100));
    fx.write_cfg(
        &fx.mounts.clone(),
        "exclude_if_present = [\"CACHEDIR.TAG\", \".nobackup\"]\nexclude_if_xattr = [\"user.nobackup\"]\n",
    );
    let plan = fx.ok(&["backup", "--dry-run"]);
    assert!(
        plan.contains("(contains .nobackup)") && plan.contains("(xattr user.nobackup)"),
        "{plan}"
    );
    fx.ok(&["backup"]);
    assert_eq!(restore("r2"), ["keep"]);

    // an empty list turns the default off
    std::thread::sleep(Duration::from_millis(1100));
    fx.write_cfg(&fx.mounts.clone(), "exclude_if_present = []\n");
    fx.ok(&["backup"]);
    assert_eq!(restore("r3"), ["cache", "custom", "keep", "tagged"]);
}

#[test]
fn another_rbtrfs_process_does_not_pull_the_repository_out_from_under_a_backup() {
    e2e!();
    // Every process mounts the repository share at the same private path. On exit one
    // used to remove that directory, which on Linux also detaches the mount in other
    // mount namespaces, killing a backup that was still using it.
    let fx = Fx::new("mountrace", &["@a"]);
    let a = &fx.mounts[0];
    fs::write(a.join("f.txt"), b"x").unwrap();
    let img = fx.base().join("repo.img");
    sh(&format!(
        "truncate -s 256M '{0}' && mkfs.btrfs -qf '{0}'",
        img.display()
    ));
    let dev = sh(&format!("losetup --find --show '{}'", img.display()))
        .trim()
        .to_string();
    let target = fx.base().join("repo-mnt");
    fs::write(
        &fx.cfg,
        format!(
            "[profile.default]\nrepository = \"restic/box\"\npassword = \"pw\"\nsubvolumes = [\"{a}\"]\n\
             [profile.default.hooks]\npre = [\"sleep 4\"]\n\
             [profile.default.repository_mount]\ntype = \"btrfs\"\nsource = \"{dev}\"\ntarget = \"{t}\"\n",
            a = a.display(),
            t = target.display()
        ),
    )
    .unwrap();

    // the backup opens the repository, then waits in the hook, then needs it again
    let backup = fx
        .cmd(&["backup"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(2000));
    // meanwhile other processes mount and unmount the same path, and one hits the lock
    let _ = fx.run(&["snapshots"]);
    let _ = fx.run(&["backup"]);
    let out = backup.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "the running backup lost its repository:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn init_creates_the_repository_and_auto_init_can_be_disabled() {
    e2e!();
    let fx = Fx::new("init", &["@a"]);
    fs::write(fx.mounts[0].join("f"), b"x").unwrap();

    // --no-init: a missing repository is an error and nothing is created or snapshotted.
    let out = fx.run(&["backup", "--no-init"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("rbtrfs init"), "should point at init: {err}");
    assert!(!fx.repo().join("config").exists());
    assert!(fx.local_runs(&fx.mounts[0]).is_empty());

    // The dry run reports the same problem.
    let out = fx.run(&["backup", "--dry-run", "--no-init"]);
    assert!(!out.status.success());

    fx.ok(&["init"]);
    assert!(fx.repo().join("config").exists());

    // A second init refuses to touch the existing repository.
    let config_before = fs::read(fx.repo().join("config")).unwrap();
    let out = fx.run(&["init"]);
    assert!(!out.status.success());
    assert_eq!(config_before, fs::read(fx.repo().join("config")).unwrap());

    fx.ok(&["backup", "--no-init"]);
}

#[test]
fn auto_init_false_in_the_config_disables_creation() {
    e2e!();
    let fx = Fx::new("autoinit", &["@a"]);
    let cfg = fs::read_to_string(&fx.cfg).unwrap();
    fs::write(
        &fx.cfg,
        cfg.replacen(
            "password = \"pw\"",
            "password = \"pw\"\nauto_init = false",
            1,
        ),
    )
    .unwrap();
    let out = fx.run(&["backup"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("rbtrfs init"));
    assert!(!fx.repo().join("config").exists());
    fx.ok(&["init"]);
    fx.ok(&["backup"]);
}

#[test]
fn extra_paths_stay_on_their_own_filesystem() {
    e2e!();
    let fx = Fx::new("extrafs", &["@a"]);
    fs::write(fx.mounts[0].join("f"), b"x").unwrap();

    let outer = fx.base().join("ext");
    let nested = outer.join("nested");
    fs::create_dir_all(&outer).unwrap();
    sh(&format!("mount -t tmpfs tmpfs '{}'", outer.display()));
    fs::create_dir_all(&nested).unwrap();
    sh(&format!("mount -t tmpfs tmpfs '{}'", nested.display()));
    fs::write(outer.join("here.txt"), b"on the extra path's filesystem").unwrap();
    fs::write(nested.join("there.txt"), b"on another filesystem").unwrap();

    fx.write_cfg(
        &fx.mounts.clone(),
        &format!("extra_paths = [\"{}\"]\n", outer.display()),
    );
    fx.ok(&["backup"]);

    let listing = fx.ok(&["ls", "latest"]);
    assert!(
        listing.contains(&format!("{}/here.txt", outer.display())),
        "{listing}"
    );
    assert!(
        !listing.contains("there.txt"),
        "a filesystem mounted inside an extra path must not be backed up:\n{listing}"
    );
}

#[test]
fn a_failed_run_removes_its_own_snapshots_but_keeps_the_earlier_ones() {
    e2e!();
    let fx = Fx::new("failcleanup", &["@a"]);
    let a = &fx.mounts[0];
    fs::write(a.join("f"), b"x").unwrap();
    fx.ok(&["backup"]);
    assert_eq!(fx.local_runs(a).len(), 1);

    // the snapshots are taken, then the post hook fails the run
    std::thread::sleep(Duration::from_millis(1100));
    fx.write_cfg(
        &fx.mounts.clone(),
        "[profile.default.hooks]\npost = [\"false\"]\n",
    );
    let out = fx.run(&["backup"]);
    assert!(!out.status.success());
    assert_eq!(
        fx.local_runs(a).len(),
        1,
        "only the earlier run's snapshot remains: {}",
        text(&out)
    );
}

#[test]
fn hooks_do_not_receive_the_terminals_ctrl_c() {
    e2e!();
    use std::os::unix::process::CommandExt;
    let fx = Fx::new("hooksig", &["@a"]);
    let marker = fx.base().join("hook-finished");
    fx.write_cfg(
        &fx.mounts.clone(),
        &format!(
            "[profile.default.hooks]\npre = [\"sleep 2; touch '{}'\"]\n",
            marker.display()
        ),
    );
    // Its own process group, like a foreground job: Ctrl-C signals the whole group.
    let child = fx
        .cmd(&["backup"])
        .process_group(0)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    sh(&format!("kill -INT -- -{}", child.id()));
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(
        marker.exists(),
        "the hook must run to the end, only rbtrfs is signalled:\n{}",
        text(&out)
    );
}

#[test]
fn a_repository_inside_a_selected_subvolume_is_not_backed_up_into_itself() {
    e2e!();
    let fx = Fx::new("selfbackup", &["@a"]);
    let a = &fx.mounts[0];
    fs::write(a.join("keep.txt"), b"data").unwrap();
    let repo = a.join("repo");
    fx.write_cfg_keep_repo_mount(
        &format!(
            "[profile.default]\nrepository = \"{}\"\npassword = \"pw\"\nsubvolumes = [\"{}\"]\n",
            repo.display(),
            a.display()
        ),
        "",
    );
    fx.ok(&["backup"]);
    std::thread::sleep(Duration::from_millis(1100));
    fx.ok(&["backup"]);
    let listing = fx.ok(&["ls", "latest"]);
    assert!(
        listing.contains(&format!("{}/keep.txt", a.display())),
        "{listing}"
    );
    assert!(
        !listing.contains(&format!("{}/repo/", a.display())),
        "the repository must not be in the backup:\n{listing}"
    );
}

#[test]
fn the_run_lock_is_private_to_root() {
    e2e!();
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new("lockperms", &["@a"]);
    fs::write(fx.mounts[0].join("f"), b"x").unwrap();
    fx.ok(&["backup"]);
    let mode = |p: &str| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("/run/rbtrfs"), 0o700);
    assert_eq!(mode("/run/rbtrfs/rbtrfs.lock"), 0o600);
}
