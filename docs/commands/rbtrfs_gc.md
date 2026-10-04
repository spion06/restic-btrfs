# rbtrfs gc

Delete local snapshots left behind by old or crashed runs.

A backup removes old local snapshots itself. Use this to reclaim ones left by a run that was killed, or to apply different `keep_local` settings.

Needs root.

## Usage

```
rbtrfs gc [OPTIONS]
```

## Options

### `--profile <PROFILE>`

Profile to use from the config file.

Default: `default`

### `--keep-local <KEEP_LOCAL>`

Keep this many of the newest snapshot sets per subvolume. Overrides the profile's `keep_local`.

### `--keep-local-days <KEEP_LOCAL_DAYS>`

Also keep sets younger than this many days. Overrides the profile's `keep_local_days`.

### `--all-keys`

Also clean up subvolumes that the profile no longer selects.

### `-c, --config <CONFIG>`

Config file. Defaults to $RBTRFS_CONFIG, then /etc/rbtrfs/config.toml.
