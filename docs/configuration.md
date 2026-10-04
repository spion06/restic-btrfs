# Configuration

This page describes each setting. For how the file is laid out (profiles,
sub-tables, value types, error messages) see [Config file format](config-file.md).
[`examples/config.toml`](https://github.com/spion06/restic-btrfs/blob/main/examples/config.toml) has a complete example.

```toml
[profile.default]
repository       = "/mnt/backup/restic"
password_command = "pass show backup/restic"
subvolumes       = ["/home", "/srv"]
exclude          = ["**/.cache", "*.tmp"]
keep_local       = 1

[profile.default.hooks]
pre  = ["/usr/local/bin/before-snapshot"]
post = ["/usr/local/bin/after-snapshot"]

[profile.default.retention]
keep_last  = 3
keep_daily = 7
```

## Keys

| Key | Default | Description |
|---|---|---|
| `repository` | required | Where the restic repository lives. See [Repository](repository.md). |
| `nice` | `10` | Process niceness for `backup`, `forget` and `gc`. See [Priority](#priority). |
| `io_priority` | `"low"` | Disk I/O priority: `"low"`, `"idle"` or `"normal"`. |
| `cpu_weight` | `20` | CPU share under contention, `0` to turn off. |
| `io_weight` | `20` | Disk share under contention, `0` to turn off. |
| `compression` | unset | zstd compression level for new data. See [Compression](#compression). |
| `repository_hot` | unset | A separate hot repository. See [Repository](repository.md#hot-and-cold-repositories). |
| `backend_options` | `{}` | Settings for the storage backend. See [Repository](repository.md#backend-options). |
| `backend_options_hot`, `backend_options_cold` | `{}` | The same, for the hot or the cold part only. |
| `password`, `password_file`, `password_command` | one required | The repository password. Set exactly one. |
| `subvolumes` | required | Mount points to back up: a list of exact paths or globs, or `"all"`. |
| `extra_paths` | `[]` | Directories on any filesystem to back up live, next to the subvolumes. |
| `exclude_subvolumes` | `[]` | Mount points to leave out of `subvolumes`. Exact paths or globs. |
| `exclude` | `[]` | Patterns to leave out. See [Filtering](filtering.md). |
| `exclude_if_present` | `["CACHEDIR.TAG"]` | Skip directories that contain a file with one of these names. See [Filtering](filtering.md#cache-directories-and-other-markers). |
| `exclude_if_xattr` | `[]` | Skip files and directories that have one of these extended attributes. |
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

rbtrfs only backs up btrfs. Other filesystems, such as an ext4 or vfat `/boot`, tmpfs
or NFS mounted inside a selected subvolume, are not included. They appear as empty
directories, and rbtrfs does not warn about them. With `"all"`, every mounted btrfs
filesystem is included, not just the one holding `/`.

btrfs snapshots are not recursive. If a subvolume is nested inside one you selected,
it shows up as an empty directory in the backup unless you select it too. rbtrfs
warns about every such subvolume, mounted or not. Read-only ones, such as snapper
snapshots, are ignored.

## Compression

Repository data is compressed with zstd. That is the only algorithm the restic
repository format has, so there is nothing to choose. `compression` sets the level:

| Value | Meaning |
|---|---|
| unset | zstd's default level |
| `0` | no compression |
| `1` to `22` | higher levels give smaller data and take longer |
| `-7` to `-1` | faster than level 1, with larger data |

```toml
compression = -3
```

The level is set when rbtrfs creates the repository. If the repository already
exists and its level is different, rbtrfs changes it on the next backup and prints a
line saying so. The change only affects data written afterwards. Existing data is
not rewritten. restic and rustic read both kinds without any setting.

The level belongs to the repository, not to the profile. If two profiles back up to
the same repository with different levels, each run switches it back.

### Choosing a level

zstd's own documentation describes the trade-off: higher levels generally give a
better ratio at the cost of speed and memory, compression speed roughly halves every
two levels, and the progression is not smooth because it depends on the data. The
default is level 3, levels 1 to 19 are the normal range, and 20 to 22 use a lot more
memory. See the [zstd manual](https://github.com/facebook/zstd/blob/dev/programs/zstd.1.md)
and the [benchmarks on the zstd site](https://facebook.github.io/zstd/).

For a feel of what it means in rbtrfs, this is a backup of 10 GB of mixed developer
files (source trees, build output, toolchain binaries) to a local NVMe on a 16-thread
Ryzen 7 5800X, with the files already in the page cache:

| Level | Time | Repository | Ratio | CPU used |
|---|---|---|---|---|
| `0` (off) | 46 s | 6.8 GB | 1.5 | 187 s |
| `-3` | 47 s | 3.3 GB | 3.1 | 193 s |
| unset (3) | 49 s | 2.6 GB | 3.8 | 212 s |
| `9` | 186 s | 2.5 GB | 4.0 | 1513 s |

Up to the default level the time barely changes, so turning compression down or off
does not make a backup faster here. Something other than compression, most likely
chunking and hashing, sets the pace. Level 9 took four times as long and used all the
cores for 4% less data. The default is a good choice unless your data is already
compressed. Results depend heavily on the data, so measure your own if it matters.

## Priority

`backup`, `forget` and `gc` can run for a long time and use a lot of CPU, so by
default they step aside for interactive programs. Four settings control this:

| Key | Default | Effect |
|---|---|---|
| `nice` | `10` | Process niceness, `0` to `19`. `0` leaves it as it was. |
| `io_priority` | `"low"` | `"low"` is the lowest best-effort class, `"idle"` only uses the disk when nothing else does, `"normal"` leaves it alone. |
| `cpu_weight` | `20` | Share of the CPU when the machine is busy. Normal programs have 100. `0` turns it off. |
| `io_weight` | `20` | Share of the disk when it is busy. Normal is 100. `0` turns it off. |

`nice` and `io_priority` apply to rbtrfs and everything it starts, including `rclone`
and your hooks. They only compete with programs in the same cgroup. A game or a
desktop application usually runs in its own systemd scope, and in a test on a desktop
kernel a nice 19 task and a nice 0 task in separate scopes each got about half of the
CPU. That is why there are also weights.

`cpu_weight` and `io_weight` are cgroup weights. When they are set, rbtrfs restarts
itself once inside a transient `systemd-run --scope` with `CPUWeight` and
`IOWeight`, which makes it yield to every other cgroup. The same test with a weight
of 10 gave the low-priority side 18% instead of 50%. This happens only when rbtrfs
runs as root on a systemd machine and is not already a systemd service. If
`systemd-run` is not available, rbtrfs keeps going with just `nice` and
`io_priority`.

In a systemd service, set `CPUWeight=`, `IOWeight=` and, if you like, `Nice=` on the
unit instead. rbtrfs still applies `nice` and `io_priority` itself.

`io_priority` and `io_weight` only have an effect when the disk's I/O scheduler
supports them (BFQ, or the `io.cost` controller). On an NVMe with the `none`
scheduler they do nothing, and `nice` and `cpu_weight` do the work.

To run flat out, for example for a first backup while you are away, set
`nice = 0`, `cpu_weight = 0`, `io_weight = 0` and `io_priority = "normal"`.

## Extra paths

rbtrfs only snapshots btrfs. `extra_paths` adds directories from other filesystems,
such as `/boot`, to the same backup:

```toml
subvolumes  = ["/", "/home"]
extra_paths = ["/boot", "/mnt/nas/share"]
```

Each path must be an absolute path to a directory. It does not have to be a mount
point. An extra path is stored under its real path, so `rbtrfs restore --subvol /boot`
and `rbtrfs ls` work like they do for subvolumes. It is also incremental, and the
`exclude` patterns apply to it.

Extra paths are not snapshots. rbtrfs reads them live, after the snapshots are
taken and after the `post` hooks have run. A file that changes while it is being
read can end up inconsistent, and hooks cannot help, because they have finished by then.
For `/boot` that rarely matters.

rbtrfs checks the paths before it takes any snapshot:

- A path inside a selected subvolume is an error, because the snapshot already
  covers it and a live copy would take its place in the backup. A directory on a
  different filesystem that is mounted inside a selected subvolume, like `/boot`
  under `/`, is fine.
- A path on a btrfs filesystem that you did not select gets a warning. Add that
  mount to `subvolumes` to back it up from a snapshot.
- A path that is missing, relative, or not a directory is an error.

## Hooks

`hooks.pre` runs before the snapshots are taken and `hooks.post` runs after. Each
entry is a shell command, passed to `sh -c`, and they run in the order listed. Use
them to get an application's data into a consistent state before the snapshots and to
release it again afterwards. The backup itself runs after `post`, from the snapshots,
so nothing has to stay held back while it runs.

Hooks run inside the private mount namespace rbtrfs uses. They see the same mounts
as the host, but anything they mount is not visible outside.

`post` hooks always run, even if a `pre` hook or the snapshot failed. SIGINT,
SIGTERM and SIGHUP are held until they finish. This means whatever a `pre` hook
set up is undone on failure or Ctrl-C. SIGKILL cannot be handled, so it skips them.

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
