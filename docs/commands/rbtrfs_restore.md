# rbtrfs restore

Restore one subvolume from a backup.

Writes the files recorded under `--subvol` into `--target`. Run it as root to restore ownership. With `--as-subvolume` the target is created as a new btrfs subvolume; subvolumes that were nested inside the original come back as plain directories.

## Usage

```
rbtrfs restore [OPTIONS] --subvol <SUBVOL> --target <TARGET> <SNAPSHOT>
```

## Arguments

### `<SNAPSHOT>`

Snapshot id, or `latest` for the newest backup from this host.

## Options

### `--profile <PROFILE>`

Profile to use from the config file.

Default: `default`

### `--subvol <SUBVOL>`

Recorded path of the subvolume to restore, for example `/home`.

### `--target <TARGET>`

Directory to restore into.

### `--host <HOST>`

Take `latest` from this host instead of the current one. Useful when restoring onto a rebuilt machine.

### `--any-host`

Take `latest` from any host.

### `--as-subvolume`

Create the target as a new btrfs subvolume. Needs root, and the target must be on btrfs and must not exist.

### `--by-name`

Restore owners by user and group name instead of numeric id. Numeric ids are the default: they are right for a restore onto the same system or a rebuilt one with the same ids, and do not depend on the names existing.

### `-c, --config <CONFIG>`

Config file. Defaults to $RBTRFS_CONFIG, then /etc/rbtrfs/config.toml.
