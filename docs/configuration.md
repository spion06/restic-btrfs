# Configuration

This page describes each setting. For how the file is laid out (profiles,
sub-tables, value types, error messages) see [Config file format](config-file.md).
[`examples/config.toml`](../examples/config.toml) has a complete example.

```toml
[profile.default]
repository       = "/mnt/backup/restic"
password_command = "pass show backup/restic"
subvolumes       = ["/home", "/srv"]
exclude          = ["**/.cache", "*.tmp"]
keep_local       = 1

[profile.default.hooks]
pre  = ["systemctl stop mydb"]
post = ["systemctl start mydb"]

[profile.default.retention]
keep_last  = 3
keep_daily = 7
```

## Keys

| Key | Default | Description |
|---|---|---|
| `repository` | required | Where the restic repository lives. See [Repository](repository.md). |
| `repository_hot` | unset | A separate hot repository. See [Repository](repository.md#hot-and-cold-repositories). |
| `backend_options` | `{}` | Settings for the storage backend. See [Repository](repository.md#backend-options). |
| `backend_options_hot`, `backend_options_cold` | `{}` | The same, for the hot or the cold part only. |
| `password`, `password_file`, `password_command` | one required | The repository password. Set exactly one. |
| `subvolumes` | required | Mount points to back up: a list of exact paths or globs, or `"all"`. |
| `exclude_subvolumes` | `[]` | Mount points to leave out of `subvolumes`. Exact paths or globs. |
| `exclude` | `[]` | Patterns to leave out. See [Filtering](filtering.md). |
| `tags` | `["rbtrfs"]` | Tags to put on each backup. |
| `keep_local` | `1` | How many of the newest local snapshots to keep for each subvolume. |
| `keep_local_days` | unset | Also keep local snapshots younger than this many days. |
| `staging` | `"top-level"` | Where local snapshots are kept: `"top-level"` or `"in-subvolume"`. |
| `staging_name` | `".rbtrfs-snapshots"` | Name of the directory that holds them. |
| `hooks.pre`, `hooks.post` | `[]` | Shell commands to run before and after the snapshots are taken. |
| `hooks.on_failure` | `"abort"` | What to do when a hook fails: `"abort"` or `"warn"`. |
| `retention.*` | unset | The policy used by `rbtrfs forget`. |
| `repository_mount.*` | unset | Mount a share for the run. See [Repository](repository.md). |

## Subvolumes

`subvolumes` says what to back up. It lists mount points, not btrfs subvolume names.
Run `rbtrfs discover` to see what is mounted. Globs such as `"/home/*"` are allowed.

```toml
subvolumes = ["/", "/home", "/srv"]
```

To back up every mounted btrfs subvolume, write `"all"` instead of a list. This is
the same as `["/**"]`, except that rbtrfs does not warn about bind mounts and
repeated mounts it skips.

```toml
subvolumes = "all"
exclude_subvolumes = ["/var/cache", "/var/tmp"]
```

`exclude_subvolumes` removes mount points from the selection, and it also works with
a list. It takes exact paths and globs, like `subvolumes`. An excluded subvolume is
never snapshotted. This is different from `exclude`, which leaves files out of a
subvolume that is backed up.

NB: `"all"` includes the top-level subvolume if it is mounted somewhere. It
contains every other subvolume as an empty directory, so you usually do not want it.
Add its mount point to `exclude_subvolumes`. `rbtrfs backup --dry-run` shows what was
selected and what was skipped.

rbtrfs only snapshots mounts of a whole subvolume. If a path is a bind mount of a
subdirectory, rbtrfs skips it and prints a warning. If the same subvolume is mounted
in two places, it is backed up once, under the first mount point.

btrfs snapshots are not recursive. If a subvolume is nested inside one you selected,
it shows up as an empty directory in the backup unless you select it too. rbtrfs
warns about every such subvolume, mounted or not. Read-only ones, such as snapper
snapshots, are ignored.

## Hooks

`hooks.pre` runs before the snapshots are taken and `hooks.post` runs after. Each
entry is passed to `sh -c`. Use them to pause something that writes to the
subvolumes, as in the example above. The backup itself runs after `post`, so the
pause only lasts as long as it takes to take the snapshots.

Hooks run inside the private mount namespace rbtrfs uses. They see the same mounts
as the host, but anything they mount is not visible outside.

`post` hooks always run, even if a `pre` hook or the snapshot failed. SIGINT,
SIGTERM and SIGHUP are held until they finish. This means a paused service is
resumed on failure or Ctrl-C. SIGKILL cannot be handled, so it skips them.

With `on_failure = "abort"` a failing hook stops the run. With `"warn"` rbtrfs prints
the error and carries on.

## Retention

`retention` is the policy `rbtrfs forget` applies to the repository. The keys are
`keep_last`, `keep_hourly`, `keep_daily`, `keep_weekly`, `keep_monthly` and
`keep_yearly`, which take a number, and `keep_within`, which takes a duration such
as `"14d"`. They have the same meaning as in restic. Without a `retention` table,
`forget` does nothing. See [Maintenance](maintenance.md#retention).

## Staging

rbtrfs keeps each run's read-only snapshots until `keep_local` removes them. The
`staging` key decides where.

`"top-level"` is the default. Snapshots go under the filesystem's top-level
subvolume, in `.rbtrfs-snapshots`. That subvolume is usually not mounted, so rbtrfs
mounts it inside its private mount namespace for the run.

`"in-subvolume"` puts the snapshots in `<mountpoint>/.rbtrfs-snapshots` instead and
leaves that directory out of the backup. Use it if the top-level subvolume cannot be
mounted. Profiles that back up the same subvolumes need different `staging_name`
values.
