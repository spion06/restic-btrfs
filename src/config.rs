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

    #[serde(default)]
    pub hooks: Hooks,
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
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        for (name, p) in &cfg.profile {
            p.validate().with_context(|| format!("profile [{name}]"))?;
        }
        if cfg.profile.values().any(|p| p.password.is_some()) {
            warn_if_accessible(path, "contains an inline `password`");
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
    fn validate(&self) -> Result<()> {
        let n = self.password.is_some() as u8
            + self.password_file.is_some() as u8
            + self.password_command.is_some() as u8;
        if n != 1 {
            bail!("exactly one of password / password_file / password_command must be set (got {n})");
        }
        if self.subvolumes.is_empty() {
            bail!("`subvolumes` must not be empty");
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
