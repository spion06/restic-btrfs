# rbtrfs snapshots

List the backups in the repository

## Usage

```
rbtrfs snapshots [OPTIONS]
```

## Options

### `--profile <PROFILE>`

Profile to use from the config file.

Default: `default`

### `--all`

Also list the internal per-subvolume snapshots that each backup is merged from.

### `-c, --config <CONFIG>`

Config file. Defaults to $RBTRFS_CONFIG, then /etc/rbtrfs/config.toml.
