//! Thin helpers around a rustic_core repository.

use anyhow::{Context, Result};
use rustic_core::{
    ConfigOptions, Credentials, KeyOptions, Repository, RepositoryBackends, RepositoryOptions,
};

use crate::config::Profile;

pub struct RepoHandle {
    backends: RepositoryBackends,
    repo_opts: RepositoryOptions,
    creds: Credentials,
    repository: String,
    /// Wanted zstd level (`None` = leave the repository's setting alone).
    compression: Option<i32>,
}

impl RepoHandle {
    pub fn from_profile(profile: &Profile) -> Result<Self> {
        let password = profile.resolve_password()?;
        let backends = profile
            .backend()
            .to_backends()
            .context("configuring restic backend")?;
        Ok(Self {
            backends,
            repo_opts: RepositoryOptions::default(),
            creds: Credentials::password(password),
            repository: profile.repository.clone(),
            compression: profile.compression,
        })
    }

    /// Does a repository (a restic `config` file) already exist at the location?
    pub fn exists(&self) -> Result<bool> {
        Ok(Repository::new(&self.repo_opts, &self.backends)
            .context("constructing repository")?
            .config_id()
            .context("looking for repository config")?
            .is_some())
    }

    /// Open the repository, initializing it only if none exists yet. A wrong
    /// password or an unreachable backend is an error, never a reason to init.
    ///
    /// A configured `compression` is set when the repository is created, and
    /// applied to an existing repository if it differs (it is a repository-wide
    /// setting that only affects data written from then on).
    pub fn open_or_init(&self, auto_init: bool) -> Result<Repository<rustic_core::OpenStatus>> {
        if !self.exists()? {
            if !auto_init {
                anyhow::bail!(
                    "no repository at {}; create it with `rbtrfs init`",
                    self.repository
                );
            }
            self.init()?;
        }
        let mut repo = self.open()?;
        if let Some(level) = self.compression {
            if repo.config().compression != Some(level) {
                repo.apply_config(&ConfigOptions::default().set_compression(level))
                    .context("applying the compression setting to the repository")?;
                println!("repository compression set to level {level}");
            }
        }
        Ok(repo)
    }

    /// Create the repository. Fails if one already exists.
    pub fn init(&self) -> Result<()> {
        if self.exists()? {
            anyhow::bail!("a repository already exists at {}", self.repository);
        }
        let mut config = ConfigOptions::default();
        if let Some(level) = self.compression {
            config = config.set_compression(level);
        }
        Repository::new(&self.repo_opts, &self.backends)?
            .init(&self.creds, &KeyOptions::default(), &config)
            .context("initializing repository")?;
        self.ensure_locks_dir();
        Ok(())
    }

    pub fn open(&self) -> Result<Repository<rustic_core::OpenStatus>> {
        Repository::new(&self.repo_opts, &self.backends)?
            .open(&self.creds)
            .context("opening repository")
    }

    /// rustic_core does not create `locks/` on init; the `restic` CLI needs it.
    fn ensure_locks_dir(&self) {
        let r = &self.repository;
        let local = r.strip_prefix("local:").unwrap_or(r);
        if !local.contains(':') {
            let _ = std::fs::create_dir_all(std::path::Path::new(local).join("locks"));
        }
    }
}
