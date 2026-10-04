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

Print the plan and check the repository and password, but change nothing. It also walks the selected paths with the real exclude patterns and reports how much would be stored and which paths are skipped. Needs root only if you cannot read the config file or the repository (or the profile mounts the repository); run it as root to see every directory.

### `--no-scan`

With `--dry-run`, skip the walk over the files and only print the plan.

### `--no-init`

Fail if the repository does not exist instead of creating it. Same as `auto_init = false` in the profile.

### `--report-excluded`

List the directories left out because they hold a marker file (`exclude_if_present`, such as CACHEDIR.TAG) or an extended attribute (`exclude_if_xattr`). Same as `report_excluded = true` in the profile.

### `-c, --config <CONFIG>`

Config file. Defaults to $RBTRFS_CONFIG, then /etc/rbtrfs/config.toml.
