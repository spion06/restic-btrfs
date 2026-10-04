# rbtrfs dump

Write one file from a backup to standard output

## Usage

```
rbtrfs dump [OPTIONS] <SNAPSHOT> <PATH>
```

## Arguments

### `<SNAPSHOT>`

Snapshot id, or `latest` for the newest backup from this host.

### `<PATH>`

Recorded path of the file, for example `/etc/fstab`.

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
