//! Configuration: a TOML file with one or more named profiles.
//!
//! ```toml
//! [profile.default]
//! repository   = "/mnt/backup/restic"
//! password_command = "pass show backup/restic"
//! subvolumes   = ["/home", "/srv"]
//! exclude      = ["**/.cache", "**/node_modules"]
//! tags         = ["rbtrfs"]
//! keep_local   = 1
//!
//! [profile.default.hooks]
//! pre  = ["/usr/local/bin/before-snapshot"]
//! post = ["/usr/local/bin/after-snapshot"]
//! on_failure = "abort"   # or "warn"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub profile: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The profile's name in the config file (filled in by [`Config::load`]).
    #[serde(skip)]
    pub name: String,

    /// restic repository location (`/path`, `rest:`, `s3:…`, `sftp:…`, …).
    pub repository: String,

    /// Process niceness for backup, forget and gc: `0..=19`, higher yields more to
    /// other programs. `0` leaves it alone.
    #[serde(default = "default_nice")]
    pub nice: i32,

    /// Disk I/O priority for those commands.
    #[serde(default)]
    pub io_priority: IoPriority,

    /// Relative CPU share (systemd `CPUWeight`, `1..=10000`, where normal programs
    /// have 100) for backup, forget and gc. `0` turns it off.
    #[serde(default = "default_weight")]
    pub cpu_weight: u32,

    /// Relative disk share (systemd `IOWeight`, `1..=10000`, normal is 100).
    /// `0` turns it off.
    #[serde(default = "default_weight")]
    pub io_weight: u32,

    /// zstd compression level for new data: `1..=22`, `-7..=-1` for the fast levels,
    /// `0` for none. Unset means zstd's default level. Repository-wide.
    #[serde(default)]
    pub compression: Option<i32>,

    /// A separate "hot" repository (rustic hot/cold setups). Optional.
    #[serde(default)]
    pub repository_hot: Option<String>,

    /// Options passed to the storage backend (both hot and cold). The keys are
    /// rustic's, for example `rclone-command`, or the settings of an `opendal:`
    /// service such as `bucket` and `access_key_id`.
    #[serde(default)]
    pub backend_options: BTreeMap<String, String>,
    /// Options for the hot repository only.
    #[serde(default)]
    pub backend_options_hot: BTreeMap<String, String>,
    /// Options for the cold repository only.
    #[serde(default)]
    pub backend_options_cold: BTreeMap<String, String>,

    /// Mount a filesystem (NFS, CIFS, …) privately for the duration of the run, to
    /// hold a local-path `repository`. See [`RepositoryMount`].
    #[serde(default)]
    pub repository_mount: Option<RepositoryMount>,

    /// Exactly one password source must be given.
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub password_file: Option<PathBuf>,
    #[serde(default)]
    pub password_command: Option<String>,

    /// Mount points to capture: a list of exact paths or globs, or the string
    /// `"all"` for every mounted btrfs subvolume.
    pub subvolumes: Subvolumes,

    /// Mount points to leave out of `subvolumes` (exact paths or globs).
    #[serde(default)]
    pub exclude_subvolumes: Vec<String>,

    /// Directories on any filesystem to back up live (not snapshotted) next to the
    /// subvolumes, for example `/boot`.
    #[serde(default)]
    pub extra_paths: Vec<PathBuf>,

    /// Exclude patterns passed through to the backup engine.
    #[serde(default)]
    pub exclude: Vec<String>,

    /// Skip any directory that contains a file with one of these names. The default,
    /// `CACHEDIR.TAG`, is the standard marker that cargo, fontconfig and many other
    /// tools put in cache directories. Set to `[]` to back up everything.
    #[serde(default = "default_exclude_if_present")]
    pub exclude_if_present: Vec<String>,

    /// Skip files and directories that have one of these extended attributes set.
    #[serde(default)]
    pub exclude_if_xattr: Vec<String>,

    /// Create the repository on the first backup if it does not exist. Set to
    /// `false` to make `backup` fail instead; create it with `rbtrfs init`.
    #[serde(default = "default_true")]
    pub auto_init: bool,

    /// Print the directories that were left out because they hold a marker file or
    /// extended attribute (`exclude_if_present`, `exclude_if_xattr`). Costs one extra
    /// walk over the files. `backup --report-excluded` does it for a single run.
    #[serde(default)]
    pub report_excluded: bool,

    /// Tags to set on the merged snapshot.
    #[serde(default = "default_tags")]
    pub tags: Vec<String>,

    /// Where to stage the read-only snapshots.
    #[serde(default)]
    pub staging: Staging,

    /// Name of the staging directory/subvolume.
    #[serde(default = "default_staging_name")]
    pub staging_name: String,

    /// How many past sets of local part-snapshots to keep for incremental
    /// parents and fast local rollback.
    #[serde(default = "default_keep_local")]
    pub keep_local: usize,

    /// Also keep every local snapshot set younger than this many days, even
    /// beyond `keep_local`.
    #[serde(default)]
    pub keep_local_days: Option<u64>,

    /// Repository-side retention for `rbtrfs forget`. Required by that command.
    #[serde(default)]
    pub retention: Option<Retention>,

    #[serde(default)]
    pub hooks: Hooks,
}

