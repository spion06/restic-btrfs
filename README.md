# rbtrfs

Consistent btrfs-snapshot backups into a **restic-format** repository.

`rbtrfs backup` snapshots every selected btrfs subvolume in one tight burst (one
consistent point in time), then backs those read-only snapshots up as a **single
restic snapshot** whose tree holds each subvolume at its real mount path. No FUSE,
no bind mounts: the read side uses a private mount namespace for the transient
top-level-subvolume mount, and rustic_core's `as_path` records the original path.

The repository is plain restic — `restic`, `rustic`, and every restic backend
(local, sftp, REST, S3, B2, …) work against it directly.

See `DESIGN.md` for the architecture and the decisions behind it.

## Status

v1 milestones 0–4 implemented and tested end-to-end against official restic 0.18:

| command | what it does |
|---|---|
| `rbtrfs discover` | show detected btrfs filesystems, mounts, and subvolumes (read-only) |
| `rbtrfs backup [--profile P] [--dry-run]` | snapshot burst → per-subvol backup → merge → local-snapshot GC (`--dry-run` needs no root) |
| `rbtrfs snapshots [--all]` | list merged snapshots (`--all` also shows the per-subvol parts) |
| `rbtrfs restore <id\|latest> --subvol /home --target DIR [--as-subvolume] [--host H \| --any-host]` | restore one subvolume into a directory (or, with `--as-subvolume`, into a new btrfs subvolume); `latest` = newest merged snapshot from this host |
| `rbtrfs ls <id\|latest> [PATH]` | list a snapshot's contents |
| `rbtrfs dump <id\|latest> PATH` | write one file from a snapshot to stdout |
| `rbtrfs forget [--prune [--instant-delete]] [--dry-run]` | apply the profile's `[retention]` to the repository (needs root: takes the run lock) |
| `rbtrfs gc [--keep-local N] [--keep-local-days D] [--all-keys]` | delete local btrfs snapshots left by past or crashed runs |

Not yet: systemd units and a mountable (FUSE) restore view. Restores write plain
files; `--as-subvolume` makes the restore target a btrfs subvolume, but subvolumes
that were nested inside it come back as plain directories.

## Requirements

- Linux with btrfs, `libbtrfsutil` (ships with `btrfs-progs`)
- root (mount namespace, snapshot ioctls, subvolume enumeration)
- unprivileged user namespaces need not be enabled; rbtrfs uses a real root mount ns

## Configuration

Default path `/etc/rbtrfs/config.toml`, override with `--config` or `$RBTRFS_CONFIG`.

```toml
[profile.default]
repository       = "/mnt/backup/restic"     # or sftp:…, rest:…, s3:…
password_command = "pass show backup/restic" # or password / password_file
subvolumes       = ["/home", "/srv", "/var/log"]   # mount points: exact paths or globs
exclude          = ["**/.cache", "**/node_modules", "/home/alice/Downloads"]
tags             = ["rbtrfs"]
keep_local       = 1                          # local snapshot sets kept per subvolume
keep_local_days  = 3                          # ...and any set younger than this (optional)

[profile.default.retention]                   # used by `rbtrfs forget`
keep_last    = 3
keep_daily   = 7
keep_weekly  = 4
keep_monthly = 12
keep_within  = "14d"

[profile.default.hooks]
pre        = ["systemctl stop mydb"]
post       = ["systemctl start mydb"]
on_failure = "abort"                          # or "warn"
```

**Subvolumes** are selected by their mount point. Only whole-subvolume mounts are
snapshotted: a bind mount of a subdirectory is skipped with a warning, and a
subvolume mounted at several places is recorded once. btrfs snapshots are not
recursive, so a subvolume nested inside a selected one shows up as an empty
directory; rbtrfs warns about nested subvolumes (mounted or not) that you did not
select. Read-only nested subvolumes (snapper snapshots) are not warned about.

**Excludes** behave like restic's `--exclude`:

- `/home/alice/Downloads` — a path starting with `/` is matched as *recorded* in the
  backup, and applies to the subvolume it lives in. Glob syntax is fine after the
  mount point (`/home/*/tmp`), not before it.
- anything else matches at any depth: `*.tmp`, `.cache`, `alice/.cache`.
- a trailing `/` matches directories only (`node_modules/`).
- do not prefix patterns with `!`.

