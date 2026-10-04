# rbtrfs forget

Apply the retention policy to the repository.

Keeps merged backups according to the profile's `retention` table, per host, and removes the internal per-subvolume snapshots that no future run needs. Snapshots that rbtrfs did not create are never touched. Stops with an error if the profile has no `retention` table.

Needs root, because it takes the same lock as `backup`.

## Usage

```
rbtrfs forget [OPTIONS]
```

## Options

### `--profile <PROFILE>`

Profile to use from the config file.

Default: `default`

### `--prune`

Also delete data that no remaining snapshot references. By default rustic only marks it and removes it on a later prune.

### `--instant-delete`

With `--prune`, delete unreferenced data immediately. This skips rustic's two-phase pruning and can corrupt the repository if anything else is using it. On a terminal you are asked to confirm.

### `--allow-unsafe`

Confirm `--instant-delete` without asking. Required when not running on a terminal.

### `--dry-run`

Show what would be forgotten and change nothing.

### `-c, --config <CONFIG>`

Config file. Defaults to $RBTRFS_CONFIG, then /etc/rbtrfs/config.toml.
