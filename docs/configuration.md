# Configuration

A TOML file with one or more named profiles. Default path `/etc/rbtrfs/config.toml`;
override with `--config FILE` or `$RBTRFS_CONFIG`. Pick a profile with
`--profile NAME` (default `default`). Keep the file mode `0600` if it holds a
password (rbtrfs warns otherwise). A complete example is in
[`examples/config.toml`](../examples/config.toml).

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
keep_last = 3
keep_daily = 7
```

## Keys

| key | default | meaning |
|---|---|---|
| `repository` | required | where the restic repository lives; see [Repository location](#repository-location) |
| `password` / `password_file` / `password_command` | one required | the repository password (exactly one of the three) |
| `subvolumes` | required | mount points to back up; exact paths or globs |
| `exclude` | `[]` | patterns to leave out; see [Excludes](#excludes) |
| `tags` | `["rbtrfs"]` | tags on the merged snapshot |
| `keep_local` | `1` | newest N local snapshot sets kept per subvolume |
| `keep_local_days` | unset | also keep local sets younger than this many days |
| `staging` | `"top-level"` | `"top-level"` or `"in-subvolume"`; see [Staging](#staging) |
| `staging_name` | `".rbtrfs-snapshots"` | name of the staging directory |
| `hooks.pre` / `hooks.post` | `[]` | shell commands around the snapshot burst |
| `hooks.on_failure` | `"abort"` | `"abort"` or `"warn"` when a hook fails |
| `retention.*` | unset | policy for `rbtrfs forget`; see [Retention](#retention) |
| `repository_mount.*` | unset | mount a share privately for the run |

## Subvolumes

Subvolumes are chosen by mount point (`rbtrfs discover` lists them).

- Only whole-subvolume mounts are snapshotted. A bind mount of a subdirectory is
  skipped with a warning; a subvolume mounted twice is recorded once.
- btrfs snapshots are not recursive: a subvolume nested inside a selected one
  appears as an empty directory. rbtrfs warns about nested subvolumes (mounted or
  not) you did not select; read-only ones (snapper snapshots) are ignored.

## Excludes

Patterns work like restic's `--exclude`:

- `/home/alice/Downloads`: a path starting with `/` is a path *as recorded in the
  backup*; it applies to the subvolume that contains it. Globs are fine after the
  mount point (`/home/*/tmp`), not before it.
- anything else matches at any depth: `*.tmp`, `.cache`, `alice/.cache`.
- a trailing `/` matches directories only (`node_modules/`).
- do not prefix a pattern with `!`.

## Hooks

Shell commands run before (`pre`) and after (`post`) the snapshot burst, inside
rbtrfs' private mount namespace: they see the host's mounts, but mounts they make
are not visible outside.

`post` hooks always run once the window opened, even if a `pre` hook or the
snapshot failed, and SIGINT/SIGTERM/SIGHUP are held until they finish, so a
quiesced service is thawed on failure or Ctrl-C (SIGKILL excepted). With
`on_failure = "warn"` a failing hook is reported but the run continues.

## Retention

`rbtrfs forget` applies `[profile.x.retention]` to the merged snapshots, per host:
`keep_last`, `keep_hourly`, `keep_daily`, `keep_weekly`, `keep_monthly`,
`keep_yearly`, and `keep_within` (a duration such as `"14d"`). Without a
`[retention]` table it does nothing. See [Maintenance](maintenance.md).

## Staging

`top-level` (default): snapshots go under the top-level subvolume
(`<subvolid=5>/.rbtrfs-snapshots/`), which rbtrfs mounts inside its private
namespace.

`in-subvolume`: snapshots go to `<mountpoint>/.rbtrfs-snapshots/` and are excluded
from the backup. Use it where the top-level subvolume can't be mounted. Profiles
that share subvolumes need different `staging_name`s.

## Repository location

`repository` is a local path (the only kind covered by the tests), `rest:https://…`,
or `rclone:remote:path` (needs `rclone`); the last two are accepted but untested
here. restic-style `sftp:` and `s3:` URLs are not supported; reach those with
`rclone:`.

### Repository on NFS or another share

If the share isn't mounted on the host, let rbtrfs mount it privately for the run:

```toml
[profile.default]
repository = "/run/rbtrfs/repo/restic/mybox"   # a path below the mount target

[profile.default.repository_mount]
type    = "nfs"                                 # anything mount(8) understands
source  = "nas.local:/export/backups"
options = "vers=4.2"                            # optional
target  = "/run/rbtrfs/repo"                    # optional (default)
```

rbtrfs runs `mount -t <type> [-o <options>] <source> <target>` inside its private
namespace before opening the repository, so `mount.nfs` and friends resolve
hostnames and options as usual. The mount never reaches the host's mount table and
disappears with the process. Because it needs the namespace, every command that
opens the repository then needs root. A local filesystem stands in for the export
in the tests; a real NFS server is not covered.
