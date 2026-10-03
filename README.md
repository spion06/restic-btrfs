# rbtrfs

Consistent btrfs-snapshot backups into a **restic-format** repository.

`rbtrfs backup` snapshots every selected btrfs subvolume back-to-back, then backs the
read-only snapshots up as **one restic snapshot** with each subvolume at its real
mount path (`/home`, `/srv`, …). The repository is plain restic: `restic` and
`rustic` read it directly.

## How it works

1. Run `pre` hooks (e.g. quiesce a database).
2. Create a read-only snapshot of every selected subvolume in one tight burst
   (about 2 ms per subvolume), then run `post` hooks.
3. Back up each snapshot with [rustic_core](https://github.com/rustic-rs/rustic_core),
   recording the original mount point instead of the staging path, so paths stay
   stable and unchanged files are never re-read.
4. Merge the per-subvolume results into a single snapshot and delete old local
   snapshots.

The snapshots live under the filesystem's top-level subvolume, which rbtrfs mounts
inside a **private mount namespace**: nothing is added to your host's mount table,
and the kernel cleans up even if the process is killed. No FUSE, no bind mounts.

btrfs has no atomic multi-subvolume snapshot, so cross-subvolume ordering is not
guaranteed (per-file integrity is). Use hooks to quiesce anything that needs more.

## Install

```
cargo install --path .        # Rust >= 1.91
```

Build needs the btrfs-progs headers (`libbtrfsutil`), `pkg-config` and libclang;
runtime needs `libbtrfsutil` and, for `repository_mount`, `mount(8)`.

## Quick start

```toml
# /etc/rbtrfs/config.toml   (mode 0600 if it holds a password)
[profile.default]
repository       = "/mnt/backup/restic"
password_file    = "/etc/rbtrfs/repo.pw"      # or password / password_command
subvolumes       = ["/", "/home", "/srv"]     # mount points; globs allowed
exclude          = ["**/.cache", "*.tmp", "/home/alice/Downloads"]
keep_local       = 1                          # local snapshot sets to keep
```

```
rbtrfs discover                  # what rbtrfs sees; copy mount points from here
rbtrfs backup --dry-run          # check the plan (no root needed)
sudo rbtrfs backup               # the first run initialises the repository
rbtrfs snapshots                 # list backups
sudo rbtrfs restore latest --subvol /home --target /mnt/restore
```

## Commands

| command | purpose |
|---|---|
| `discover [--json]` | show btrfs filesystems, mounts and subvolumes (read-only) |
| `backup [--dry-run]` | snapshot, back up, merge, clean up local snapshots |
| `snapshots [--all]` | list backups (`--all` also shows the internal per-subvolume parts) |
| `restore <id\|latest> --subvol P --target DIR [--as-subvolume]` | restore one subvolume; `--as-subvolume` makes `DIR` a new btrfs subvolume |
| `ls <id\|latest> [PATH]` / `dump <id\|latest> PATH` | list a snapshot / print one file |
| `forget [--prune] [--dry-run]` | thin the repository per `[retention]` |
| `gc [--keep-local N] [--keep-local-days D] [--all-keys]` | delete local snapshots left by old or crashed runs |

Common options: `--config FILE` (or `$RBTRFS_CONFIG`; default
`/etc/rbtrfs/config.toml`) and `--profile NAME` (default `default`). `latest` means
the newest backup *from this host*; use `--host NAME` or `--any-host` (restoring
onto a rebuilt machine). `backup`, `gc` and `forget` need root.

## Configuration

```toml
[profile.default]
repository       = "/mnt/backup/restic"
password_command = "pass show backup/restic"   # exactly one of password / password_file / password_command
subvolumes       = ["/home", "/srv"]
exclude          = ["**/.cache", "*.tmp"]
tags             = ["rbtrfs"]                  # tags on the merged snapshot
keep_local       = 1                           # newest N local snapshot sets per subvolume
keep_local_days  = 3                           # ...plus any younger than this (optional)
staging          = "top-level"                 # or "in-subvolume" (see below)

[profile.default.hooks]
pre        = ["systemctl stop mydb"]
post       = ["systemctl start mydb"]
on_failure = "abort"                           # or "warn"

[profile.default.retention]                    # for `rbtrfs forget`
keep_last = 3
keep_daily = 7
keep_weekly = 4
keep_monthly = 12
keep_within = "14d"
```

- **Subvolumes** are chosen by mount point. Bind mounts of a subdirectory are
  skipped, and a subvolume mounted twice is recorded once. Snapshots are not
  recursive: a subvolume nested inside a selected one appears as an empty
  directory, and rbtrfs warns unless you select it too.
- **Excludes** work like restic's: `/path` is a path as recorded in the backup
  (it applies to the subvolume that contains it), anything else matches at any
  depth (`*.tmp`, `.cache`, `alice/.cache`), and a trailing `/` matches
  directories only.
- **Hooks** run inside the private namespace. `post` hooks always run once the
  window opened, even if a `pre` hook or the snapshot failed, and
  SIGINT/SIGTERM/SIGHUP are held until they finish, so a quiesced service is
  thawed on failure or Ctrl-C (SIGKILL excepted).
- **`staging = "in-subvolume"`** puts snapshots in `<mountpoint>/.rbtrfs-snapshots/`
  (excluded from the backup) for hosts that can't mount the top-level subvolume.
  Profiles sharing subvolumes need different `staging_name`s.
- **Retention.** `forget` keeps merged snapshots per the policy (per host) and
  drops the internal part snapshots older than the newest run. Snapshots rbtrfs
  didn't create are never touched; with no `[retention]` it does nothing.
  `--prune` also frees unreferenced data, lazily by default. `--instant-delete`
  frees it now but is unsafe if anything else uses the repository, so it asks you
  to type `yes` on a terminal and otherwise needs `--allow-unsafe`.

### Repository location

`repository` is a local path (the only one covered by the tests), `rest:https://…`
or `rclone:remote:path` (needs `rclone`); the latter two are accepted but untested
here. restic-style `sftp:`/`s3:` URLs are **not** supported: reach those through
`rclone:`. For a share that isn't mounted on the host (NFS, CIFS, …), have
rbtrfs mount it privately for the run:

```toml
[profile.default]
repository = "/run/rbtrfs/repo/restic/mybox"   # a path below the mount target

[profile.default.repository_mount]
type    = "nfs"                                 # anything mount(8) understands
source  = "nas.local:/export/backups"
options = "vers=4.2"                            # optional
target  = "/run/rbtrfs/repo"                    # optional (default)
```

The mount is private to the process. Because it needs the namespace, **every command
then needs root** (not just `backup`). Only a local filesystem stands in for the
export in the tests, not a real NFS server.

## Concurrency and safety

One `backup`/`gc`/`forget` runs at a time per machine (`/run/rbtrfs/rbtrfs.lock`).
rustic_core cannot take restic's repository lock, so other tools are on their own.
Against a running `rbtrfs backup`:

| other activity | safe? |
|---|---|
| rustic prune/forget (default options), `restic backup`, readers | yes (rustic's two-phase pruning; tested) |
| `rbtrfs forget --prune` on the same machine | yes (run lock) |
| **`restic prune` / `restic forget --prune`** | **no**: reproduced a missing pack while the backup exited 0 |
| **`--instant-delete`** (any tool) | **no**: skips the two-phase safety |

Prune with `rbtrfs forget --prune` (or rustic), and run restic's own prune only
when no backup can be running.

## Tests

`cargo test` runs everything. The end-to-end tests build real btrfs filesystems on
loop devices and need root; when you aren't root each test re-runs itself in its
own privileged container ([testcontainers](https://crates.io/crates/testcontainers):
no sudo, parallel, nothing touches your mounts), which needs Docker access. Without
it they skip with a notice; `RBTRFS_E2E_REQUIRED=1` makes that an error. As root
they run directly and need `mkfs.btrfs`, `losetup`, `setfattr` and `restic` on
`PATH`. The suite also checks the repository with the official `restic check
--read-data` (restic 0.19). `DESIGN.md` has the architecture and decisions;
`spikes/` holds the original proofs of concept.

## License

MIT OR Apache-2.0 ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).
