# rbtrfs discover

Show btrfs filesystems, mounts and subvolumes.

Lists every mounted btrfs filesystem with its mounts, and, when run as root, all of its subvolumes. Use it to find the mount points to put in `subvolumes`. It changes nothing.

## Usage

```
rbtrfs discover [OPTIONS]
```

## Options

### `--json`

Print JSON instead of text.

### `-c, --config <CONFIG>`

Config file. Defaults to $RBTRFS_CONFIG, then /etc/rbtrfs/config.toml.