**`keep_local`** is how many local read-only snapshot sets to keep per subvolume
(handy for fast local rollback); with `keep_local_days`, sets younger than that
are kept too. Incremental backups do **not** depend on them: the previous run is
found through the repository.

**`[retention]`** thins the repository with `rbtrfs forget` (restic semantics:
`keep_last/hourly/daily/weekly/monthly/yearly`, `keep_within`), per host, over
the merged snapshots rbtrfs wrote. The per-subvolume *part* snapshots only exist
to make the next run incremental, so forget keeps just those of the newest run.
Snapshots with other labels (your own restic jobs) are never touched. Without a
`[retention]` table `forget` refuses to do anything. `--prune` also frees the data
nothing references any more; by default rustic marks it for deletion and removes
it on a later prune, `--instant-delete` removes it now (only if nothing else is
using the repository).

**Staging.** `staging` defaults to `"top-level"` (snapshots under
`<subvolid=5>/.rbtrfs-snapshots/`, reached by a transient mount inside a private
mount namespace). Use `staging = "in-subvolume"` where the top-level subvolume
can't be mounted: snapshots go to `<mountpoint>/.rbtrfs-snapshots/` and are
excluded from the backup automatically. Profiles that back up the same
subvolumes should use different `staging_name`s.

**Hooks run inside rbtrfs' private mount namespace.** They see the same mounts as
the host, but anything they mount is not visible outside, and vice versa.

The config file is read as root; if it holds an inline `password` (or you use a
`password_file`) keep it mode `0600` — rbtrfs warns otherwise.

## Consistency

btrfs has no atomic multi-subvolume snapshot ioctl. rbtrfs runs pre-hooks, then
issues every snapshot call back-to-back with no I/O in between (sub-millisecond
skew), then post-hooks. The backup then reads the **read-only snapshots**, never
the live subvolumes. Per-file integrity is guaranteed; exact cross-subvolume
ordering is not. Quiesce databases/VMs in `pre`/`post` hooks if you need more.

Post-hooks always run once the consistency window opened — even if a pre-hook
failed part-way or the snapshot failed — and SIGINT/SIGTERM/SIGHUP are held until
they have finished, so a stopped service is not left stopped.

## Concurrency and repository maintenance

rbtrfs takes a host-wide lock (`/run/rbtrfs/rbtrfs.lock`) so only one `backup`,
`gc` or `forget` runs at a time — which is also what makes `rbtrfs forget --prune`
safe against rbtrfs's own backups. It does **not** take restic's repository lock
(rustic_core 0.13 has no API to write one), so it cannot exclude a *different*
tool: do not run `restic forget --prune` / `rustic prune` while a backup is
running. Prefer `rbtrfs forget --prune`, or schedule other maintenance apart from
backups.

## Tests

```
cargo test
```

Unit tests need nothing. The end-to-end tests (`tests/e2e_loopback.rs`) build
throwaway loopback btrfs filesystems, so they need root. When you are not root,
each test re-runs itself inside its own privileged container (via
[testcontainers](https://crates.io/crates/testcontainers)): no sudo, parallel,
and nothing touches the host's mounts. This needs access to a Docker daemon
(your user in the `docker` group, or `DOCKER_HOST`). Without one the e2e tests
print a notice and skip; set `RBTRFS_E2E_REQUIRED=1` to make that a failure.
When run as root (`sudo -E cargo test`, or in CI) they run directly, serially,
and need `mkfs.btrfs`, `losetup`, `setfattr`/`getfattr` and `restic` on `PATH`.

They cover snapshot isolation, excludes, metadata fidelity (mode, owner, mtime,
symlinks, xattrs), incremental parents, local and repository retention, hook
failure and signal handling, SIGKILL mid-run (no leaked mounts, GC reclaims the
orphans), files staying individually intact while writers churn two subvolumes,
the run lock, in-subvolume staging, nested subvolumes, `ls`/`dump`/`--as-subvolume`,
and an official `restic check --read-data` of the repository rbtrfs wrote.

## Development spikes

`spikes/` holds the Milestone 0 proofs (`cargo run --example spike_*`), including
the cross-check that official `restic check --read-data` / `restic restore` accept
what rbtrfs writes.

## License

Licensed under either of Apache License 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or
MIT ([LICENSE-MIT](LICENSE-MIT)) at your option.
