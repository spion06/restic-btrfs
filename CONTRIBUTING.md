# Contributing

Issues and pull requests are welcome.

## Build and test

```
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Rust 1.91 or newer, plus the btrfs-progs headers (`libbtrfsutil`), `pkg-config` and
libclang.

The end-to-end tests need root (loop devices, mounts). When you aren't root, each
test re-runs itself in a privileged container, so you only need access to a Docker
daemon (your user in the `docker` group, or `DOCKER_HOST`). Without one they skip
with a notice; set `RBTRFS_E2E_REQUIRED=1` to fail instead. As root they run
directly and need `mkfs.btrfs`, `losetup`, `setfattr`/`getfattr` and `restic`.

## Docs

`docs/commands/` is generated from the CLI definition. After changing a flag or its
help text, regenerate it, or the test suite fails:

    cargo run -- gendocs docs/commands

## Guidelines

- Keep btrfs calls behind `BtrfsOps`, and keep the namespace `unshare` first in `main`.
- A bug fix should come with a test that fails without it.
- Update the README, `docs/` and `CHANGELOG.md` when behaviour changes.
- Prefer small commits with a message explaining the why.

## Releasing

Update `version` in `Cargo.toml` and move the `CHANGELOG.md` entries under the new
version, commit, then push a matching tag (`git tag v0.2.0 && git push origin v0.2.0`).
The release workflow runs CI, builds x86_64 and aarch64 Linux tarballs (binary, man
page, completions, licences) and publishes them as a GitHub release.

See [`docs/architecture.md`](docs/architecture.md) for how the pieces fit.

By contributing you agree your work is licensed under MIT OR Apache-2.0.
