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

    use testcontainers::core::{
        wait::ExitWaitStrategy, ImageExt, Mount, WaitFor,
    };
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
        a.ancestors().find(|d| b.starts_with(d)).unwrap_or(Path::new("/")).to_path_buf()
    }

    /// Run test `name` of this test binary as root inside a privileged container.
    /// Returns `true` if the test was handled (passed, or skipped with a notice);
    /// panics if the test failed in the container.
    pub fn delegated(type_name: &str) -> bool {
        if is_root() {
            return false;
        }
        // "e2e_loopback::excludes_exclude::f" -> "excludes_exclude"
        let name = type_name.trim_end_matches("::f").rsplit("::").next().unwrap();

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
            .with_mount(Mount::bind_mount(root.to_string_lossy(), root.to_string_lossy()))
            // scratch images live in RAM: fast, and gone with the container
            .with_mount(Mount::tmpfs_mount("/tmp"))
            .with_env_var("RUST_BACKTRACE", "1")
            .with_startup_timeout(Duration::from_secs(900))
            .with_cmd([exe.to_string_lossy().into_owned(), "--exact".into(), name.into(), "--nocapture".into()])
            .start()
            .unwrap_or_else(|e| panic!("starting container for {name}: {e}"));

        let code = container.exit_code().unwrap();
        if code != Some(0) {
            let out = String::from_utf8_lossy(&container.stdout_to_vec().unwrap_or_default()).into_owned();
            let err = String::from_utf8_lossy(&container.stderr_to_vec().unwrap_or_default()).into_owned();
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
        assert!(container::is_root(), "e2e fixtures need root (e2e!() delegates to a container)");
        let serial = serial();
        let base = std::env::temp_dir().join(format!("rbtrfs-it-{tag}"));
        Self::teardown(&base);
        fs::create_dir_all(&base).unwrap();
        let img = base.join("fs.img");
        sh(&format!("truncate -s 512M {}", img.display()));
        sh(&format!("mkfs.btrfs -qf {}", img.display()));
        let dev = sh(&format!("losetup --find --show {}", img.display())).trim().to_string();
        let mnt = base.join("mnt");
        fs::create_dir_all(&mnt).unwrap();
        sh(&format!("mount {dev} {}", mnt.display()));
        Self { base, dev, _serial: serial }
    }

    /// Best-effort removal of everything under `base`, also from aborted runs.
    fn teardown(base: &Path) {
        // unmount deepest first (the top level plus every subvolume mount)
        try_sh(&format!(
            "for m in $(mount | awk '$3 ~ \"^{}\" {{print $3}}' | sort -r); do umount \"$m\"; done",
            base.display()
        ));
        try_sh(&format!(
            "losetup -j {}/fs.img | cut -d: -f1 | xargs -r losetup -d",
            base.display()
        ));
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
            sh(&format!("btrfs subvolume create {}/mnt/'{name}'", fs.base.display()));
            let mp = fs.base.join(format!("s{i}"));
            fs::create_dir_all(&mp).unwrap();
            sh(&format!("mount -o subvol='{name}' {} {}", fs.dev, mp.display()));
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
        assert!(out.status.success(), "rbtrfs {args:?} failed:\n{}", text(&out));
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
        assert!(out.status.success(), "restic {args:?} failed:\n{}", text(&out));
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
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Everything about a tree that a faithful backup must preserve.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    File { content: Vec<u8>, mode: u32, uid: u32, gid: u32, mtime: i64 },
    Dir { mode: u32, uid: u32, gid: u32 },
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
                Entry::Dir { mode: md.mode() & 0o7777, uid: md.uid(), gid: md.gid() }
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
    sh(&format!("chmod 640 '{0}/private.txt' && chown 1234:5678 '{0}/private.txt'", a.display()));
    sh(&format!("touch -d @1700000000 '{}/private.txt'", a.display()));
    symlink("sub/hello.txt", a.join("link")).unwrap();
    sh(&format!("setfattr -n user.rbtrfs -v marker '{}/sub/hello.txt'", a.display()));

    let before = (walk(a), walk(b), walk(c));
    let mounts_before = host_mounts();

    fx.ok(&["discover"]);
    let json = fx.ok(&["discover", "--json"]);
    serde_json::from_str::<serde_json::Value>(&json).expect("discover --json is valid JSON");

    fx.ok(&["backup"]);
    fx.ok(&["backup"]); // second run: parent detection + GC

    assert_eq!(mounts_before, host_mounts(), "host mount table changed (namespace leak)");
    // keep_local defaults to 1: exactly one local snapshot set per subvolume remains
    for mp in &fx.mounts {
        assert_eq!(fx.local_runs(mp).len(), 1, "local sets for {}", mp.display());
    }

    for (i, mp) in fx.mounts.iter().enumerate() {
        let restored = fx.restore("latest", mp, &format!("restore-{i}"));
        let want = [&before.0, &before.1, &before.2][i];
        assert_eq!(want, &walk(&restored), "subvolume {} did not round-trip", mp.display());
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

    assert_eq!(fs::read_to_string(a.join("f.txt")).unwrap().trim(), "CHANGED", "hook ran");
    let restored = fx.restore("latest", a, "restored");
    assert_eq!(fs::read(restored.join("f.txt")).unwrap(), b"ORIGINAL");
    assert!(!restored.join("new.txt").exists(), "post-snapshot file leaked into the backup");
}

#[test]
fn excludes_exclude() {
    e2e!();
    let fx = Fx::new("excludes", &["@a"]);
    let a = &fx.mounts[0];
    for d in [".cache", "u/.cache", "Downloads", "sub", "keepdir/node_modules"] {
        fs::create_dir_all(a.join(d)).unwrap();
    }
    for f in [
        "keep.txt", "skip.tmp", ".cache/x", "u/.cache/y", "u/keep.txt", "Downloads/z", "sub/keep2",
        "sub/deep.tmp", "keepdir/node_modules/m", "keepdir/k",
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

    let snaps: serde_json::Value = serde_json::from_str(&fx.restic(&["snapshots", "--json"])).unwrap();
    let mut parts: Vec<&serde_json::Value> = snaps
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["tags"].as_array().unwrap().iter().any(|t| t == "rbtrfs:part"))
        .collect();
    parts.sort_by_key(|s| s["time"].as_str().unwrap().to_string());
    assert_eq!(parts.len(), 2);
    assert!(parts[1]["parent"].is_string(), "second part has a parent: {}", parts[1]);
    assert_eq!(parts[1]["parent"], parts[0]["id"], "chained to the previous run's part");
    let added = parts[1]["summary"]["data_added"].as_u64().unwrap();
    assert_eq!(added, 0, "unchanged data was re-read/re-added");

    // merged snapshots form a chain too
    let mut merged: Vec<&serde_json::Value> = snaps
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| !s["tags"].as_array().unwrap().iter().any(|t| t == "rbtrfs:part"))
        .collect();
    merged.sort_by_key(|s| s["time"].as_str().unwrap().to_string());
    assert_eq!(merged.len(), 2);
    assert!(merged[0]["parent"].is_null(), "first merged snapshot has no parent");
    assert_eq!(merged[1]["parent"], merged[0]["id"], "merged snapshots are chained");
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
    assert!(!out.status.success(), "backup must fail when a pre-hook fails");
    assert!(marker.exists(), "post-hook must run after a failed pre-hook:\n{}", text(&out));
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
    sh(&format!("mount -o subvol=@inner {} {}", fx.fs.dev, inner.display()));
    fs::write(outer.join("o.txt"), b"outer").unwrap();
    fs::write(inner.join("i.txt"), b"inner").unwrap();

    fx.write_cfg(&[outer.clone(), inner.clone()], "");
    let out = fx.run(&["backup"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("nested under a selected"), "no nested warning when both selected");

    let restored = files(&fx.restore("latest", outer, "restored"));
    assert_eq!(restored.get("o.txt").map(|v| v.as_slice()), Some(&b"outer"[..]));
    assert_eq!(restored.get("inner/i.txt").map(|v| v.as_slice()), Some(&b"inner"[..]));

    // selecting only the outer one must warn that the inner one will be empty
    fx.write_cfg(std::slice::from_ref(outer), "");
    let out = fx.run(&["backup", "--dry-run"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("nested under a selected"), "{}", text(&out));
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
    assert_eq!(fs::read_to_string(restored.join("f.txt")).unwrap().trim(), "CHANGED");
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
        assert_eq!(runs.len(), 1, "in-subvolume GC left {runs:?} for {}", mp.display());
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

    let cfg = fs::read_to_string(&fx.cfg).unwrap().replace("password = \"pw\"", "password = \"nope\"");
    fs::write(&fx.cfg, cfg).unwrap();
    let out = fx.run(&["backup"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("initializing repository"), "must not try to init: {err}");
    assert_eq!(config_before, fs::read(fx.repo().join("config")).unwrap());
    assert_eq!(keys_before, list_dir(&fx.repo().join("keys")));
    assert!(fx.local_runs(&fx.mounts[0]).len() <= 1, "no extra snapshot taken before failing");
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
    let out = fx.run(&["restore", "latest", "--subvol", &a.to_string_lossy(), "--target", &target.to_string_lossy()]);
    assert!(!out.status.success(), "latest must not pick another host's snapshot");

    fx.ok(&["restore", "latest", "--any-host", "--subvol", &a.to_string_lossy(), "--target", &fx.base().join("r2").to_string_lossy()]);
    fx.ok(&["restore", "latest", "--host", "other-machine", "--subvol", &a.to_string_lossy(), "--target", &fx.base().join("r3").to_string_lossy()]);
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
    sh(&format!("btrfs subvolume snapshot -r '{0}' '{0}/.snap'", outer.display()));

    let out = fx.run(&["backup", "--dry-run"]);
    assert!(out.status.success(), "{}", text(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("subvolume /@outer/vm is nested under a selected"), "{}", text(&out));
    assert!(!err.contains(".snap"), "read-only snapshots are not warned about:\n{err}");
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
    for want in [format!("{}/sub/hello.txt", a.display()), format!("{}/other", b.display())] {
        assert!(all.lines().any(|l| l.ends_with(&want)), "ls / lacks {want}:\n{all}");
    }
    let sub = fx.ok(&["ls", "latest", &a.to_string_lossy()]);
    assert!(sub.contains(&format!("{}/sub/hello.txt", a.display())), "{sub}");
    assert!(!sub.contains(&b.to_string_lossy().to_string()), "ls <path> is scoped:\n{sub}");
    let line = sub.lines().find(|l| l.ends_with("/sub")).expect("sub dir listed");
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
    fx.ok(&["restore", "latest", "--as-subvolume", "--subvol", &a.to_string_lossy(), "--target", &target.to_string_lossy()]);
    sh(&format!("btrfs subvolume show '{}'", target.display())); // fails if not a subvolume
    assert_eq!(fs::read(target.join("sub/f.txt")).unwrap(), b"payload");

    // refuses to overwrite, and a non-btrfs target fails without leaving anything
    let again = fx.run(&["restore", "latest", "--as-subvolume", "--subvol", &a.to_string_lossy(), "--target", &target.to_string_lossy()]);
    assert!(!again.status.success());
    let plain = std::env::temp_dir().join("rbtrfs-it-assubvol-not-btrfs");
    let _ = fs::remove_dir_all(&plain);
    let out = fx.run(&["restore", "latest", "--as-subvolume", "--subvol", &a.to_string_lossy(), "--target", &plain.to_string_lossy()]);
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
        let v: serde_json::Value = serde_json::from_str(&fx.restic(&["snapshots", "--json"])).unwrap();
        v.as_array().unwrap().iter()
            .filter(|s| s["tags"].as_array().unwrap().iter().any(|t| t == "rbtrfs:part") == part)
            .count()
    };
    assert_eq!((count(&fx, false), count(&fx, true)), (3, 3));

    // no policy configured: refuse
    let out = fx.run(&["forget"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("retention"), "{}", text(&out));

    fx.write_cfg(&fx.mounts.clone(), "keep_local = 1\n[profile.default.retention]\nkeep_last = 1\n");
    // dry run changes nothing
    let dry = fx.ok(&["forget", "--dry-run"]);
    assert!(dry.contains("forget 2"), "{dry}");
    assert_eq!((count(&fx, false), count(&fx, true)), (3, 3));

    // default prune: rustic only *marks* unneeded packs (a rustic-specific index
    // field) and deletes them on a later prune; official restic must still accept it
    fx.ok(&["forget", "--prune"]);
    assert_eq!((count(&fx, false), count(&fx, true)), (1, 1), "newest merged + its parts");
    fx.restic_check();
    fx.restic(&["snapshots"]);
    let restored = fx.restore("latest", a, "restored-marked");
    assert_eq!(files(&restored).len(), 3);
    // and instant delete really frees them
    let refused = fx.run(&["forget", "--prune", "--instant-delete"]);
    assert!(!refused.status.success(), "--instant-delete must require --allow-unsafe");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--allow-unsafe"), "{}", text(&refused));
    fx.ok(&["forget", "--prune", "--instant-delete", "--allow-unsafe"]);
    fx.restic_check();

    // the newest backup is intact, and the next run still chains onto the kept parts
    let restored = fx.restore("latest", a, "restored");
    assert_eq!(files(&restored).len(), 3);
    std::thread::sleep(Duration::from_millis(1100));
    fx.ok(&["backup"]);
    let v: serde_json::Value = serde_json::from_str(&fx.restic(&["snapshots", "--json"])).unwrap();
    let newest_part = v.as_array().unwrap().iter()
        .filter(|s| s["tags"].as_array().unwrap().iter().any(|t| t == "rbtrfs:part"))
        .max_by_key(|s| s["time"].as_str().unwrap().to_string())
        .unwrap();
    assert!(newest_part["parent"].is_string(), "incremental chain survived forget");
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
    let spawn = |fx: &Fx| fx.cmd(&["backup"]).stderr(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();

    // (1) SIGTERM during a slow pre-hook: no snapshot, but the post-hook still runs
    let marker1 = fx.base().join("post-ran-1");
    fx.write_cfg(&fx.mounts.clone(), &format!(
        "[profile.default.hooks]\npre = [\"sleep 3\"]\npost = [\"touch '{}'\"]\n", marker1.display()));
    let child = spawn(&fx);
    std::thread::sleep(Duration::from_millis(1200));
    kill("TERM", child.id());
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("interrupted"), "{}", text(&out));
    assert!(marker1.exists(), "post-hook must run although we were signalled");
    assert!(fx.local_runs(a).is_empty(), "no snapshot after an interrupt before the burst");

    // (2) SIGTERM during a slow post-hook: every post-hook still runs
    let marker2 = fx.base().join("post-ran-2");
    fx.write_cfg(&fx.mounts.clone(), &format!(
        "[profile.default.hooks]\npost = [\"sleep 3\", \"touch '{}'\"]\n", marker2.display()));
    let child = spawn(&fx);
    wait_until(20, "the snapshot", || !fx.local_runs(a).is_empty());
    kill("TERM", child.id());
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(marker2.exists(), "later post-hooks must still run:\n{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("after snapshot"), "{}", text(&out));
}

#[test]
fn sigkill_mid_run_leaks_no_mounts_and_gc_reclaims_the_snapshots() {
    e2e!();
    let fx = Fx::new("sigkill", &["@a"]);
    let a = &fx.mounts[0];
    // a slow post-hook keeps the run alive after the burst
    fx.write_cfg(&fx.mounts.clone(), "[profile.default.hooks]\npost = [\"sleep 3\"]\n");
    let mounts_before = host_mounts();

    let mut child = fx.cmd(&["backup"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    wait_until(20, "the snapshot", || !fx.local_runs(a).is_empty());
    child.kill().unwrap(); // SIGKILL
    child.wait().unwrap();

    assert_eq!(mounts_before, host_mounts(), "SIGKILL leaked a mount into the host namespace");
    assert_eq!(fx.local_runs(a).len(), 1, "the orphaned snapshot persists on disk");
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
    data.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

fn write_valid(path: &Path, seed: u64) {
    let body: Vec<u8> = (0..300_000u64).map(|i| (i.wrapping_mul(seed | 1) >> 3) as u8).collect();
    let mut content = format!("{}:{:016x}\n", body.len(), fnv(&body)).into_bytes();
    content.extend_from_slice(&body);
    let tmp = path.with_file_name(format!(".{}.tmp", path.file_name().unwrap().to_string_lossy()));
    fs::write(&tmp, content).unwrap();
    fs::rename(&tmp, path).unwrap(); // readers (and snapshots) see old or new, never half
}

fn is_valid(bytes: &[u8]) -> bool {
    let Some(nl) = bytes.iter().position(|b| *b == b'\n') else { return false };
    let Ok(header) = std::str::from_utf8(&bytes[..nl]) else { return false };
    let Some((len, sum)) = header.split_once(':') else { return false };
    let body = &bytes[nl + 1..];
    len.parse::<usize>().ok() == Some(body.len()) && u64::from_str_radix(sum, 16).ok() == Some(fnv(body))
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
    assert!(rounds.load(Ordering::Relaxed) > 3, "the writer must actually have raced the backups");

    // Per-file integrity is what btrfs snapshots guarantee. Which *generation* of
    // each file lands in each subvolume is NOT asserted: the two snapshots are
    // taken microseconds apart, not atomically. In-flight `.tmp` files may be
    // captured part-written and are ignored; final names must be whole.
    for (i, d) in dirs.iter().enumerate() {
        let restored = files(&fx.restore("latest", d, &format!("restored-{i}")));
        let finals: Vec<_> = restored.iter().filter(|(k, _)| !k.starts_with('.')).collect();
        assert_eq!(finals.len(), 8, "every file present in {}", d.display());
        for (name, bytes) in finals {
            assert!(is_valid(bytes), "{name} in {} is torn or corrupt", d.display());
        }
    }
    fx.restic_check();
}

/// What an *external* rustic user does: forget all but the newest run, then prune
/// with default (two-phase, delayed-deletion) options, in a loop. It uses the
/// library directly, so it does NOT take rbtrfs' run lock.
fn external_rustic_forget_and_prune(repo: PathBuf, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<(u32, Vec<String>)> {
    use rustic_core::{ConfigOptions, Credentials, PruneOptions, Repository, RepositoryOptions};
    std::thread::spawn(move || {
        let backends = rustic_backend::BackendOptions::default()
            .repository(repo.to_string_lossy())
            .to_backends()
            .unwrap();
        let creds = Credentials::password("pw");
        let keep = rbtrfs::config::Retention { keep_last: Some(1), ..Default::default() }
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
                let ids: Vec<_> = plan.forget_merged.into_iter().chain(plan.forget_parts).collect();
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
    eprintln!("external pruner: {rounds} successful rounds, {} errors: {errors:#?}", errors.len());
    assert!(rounds >= 3, "the pruner must really have raced the backups");

    // The repository is intact for official restic, and the newest backup restores.
    fx.restic_check();
    for (i, d) in dirs.iter().enumerate() {
        let restored = files(&fx.restore("latest", d, &format!("restored-{i}")));
        let finals: Vec<_> = restored.iter().filter(|(k, _)| !k.starts_with('.')).collect();
        assert_eq!(finals.len(), 6);
        assert!(finals.iter().all(|(_, b)| is_valid(b)), "restored data corrupt after concurrent prune");
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
    eprintln!("rustic pruner: {rounds} rounds, {} errors: {errors:#?}", errors.len());
    assert!(rounds >= 3);

    fx.restic_check();
    let out = fx.base().join("restic-restore");
    fx.restic(&["restore", "latest", "--target", &out.to_string_lossy(), "--path", &data.to_string_lossy()]);
}
