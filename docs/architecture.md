# Architecture

How rbtrfs works and why. For usage see the [README](../README.md).

## Flow of a backup

```
select subvolumes -> stage (top-level subvolume, private mount ns)
  -> pre-hooks -> snapshot burst -> post-hooks
  -> per-subvolume backup of the read-only snapshot (as_path = real mount point)
  -> merge parts into one snapshot -> delete old local snapshots
```

## Key decisions

**rustic_core, in-process.** `BackupOptions::as_path` reads from the staging
snapshot but records the original mount point, which the restic CLI cannot do. It
makes paths stable across runs (so unchanged files are never re-read) without FUSE
or bind mounts. The repository stays plain restic; the test suite checks it with
the official `restic check --read-data`.

**Private mount namespace.** The top-level subvolume (`subvolid=5`), where
snapshots are staged, is rarely mounted. rbtrfs `unshare(CLONE_NEWNS)`s and makes
`/` private, then mounts it there: invisible to the host, cleaned up by the kernel
even on SIGKILL. `unshare` must run first, while the process is single-threaded,
because it applies per thread and rustic_core starts a thread pool. A
`staging = "in-subvolume"` fallback needs no mount.

**One snapshot per subvolume, then merge.** `as_path` takes a single path per
backup, so each subvolume is backed up separately and `merge_snapshots` joins them
into one snapshot with each subvolume at its real path. The parts stay in the
repository (tagged `rbtrfs:part`, labelled `rbtrfs-part:<key>`) because they are
what the next run uses as its parent. Merged and part snapshots are stamped with
the burst time. The repository must be **re-opened** between the part backups and
the merge, otherwise the in-memory index misses the newly written trees.

**Consistency.** btrfs has no atomic multi-subvolume snapshot (and `FIFREEZE`
would deadlock the transaction). rbtrfs does all the snapshots back-to-back with
every path and option prepared beforehand (about 2 ms per subvolume), wrapped in
hooks that always run and defer termination signals.

**btrfs behind a trait.** `BtrfsOps` (`libbtrfsutil` today) keeps FFI out of the
rest of the code and lets the burst and GC be unit-tested against a fake.

**Generic discovery.** Mounts come from `/proc/self/mountinfo`, grouped per
filesystem; nothing assumes an `@`/`@home` naming scheme. Subvolume keys are
derived from the mount point with `-` and `%` escaped so they never collide.

**Snapshots outlive the process.** Snapshots are on-disk subvolumes, not
namespace-scoped: a killed run leaks them (the mount vanishes, they do not). `gc`
is therefore required, not optional.

## Code map

| module | role |
|---|---|
| `main`, `cli` | argument parsing, namespace decision, commands |
| `config`, `excludes` | TOML profiles; user excludes to rustic globs |
| `mountinfo`, `discover`, `select` | find btrfs mounts and match `subvolumes` |
| `btrfs` | `BtrfsOps` trait and the libbtrfsutil implementation |
| `ns`, `snapshot`, `gc` | namespace and mounts, staging, burst, local GC |
| `hooks`, `signals`, `lock` | consistency window, signal deferral, run lock |
| `repo`, `backup`, `restore`, `forget` | repository access and the commands on it |

## Tests

Unit tests cover parsing, selection, excludes (against the real matcher), the
burst/GC against a fake backend and the forget planner. `tests/e2e_loopback.rs`
builds real btrfs filesystems on loop devices; when not root each test re-runs in
its own privileged container (testcontainers). It covers snapshot isolation,
metadata fidelity, signals and SIGKILL, concurrent writers, retention and safety
against concurrent prunes. The `spikes/` examples are the original proofs of
concept.

## Known limitations

- No restic repository lock (see [Maintenance](maintenance.md)).
- Only local repositories are tested; `opendal` options cannot be configured.
- No mountable (FUSE) restore view; nested subvolumes restore as plain directories.
- Merged snapshots chain via `parent` for listings; incremental detection uses the parts.
