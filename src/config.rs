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
//! pre  = ["systemctl stop mydb"]
//! post = ["systemctl start mydb"]
//! on_failure = "abort"   # or "warn"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub profile: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// restic repository location (`/path`, `rest:`, `s3:…`, `sftp:…`, …).
    pub repository: String,

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

    /// Mount points to capture. Shell-style globs allowed; matched against the
    /// set of currently mounted btrfs subvolumes.
    pub subvolumes: Vec<String>,

    /// Exclude patterns passed through to the backup engine.
    #[serde(default)]
    pub exclude: Vec<String>,

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
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
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
        if self.subvolumes.is_empty() {
            bail!("`subvolumes` must not be empty");
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
