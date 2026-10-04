# rbtrfs

[![CI](https://github.com/spion06/restic-btrfs/actions/workflows/ci.yml/badge.svg)](https://github.com/spion06/restic-btrfs/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20%7C%20Apache--2.0-blue)](#license)

[Install](docs/install.md) · [Usage](docs/usage.md) · [Documentation](https://spion06.github.io/restic-btrfs/) · [Releases](https://github.com/spion06/restic-btrfs/releases)

rbtrfs *("restic for btrfs snapshots")* is a command-line program that backs up btrfs
subvolumes together into a restic repository, with each subvolume stored under its
real path.

A run takes read-only snapshots of all the selected subvolumes back to back, then
reads the backup from those snapshots instead of the live filesystem. The result is
one restic snapshot containing `/`, `/home`, `/srv` and whatever else you chose.

## Features

- Snapshots all selected subvolumes back to back, about 2 ms apart ([how](docs/architecture.md))
- Backs up from the read-only snapshots, not the live data
- Backs up the subvolumes you list, or every mounted one with `subvolumes = "all"` ([configuration](docs/configuration.md#subvolumes))
- Can add directories from other filesystems, such as `/boot`, to the same backup ([extra paths](docs/configuration.md#extra-paths))
- Stores one restic snapshot with every subvolume at its real path
- Writes a plain restic repository that `restic` and `rustic` can read ([repository](docs/repository.md))
- Skips unchanged files, because paths stay the same from run to run
- Stages snapshots in a private mount namespace, so the host's mount table is never touched
- Runs `pre` and `post` hooks around the snapshot, and always runs `post` ([hooks](docs/usage.md#hooks))
- Excludes files with restic-style patterns ([filtering](docs/filtering.md))
- Applies a retention policy and prunes ([maintenance](docs/maintenance.md))
- Restores to a directory or a new btrfs subvolume, and can list and print files from a backup
- Stores to a local path, a REST server, rclone or an OpenDAL service such as S3 ([repository](docs/repository.md))
- Can mount an NFS or CIFS repository privately for the run ([repository](docs/repository.md#a-share-that-is-not-mounted-on-the-host))
- Generates its own man page and shell completions

btrfs cannot snapshot several subvolumes atomically, so two files in different
subvolumes may be a few milliseconds apart. Each file is consistent. If an
application needs more than that, use hooks to get it into a consistent state first.

## Quick start

```toml
# /etc/rbtrfs/config.toml
[profile.default]
repository    = "/mnt/backup/restic"
password_file = "/etc/rbtrfs/repo.pw"
subvolumes    = ["/", "/home", "/srv"]
exclude       = ["**/.cache", "*.tmp"]
```

    rbtrfs discover                # list btrfs mounts and subvolumes
    rbtrfs backup --dry-run        # show what would be backed up
    sudo rbtrfs backup             # the first run creates the repository
    rbtrfs snapshots               # list backups
    sudo rbtrfs restore latest --subvol /home --target /mnt/restore

## Installation and documentation

- [Documentation site](https://spion06.github.io/restic-btrfs/)
- [Install](docs/install.md)
- [Usage](docs/usage.md): configure, back up, restore, schedule, exit codes
- [Config file format](docs/config-file.md)
- [Configuration](docs/configuration.md)
- [Filtering](docs/filtering.md)
- [Repository](docs/repository.md)
- [Maintenance and safety](docs/maintenance.md)
- [Command reference](docs/commands/index.md)
- [Architecture](docs/architecture.md)
- [Contributing](CONTRIBUTING.md) and [changelog](CHANGELOG.md)

NB: do not run `restic prune` while a backup might be running, and do not use
`forget --prune --instant-delete` unless nothing else is using the repository.
rbtrfs cannot take restic's repository lock. See
[maintenance](docs/maintenance.md#concurrency).

## Downloads

Release tarballs for Linux x86_64 and aarch64 are on the
[releases page](https://github.com/spion06/restic-btrfs/releases).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT)
at your option.
