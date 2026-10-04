# rbtrfs

rbtrfs is a backup tool for btrfs. It snapshots a set of subvolumes together and
stores them in a restic repository, with each subvolume under its real path
(`/home`, `/srv`, and so on).

The repository is plain restic, so `restic` and `rustic` can read it directly.
rbtrfs uses [rustic_core](https://github.com/rustic-rs/rustic_core) to write it.

A backup takes read-only snapshots of every selected subvolume back to back, then
reads the data from those snapshots instead of the live filesystem. The snapshots
are staged under the top-level subvolume, which rbtrfs mounts inside a private
mount namespace. The host's mount table is never touched, and nothing is left
mounted if the process is killed. Because rustic_core can record a different path
than the one it reads from, files keep the same path from run to run and unchanged
files are not read again.

btrfs cannot snapshot several subvolumes atomically. Creating each snapshot takes
about 2 ms, so the snapshots of one run are a few milliseconds apart. Each file is
consistent, but two files in different subvolumes may not be. If that matters, use
`pre` and `post` hooks to quiesce whatever writes to them.

## Install

Linux only. Release tarballs for x86_64 and aarch64 are on the
[releases page](https://github.com/spion06/restic-btrfs/releases). They need
`libbtrfsutil` (from `btrfs-progs`) at runtime.

To build from source you need Rust 1.91 or newer, the `libbtrfsutil` headers,
`pkg-config` and libclang:

    cargo install --path .

The binary generates its own man page and shell completions:

    rbtrfs man | sudo tee /usr/local/share/man/man1/rbtrfs.1 >/dev/null
    rbtrfs completions bash | sudo tee /etc/bash_completion.d/rbtrfs >/dev/null

## Usage

Write a config to `/etc/rbtrfs/config.toml`:

```toml
[profile.default]
repository    = "/mnt/backup/restic"
password_file = "/etc/rbtrfs/repo.pw"
subvolumes    = ["/", "/home", "/srv"]
exclude       = ["**/.cache", "*.tmp"]
```

Then:

    rbtrfs discover               # list btrfs mounts and subvolumes
    rbtrfs backup --dry-run       # show what would be backed up
    sudo rbtrfs backup            # the first run creates the repository
    rbtrfs snapshots              # list backups
    sudo rbtrfs restore latest --subvol /home --target /mnt/restore

Other commands:

- `ls` and `dump` list a snapshot and print a single file from it.
- `forget` applies a retention policy to the repository, optionally with `--prune`.
- `gc` removes local snapshots left behind by old or crashed runs.
- `man` and `completions` print the man page and shell completions.

`backup`, `gc` and `forget` need root. `latest` means the newest backup from this
host; use `--host NAME` or `--any-host` when restoring onto a different machine.
Use `--config FILE` (or `$RBTRFS_CONFIG`) and `--profile NAME` to choose a
configuration.

## Notes

The repository can be a local path, `rest:https://...` or `rclone:remote:path`.
Only local paths are tested. restic's `sftp:` and `s3:` URLs are not supported;
use `rclone:` for those. A share that is not mounted on the host can be mounted
privately for a run with `repository_mount`, which is described in the
configuration docs.

rustic_core cannot take restic's repository lock. Do not run `restic prune` while
a backup might be running, and do not use `--instant-delete` unless nothing else
is using the repository. `rbtrfs forget --prune` and rustic's own prune are fine.
More in [docs/maintenance.md](docs/maintenance.md).

## Documentation

- [Configuration](docs/configuration.md)
- [Maintenance and safety](docs/maintenance.md)
- [Architecture](docs/architecture.md)
- [Contributing](CONTRIBUTING.md) and [changelog](CHANGELOG.md)

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