/// Which mounted subvolumes to back up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subvolumes {
    /// Every mounted btrfs subvolume.
    All,
    /// Exact mount points or globs.
    List(Vec<String>),
}

impl<'de> Deserialize<'de> for Subvolumes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            List(Vec<String>),
        }
        match Raw::deserialize(d)? {
            Raw::Text(t) if t == "all" => Ok(Self::All),
            Raw::Text(t) => Err(serde::de::Error::custom(format!(
                "`subvolumes` must be a list of mount points or the string \"all\", not {t:?}"
            ))),
            Raw::List(v) => Ok(Self::List(v)),
        }
    }
}

/// A filesystem to mount, inside rbtrfs' private mount namespace, before the
/// repository is opened.
///
/// ```toml
/// [profile.default.repository_mount]
/// type    = "nfs"
/// source  = "nas.local:/export/backups"
/// options = "vers=4.2"
/// target  = "/run/rbtrfs/repo"                  # the default
///
/// [profile.default]
/// repository = "/run/rbtrfs/repo/restic/mybox"  # a path below `target`
/// ```
///
/// Runs `mount -t <type> [-o <options>] <source> <target>`, so anything `mount(8)`
/// and its helpers (`mount.nfs`, `mount.cifs`, …) can mount works. The mount is
/// private to the process: it never appears in the host's mount table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryMount {
    #[serde(rename = "type")]
    pub fstype: String,
    pub source: String,
    #[serde(default)]
    pub options: Option<String>,
    #[serde(default = "default_repo_mount_target")]
    pub target: PathBuf,
}

fn default_repo_mount_target() -> PathBuf {
    PathBuf::from("/run/rbtrfs/repo")
}

/// Which merged snapshots `rbtrfs forget` keeps (restic/rustic semantics).
///
/// ```toml
/// [profile.default.retention]
/// keep_last = 3
/// keep_daily = 7
/// keep_weekly = 4
/// keep_monthly = 12
/// keep_within = "14d"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub keep_last: Option<u32>,
    pub keep_hourly: Option<u32>,
    pub keep_daily: Option<u32>,
    pub keep_weekly: Option<u32>,
    pub keep_monthly: Option<u32>,
    pub keep_yearly: Option<u32>,
    /// Keep everything newer than this, relative to the newest snapshot (`"30d"`).
    pub keep_within: Option<String>,
}

impl Retention {
    pub fn is_empty(&self) -> bool {
        self.keep_last.is_none()
            && self.keep_hourly.is_none()
            && self.keep_daily.is_none()
            && self.keep_weekly.is_none()
            && self.keep_monthly.is_none()
            && self.keep_yearly.is_none()
            && self.keep_within.is_none()
    }

