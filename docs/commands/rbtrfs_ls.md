# rbtrfs ls

List the contents of a backup

## Usage

```
rbtrfs ls [OPTIONS] <SNAPSHOT> [PATH]
```

## Arguments

### `<SNAPSHOT>`

Snapshot id, or `latest` for the newest backup from this host.

### `<PATH>`

Recorded path to list, for example `/home/alice`.

Default: `/`

## Options

### `--profile <PROFILE>`

Profile to use from the config file.

Default: `default`

### `--host <HOST>`

Take `latest` from this host instead of the current one.

### `--any-host`

Take `latest` from any host.

### `-c, --config <CONFIG>`

Config file. Defaults to $RBTRFS_CONFIG, then /etc/rbtrfs/config.toml.
