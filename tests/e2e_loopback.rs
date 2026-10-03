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
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;


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