    /// Convert to rustic_core's options (which parse `keep_within` as a duration).
    pub fn to_keep_options(&self) -> Result<rustic_core::KeepOptions> {
        let mut v = serde_json::Map::new();
        for (k, n) in [
            ("keep-last", self.keep_last),
            ("keep-hourly", self.keep_hourly),
            ("keep-daily", self.keep_daily),
            ("keep-weekly", self.keep_weekly),
            ("keep-monthly", self.keep_monthly),
            ("keep-yearly", self.keep_yearly),
        ] {
            if let Some(n) = n {
                v.insert(k.into(), n.into());
            }
        }
        if let Some(w) = &self.keep_within {
            v.insert("keep-within".into(), w.clone().into());
        }
        serde_json::from_value(serde_json::Value::Object(v))
            .context("invalid retention (is keep_within a duration like \"30d\"?)")
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Staging {
    /// Snapshots under `<staging_name>/` in the top-level (`subvolid=5`) subvolume.
    #[default]
    TopLevel,
    /// Snapshots under `<mountpoint>/<staging_name>/` inside each source subvolume.
    InSubvolume,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    #[serde(default)]
    pub pre: Vec<String>,
    #[serde(default)]
    pub post: Vec<String>,
    #[serde(default)]
    pub on_failure: HookFailure,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HookFailure {
    #[default]
    Abort,
    Warn,
}

fn default_exclude_if_present() -> Vec<String> {
    vec!["CACHEDIR.TAG".to_string()]
}
fn default_nice() -> i32 {
    10
}
fn default_weight() -> u32 {
    20
}

/// Disk I/O priority of a backup run.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum IoPriority {
    /// Lowest best-effort priority.
    #[default]
    Low,
    /// Only when the disk is otherwise idle. Can starve on a busy disk.
    Idle,
    /// Leave the priority alone.
    Normal,
}

fn default_true() -> bool {
    true
}
fn default_tags() -> Vec<String> {
    vec!["rbtrfs".to_string()]
}
fn default_staging_name() -> String {
    ".rbtrfs-snapshots".to_string()
}
fn default_keep_local() -> usize {
    1
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        for (name, p) in cfg.profile.iter_mut() {
            p.name = name.clone();
            p.resolve_repository_path()
                .with_context(|| format!("profile [{name}]"))?;
        }
        for (name, p) in &cfg.profile {
            p.validate().with_context(|| format!("profile [{name}]"))?;
        }
        if cfg.profile.values().any(|p| p.password.is_some()) {
            warn_if_accessible(path, "contains an inline `password`");
        } else if cfg.profile.values().any(Profile::has_secret_backend_option) {
            warn_if_accessible(path, "contains backend credentials");
        }
        for p in cfg.profile.values() {
            if let Some(f) = &p.password_file {
                warn_if_accessible(f, "is a password_file");
            }
        }
        Ok(cfg)
    }

    pub fn profile(&self, name: &str) -> Result<&Profile> {
        self.profile
            .get(name)
            .ok_or_else(|| anyhow!("no profile named [{name}] in config"))
    }

    /// Default config path: `$RBTRFS_CONFIG`, else `/etc/rbtrfs/config.toml`.
    pub fn default_path() -> PathBuf {
        std::env::var_os("RBTRFS_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/rbtrfs/config.toml"))
    }
}

/// Warn (never fail) if a secret-bearing file is readable by group or others.
fn warn_if_accessible(path: &Path, why: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "rbtrfs: warning: {} {why} but is accessible by group/others (mode {mode:03o}); \
                 consider `chmod 600`",
                path.display()
            );
        }
    }
}

