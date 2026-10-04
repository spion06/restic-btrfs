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
    pub fn open_or_init(&self) -> Result<Repository<rustic_core::OpenStatus>> {
        if !self.exists()? {
            Repository::new(&self.repo_opts, &self.backends)?
                .init(
                    &self.creds,
                    &KeyOptions::default(),
                    &ConfigOptions::default(),
                )
                .context("initializing repository")?;
            self.ensure_locks_dir();
        }
        self.open()
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
