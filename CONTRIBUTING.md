# Contributing

Issues and pull requests are welcome.

## Build and test

```
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Rust 1.91 or newer, plus `pkg-config`, libclang and the `libbtrfsutil` headers from
btrfs-progs 6.8 or newer (see [Install](docs/install.md#from-source) if your
distribution's are older).

The end-to-end tests need root (loop devices, mounts). When you aren't root, each
test re-runs itself in a privileged container, so you only need access to a Docker
daemon (your user in the `docker` group, or `DOCKER_HOST`). Without one they skip
with a notice; set `RBTRFS_E2E_REQUIRED=1` to fail instead. As root they run
directly and need `mkfs.btrfs`, `losetup`, `setfattr`/`getfattr` and `restic`.

## Docs

`docs/commands/` is generated from the CLI definition. After changing a flag or its
help text, regenerate it, or the test suite fails:

    cargo run -- gendocs docs/commands

The docs are also published as a site, built with [mdBook](https://rust-lang.github.io/mdBook/)
from `docs/` (`book.toml`, `docs/SUMMARY.md`). To preview it, install mdBook and run
`mdbook serve`. Add a new page to `docs/SUMMARY.md` or it will not appear on the site.

## Guidelines

- Keep btrfs calls behind `BtrfsOps`, and keep the namespace `unshare` first in `main`.
- A bug fix should come with a test that fails without it.
- Update the README, `docs/` and `CHANGELOG.md` when behaviour changes.
- Prefer small commits with a message explaining the why.

## Commit messages

Releases are versioned and the changelog is written from commit messages, so use
[conventional commits](https://www.conventionalcommits.org/):

| Prefix | Use for | Version bump |
|---|---|---|
| `feat:` | a new feature or option | patch |
| `fix:` | a bug fix | patch |
| `feat!:` or `fix!:` (or `BREAKING CHANGE:` in the body) | a change that breaks existing configs or behaviour | minor |
| `docs:`, `perf:`, `refactor:` | shown in the changelog | patch |
| `chore:`, `ci:`, `test:`, `build:`, `style:` | not shown in the changelog | none |

An optional scope goes in brackets, for example `fix(restore): handle a missing target`.
While the version is below 1.0 a breaking change bumps the minor number and everything
else bumps the patch number (the same rules Cargo uses). Going to 1.0.0 is a deliberate,
manual release. Commits that are not conventional are ignored by the changelog.

## Releasing

There are two ways to release. Both run the same build, CI and publish steps.

**From the Actions tab:** **release → Run workflow**.

- **bump** is `auto` (work out the version from the commits since the last tag) or a
  forced `patch`, `minor` or `major`.
- **dry run** only shows the next version and the changelog entry in the run summary
  and publishes nothing. Do this first.

A real run bumps `version` in `Cargo.toml` and `Cargo.lock`, writes the new section
into `CHANGELOG.md`, builds the x86_64 and aarch64 tarballs from that commit, and runs
the CI checks. Only if all of that passes does it push the release commit to `main`,
tag it and publish the GitHub release, with the changelog section as the notes. If
anything fails, `main` is untouched and you can simply run it again.

**By pushing a tag:** prepare the release commit yourself, then push a `vX.Y.Z` tag.

    .github/scripts/prepare-release.sh auto     # needs git-cliff; or patch, minor, major
    git add Cargo.toml Cargo.lock CHANGELOG.md
    git commit -m "chore(release): vX.Y.Z"
    git tag -a vX.Y.Z -m "rbtrfs X.Y.Z"
    git push origin main vX.Y.Z

The workflow checks that the tag matches the version in `Cargo.toml` and that
`CHANGELOG.md` has a section for it, then builds and publishes the tag as it is. If the
build fails nothing is published; delete the tag (`git push origin :vX.Y.Z`), fix the
problem and tag again.

With `auto` and no `feat`, `fix`, `docs`, `perf` or `refactor` commits since the last
tag, the script stops with "nothing to release".

The configuration is in `cliff.toml` (git-cliff) and `.github/scripts/prepare-release.sh`.

See [`docs/architecture.md`](docs/architecture.md) for how the pieces fit.

By contributing you agree your work is licensed under MIT OR Apache-2.0.