impl Profile {
    /// With `repository_mount`, a relative `repository` (`"tempest"`, or `"."` for the
    /// mount itself) is taken relative to the mount target. A string that looks like
    /// a backend URL (`rest:...`) is left alone and rejected later by `validate`.
    pub fn resolve_repository_path(&mut self) -> Result<()> {
        let Some(m) = &self.repository_mount else {
            return Ok(());
        };
        let repo = Path::new(&self.repository);
        let looks_like_url = self
            .repository
            .split('/')
            .next()
            .is_some_and(|first| first.contains(':'));
        if repo.is_absolute() || looks_like_url {
            return Ok(());
        }
        if repo
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            bail!("a relative `repository` must not contain `..` (it is relative to the mount target)");
        }
        let mut full = m.target.clone();
        for c in repo.components() {
            if let std::path::Component::Normal(part) = c {
                full.push(part); // drops `.` components
            }
        }
        self.repository = full.to_string_lossy().into_owned();
        Ok(())
    }

    /// Does any backend option look like a credential (key, secret, token, ...)?
    fn has_secret_backend_option(&self) -> bool {
        const HINTS: [&str; 5] = ["key", "secret", "token", "pass", "credential"];
        self.backend_options
            .keys()
            .chain(self.backend_options_hot.keys())
            .chain(self.backend_options_cold.keys())
            .any(|k| {
                let k = k.to_lowercase();
                HINTS.iter().any(|h| k.contains(h))
            })
    }

    /// The options to hand to rustic's backend factory.
    pub fn backend(&self) -> rustic_backend::BackendOptions {
        let mut b = rustic_backend::BackendOptions::default()
            .repository(&self.repository)
            .options(self.backend_options.clone())
            .options_hot(self.backend_options_hot.clone())
            .options_cold(self.backend_options_cold.clone());
        if let Some(hot) = &self.repository_hot {
            b = b.repo_hot(hot);
        }
        b
    }

    fn validate(&self) -> Result<()> {
        let n = self.password.is_some() as u8
            + self.password_file.is_some() as u8
            + self.password_command.is_some() as u8;
        if n != 1 {
            bail!(
                "exactly one of password / password_file / password_command must be set (got {n})"
            );
        }
        if matches!(&self.subvolumes, Subvolumes::List(v) if v.is_empty()) {
            bail!("`subvolumes` must not be empty");
        }
        for m in &self.exclude_if_present {
            if m.is_empty() || m.contains('/') {
                bail!("exclude_if_present entries are file names without a slash, got {m:?}");
            }
        }
        if self.exclude_if_xattr.iter().any(|x| x.is_empty()) {
            bail!("exclude_if_xattr entries must not be empty");
        }
        let mut parts = Path::new(&self.staging_name).components();
        if !matches!(
            (parts.next(), parts.next()),
            (Some(std::path::Component::Normal(_)), None)
        ) || self.staging_name.contains('/')
        {
            bail!(
                "staging_name must be a single directory name, got {:?}",
                self.staging_name
            );
        }
        if !(0..=19).contains(&self.nice) {
            bail!("nice must be between 0 and 19, got {}", self.nice);
        }
        for (name, w) in [
            ("cpu_weight", self.cpu_weight),
            ("io_weight", self.io_weight),
        ] {
            if w > 10_000 {
                bail!("{name} must be between 1 and 10000 (0 turns it off), got {w}");
            }
        }
        if let Some(c) = self.compression {
            if !(-7..=22).contains(&c) {
                bail!("compression must be between -7 and 22 (0 turns it off), got {c}");
            }
        }
        if !self.backend_options_hot.is_empty() && self.repository_hot.is_none() {
            bail!("backend_options_hot needs `repository_hot` to be set");
        }
        if let Some(m) = &self.repository_mount {
            if m.fstype.trim().is_empty() || m.source.trim().is_empty() {
                bail!("repository_mount needs a non-empty `type` and `source`");
            }
            if !m.target.is_absolute() {
                bail!("repository_mount.target must be an absolute path");
            }
            if !Path::new(&self.repository).starts_with(&m.target) {
                bail!(
                    "with repository_mount, `repository` must be a path below its target {} (got {:?})",
                    m.target.display(),
                    self.repository
                );
            }
        }
        if let Some(r) = &self.retention {
            r.to_keep_options().context("[retention]")?;
        }
        Ok(())
    }

    /// Resolve the repository password from whichever source is configured.
    pub fn resolve_password(&self) -> Result<String> {
        if let Some(p) = &self.password {
            return Ok(p.clone());
        }
        if let Some(f) = &self.password_file {
            let s = std::fs::read_to_string(f)
                .with_context(|| format!("reading password_file {}", f.display()))?;
            return Ok(s.trim_end_matches(['\n', '\r']).to_string());
        }
        if let Some(cmd) = &self.password_command {
            let out = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .with_context(|| format!("running password_command: {cmd}"))?;
            if !out.status.success() {
                bail!("password_command failed ({}): {cmd}", out.status);
            }
            let s = String::from_utf8(out.stdout).context("password_command output not utf-8")?;
            return Ok(s.trim_end_matches(['\n', '\r']).to_string());
        }
        unreachable!("validated")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal() {
        let cfg: Config = toml::from_str(
            r#"
            [profile.default]
            repository = "/repo"
            password = "hunter2"
            subvolumes = ["/home"]
            "#,
        )
        .unwrap();
        let p = cfg.profile("default").unwrap();
        p.validate().unwrap();
        assert_eq!(p.tags, vec!["rbtrfs"]);
        assert_eq!(p.keep_local, 1);
        assert_eq!(p.staging, Staging::TopLevel);
        assert_eq!(p.resolve_password().unwrap(), "hunter2");
    }

    fn with_mount(repo: &str, extra: &str) -> Result<()> {
        let cfg: Config = toml::from_str(&format!(
            r#"
            [profile.default]
            repository = "{repo}"
            password = "x"
            subvolumes = ["/home"]
            [profile.default.repository_mount]
            type = "nfs"
            source = "nas:/export"
            {extra}
            "#
        ))
        .unwrap();
        cfg.profile("default").unwrap().validate()
    }

    #[test]
    fn backend_options_reach_rustic_and_flag_credentials() {
        let cfg: Config = toml::from_str(
            r#"
            [profile.default]
            repository = "opendal:s3"
            repository_hot = "opendal:fs"
            password = "x"
            subvolumes = ["/home"]
            [profile.default.backend_options]
            bucket = "b"
            access_key_id = "AKIA"
            [profile.default.backend_options_cold]
            retry = "3"
            [profile.default.backend_options_hot]
            root = "/hot"
            "#,
        )
        .unwrap();
        let p = cfg.profile("default").unwrap();
        p.validate().unwrap();
        let b = p.backend();
        assert_eq!(b.repository.as_deref(), Some("opendal:s3"));
        assert_eq!(b.repo_hot.as_deref(), Some("opendal:fs"));
        assert_eq!(b.options["bucket"], "b");
        assert_eq!(b.options_cold["retry"], "3");
        assert_eq!(b.options_hot["root"], "/hot");
        assert!(p.has_secret_backend_option());
    }

    #[test]
    fn hot_options_need_a_hot_repository_and_plain_options_are_not_secret() {
        let cfg: Config = toml::from_str(
            r#"
            [profile.default]
            repository = "/r"
            password = "x"
            subvolumes = ["/home"]
            [profile.default.backend_options]
            retry = "3"
            [profile.default.backend_options_hot]
            root = "/hot"
            "#,
        )
        .unwrap();
        let p = cfg.profile("default").unwrap();
        assert!(p.validate().is_err());
        assert!(!p.has_secret_backend_option());
    }

    #[test]
    fn relative_repository_is_taken_below_the_mount_target() {
        let load = |repo: &str, target: &str| {
            let mut cfg: Config = toml::from_str(&format!(
                "[profile.default]\nrepository = \"{repo}\"\npassword = \"x\"\nsubvolumes = [\"/home\"]\n\
                 [profile.default.repository_mount]\ntype = \"nfs\"\nsource = \"nas:/e\"\n{target}"
            ))
            .unwrap();
            let p = cfg.profile.get_mut("default").unwrap();
            p.resolve_repository_path()?;
            p.validate()?;
            Ok::<_, anyhow::Error>(p.repository.clone())
        };
        assert_eq!(load("tempest", "").unwrap(), "/run/rbtrfs/repo/tempest");
        assert_eq!(load("a/b", "target = \"/mnt/x\"").unwrap(), "/mnt/x/a/b");
        assert_eq!(load(".", "").unwrap(), "/run/rbtrfs/repo");
        // absolute paths still have to be below the target, and `..` cannot escape it
        assert_eq!(
            load("/run/rbtrfs/repo/x", "").unwrap(),
            "/run/rbtrfs/repo/x"
        );
        assert!(load("/elsewhere", "").is_err());
        assert!(load("../escape", "").is_err());
        assert!(load("rest:https://h/", "").is_err());
    }

    #[test]
    fn load_resolves_a_relative_repository() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.toml");
        std::fs::write(
            &file,
            "[profile.default]\nrepository = \"restic/box\"\npassword_command = \"true\"\n\
             subvolumes = [\"/home\"]\n[profile.default.repository_mount]\ntype = \"nfs\"\n\
             source = \"nas:/e\"\n",
        )
        .unwrap();
        let cfg = Config::load(&file).unwrap();
        assert_eq!(
            cfg.profile("default").unwrap().repository,
            "/run/rbtrfs/repo/restic/box"
        );
    }

    #[test]
    fn repository_mount_validation() {
        with_mount("/run/rbtrfs/repo/restic/box", "").unwrap();
        with_mount(
            "/srv/x/restic",
            "target = \"/srv/x\"\noptions = \"vers=4.2\"",
        )
        .unwrap();
        // repository outside the mount target, relative target, remote repo: all rejected
        assert!(with_mount("/elsewhere/repo", "").is_err());
        assert!(with_mount("/srv/x/r", "target = \"srv/x\"").is_err());
        assert!(with_mount("rest:https://h/", "").is_err());
        // a sibling that merely shares a string prefix is not "below" the target
        assert!(with_mount("/run/rbtrfs/repository/r", "").is_err());
    }

    #[test]
    fn retention_converts_and_validates() {
        let cfg: Config = toml::from_str(
            r#"
            [profile.default]
            repository = "/repo"
            password = "x"
            subvolumes = ["/home"]
            keep_local_days = 7
            [profile.default.retention]
            keep_last = 3
            keep_within = "30d"
            "#,
        )
        .unwrap();
        let p = cfg.profile("default").unwrap();
        p.validate().unwrap();
        assert_eq!(p.keep_local_days, Some(7));
        let k = p.retention.as_ref().unwrap().to_keep_options().unwrap();
        assert_eq!(k.keep_last, Some(3));
        assert!(k.keep_within.is_some());

        let bad = Retention {
            keep_within: Some("soon".into()),
            ..Default::default()
        };
        assert!(bad.to_keep_options().is_err());
        assert!(Retention::default().is_empty());
    }

    #[test]
    fn unknown_keys_are_errors_at_every_level() {
        let bad = [
            "[profil.default]\nrepository = \"/r\"",
            "[profile.default]\nrepository = \"/r\"\npassword = \"x\"\nsubvolumes = [\"/h\"]\nkeep_loca = 1",
            "[profile.default]\nrepository = \"/r\"\npassword = \"x\"\nsubvolumes = [\"/h\"]\n[profile.default.hook]\npre = []",
        ];
        for text in bad {
            assert!(toml::from_str::<Config>(text).is_err(), "{text}");
        }
    }

    #[test]
    fn subvolumes_accepts_a_list_or_the_word_all() {
        let parse = |v: &str| {
            toml::from_str::<Config>(&format!(
                "[profile.default]\nrepository = \"/r\"\npassword = \"x\"\nsubvolumes = {v}\n"
            ))
        };
        let get = |v: &str| {
            parse(v)
                .unwrap()
                .profile("default")
                .unwrap()
                .subvolumes
                .clone()
        };
        assert_eq!(get("\"all\""), Subvolumes::All);
        assert_eq!(get("[\"/home\"]"), Subvolumes::List(vec!["/home".into()]));
        assert!(parse("\"everything\"").is_err());
        assert!(parse("5").is_err());
    }

    #[test]
    fn extra_paths_parse() {
        let cfg: Config = toml::from_str(
            "[profile.default]\nrepository = \"/r\"\npassword = \"x\"\nsubvolumes = [\"/\"]\n\
             extra_paths = [\"/boot\", \"/mnt/nas/share\"]\n",
        )
        .unwrap();
        let p = cfg.profile("default").unwrap();
        p.validate().unwrap();
        assert_eq!(
            p.extra_paths,
            [PathBuf::from("/boot"), PathBuf::from("/mnt/nas/share")]
        );
    }

    #[test]
    fn marker_defaults_and_validation() {
        let parse = |extra: &str| {
            let cfg: Config = toml::from_str(&format!(
                "[profile.default]\nrepository = \"/r\"\npassword = \"x\"\nsubvolumes = [\"/h\"]\n{extra}"
            ))
            .unwrap();
            let p = cfg.profile("default").unwrap().clone();
            p.validate().map(|_| p)
        };
        let d = parse("").unwrap();
        assert_eq!(d.exclude_if_present, ["CACHEDIR.TAG"]);
        assert!(d.exclude_if_xattr.is_empty());
        let c =
            parse("exclude_if_present = [\".nobackup\"]\nexclude_if_xattr = [\"user.nobackup\"]\n")
                .unwrap();
        assert_eq!(c.exclude_if_present, [".nobackup"]);
        assert_eq!(c.exclude_if_xattr, ["user.nobackup"]);
        assert!(parse("exclude_if_present = []")
            .unwrap()
            .exclude_if_present
            .is_empty());
        assert!(parse("exclude_if_present = [\"a/b\"]").is_err());
        assert!(parse("exclude_if_present = [\"\"]").is_err());
        assert!(parse("exclude_if_xattr = [\"\"]").is_err());
    }

    #[test]
    fn priority_defaults_and_validation() {
        let parse = |extra: &str| {
            let cfg: Config = toml::from_str(&format!(
                "[profile.default]\nrepository = \"/r\"\npassword = \"x\"\nsubvolumes = [\"/h\"]\n{extra}"
            ))
            .unwrap();
            let p = cfg.profile("default").unwrap().clone();
            p.validate().map(|_| p)
        };
        let d = parse("").unwrap();
        assert_eq!(
            (d.nice, d.io_priority, d.cpu_weight, d.io_weight),
            (10, IoPriority::Low, 20, 20)
        );
        let c =
            parse("nice = 0\nio_priority = \"idle\"\ncpu_weight = 0\nio_weight = 50\n").unwrap();
        assert_eq!(
            (c.nice, c.io_priority, c.cpu_weight, c.io_weight),
            (0, IoPriority::Idle, 0, 50)
        );
        assert!(parse("nice = 20").is_err());
        assert!(parse("nice = -5").is_err());
        assert!(parse("cpu_weight = 10001").is_err());
    }

    #[test]
    fn staging_name_must_be_one_directory_name() {
        let parse = |extra: &str| {
            let cfg: Config = toml::from_str(&format!(
                "[profile.default]\nrepository = \"/r\"\npassword = \"x\"\nsubvolumes = [\"/h\"]\n{extra}"
            ))
            .unwrap();
            let p = cfg.profile("default").unwrap().clone();
            p.validate().map(|_| p)
        };
        assert!(parse("staging_name = \"snaps\"").is_ok());
        for bad in ["", ".", "..", "../x", "a/b", "/abs", "a/"] {
            assert!(
                parse(&format!("staging_name = {bad:?}")).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn compression_level_is_validated() {
        let parse = |c: &str| {
            let cfg: Config = toml::from_str(&format!(
                "[profile.default]\nrepository = \"/r\"\npassword = \"x\"\nsubvolumes = [\"/h\"]\ncompression = {c}\n"
            ))
            .unwrap();
            cfg.profile("default").unwrap().validate()
        };
        for ok in ["0", "3", "22", "-7", "-1"] {
            parse(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in ["23", "-8", "100"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn exclude_subvolumes_defaults_to_empty() {
        let cfg: Config = toml::from_str(
            "[profile.default]\nrepository = \"/r\"\npassword = \"x\"\nsubvolumes = \"all\"\n\
             exclude_subvolumes = [\"/var/cache\"]\n",
        )
        .unwrap();
        assert_eq!(
            cfg.profile("default").unwrap().exclude_subvolumes,
            ["/var/cache"]
        );
        cfg.profile("default").unwrap().validate().unwrap();
    }

    #[test]
    fn rejects_two_password_sources() {
        let cfg: Config = toml::from_str(
            r#"
            [profile.default]
            repository = "/repo"
            password = "a"
            password_file = "/x"
            subvolumes = ["/home"]
            "#,
        )
        .unwrap();
        assert!(cfg.profile("default").unwrap().validate().is_err());
    }
}
