# rbtrfs backup

Snapshot the selected subvolumes and back them up.

Runs the `pre` hooks, takes a read-only snapshot of every selected subvolume back to back, runs the `post` hooks, then backs the snapshots up into the repository as one snapshot with each subvolume at its real mount path. Finally it deletes local snapshots beyond `keep_local`. The repository is created on the first run.

Needs root. Only one backup, gc or forget runs at a time on a machine.

## Usage

```
rbtrfs backup [OPTIONS]
```

## Options

### `--profile <PROFILE>`

Profile to use from the config file.

Default: `default`

### `--dry-run`

Print the plan and check the repository and password, but change nothing. Does not need root.

### `-c, --config <CONFIG>`

Config file. Defaults to $RBTRFS_CONFIG, then /etc/rbtrfs/config.toml.
