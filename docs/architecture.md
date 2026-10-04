# Architecture

How rbtrfs works and why. For usage see the [README](https://github.com/spion06/restic-btrfs/blob/main/README.md).

## Flow of a backup

```
select subvolumes -> mount the top-level subvolume (private mount namespace)
  -> pre hooks -> take all snapshots -> post hooks
  -> back up each snapshot, recording the real mount point (as_path)
  -> back up the extra paths, read live
  -> merge the results into one backup -> delete old local snapshots
```

## Key decisions

### rustic_core, in-process

rustic_core can read from one path and record another (`BackupOptions::as_path`).
The restic CLI cannot do this. It makes paths stable across runs (so
unchanged files are never re-read) without FUSE or bind mounts. The repository stays
plain restic; the test suite checks it with the official `restic check --read-data`.

### Private mount namespace

The top-level subvolume (`subvolid=5`), where snapshots are staged, is rarely
mounted. rbtrfs `unshare(CLONE_NEWNS)`s and makes `/` private, then mounts it there:
invisible to the host, and cleaned up by the kernel when the last process in the
namespace exits, even after a SIGKILL. `unshare` must run
first, while the process is single-threaded, because it applies per thread and
rustic_core starts a thread pool. A `staging = "in-subvolume"` fallback needs no
mount.

A mount namespace isolates the mount table, not the directories under it. Mounts made
by one rbtrfs process are invisible to every other process, as intended. But a mount
point is an ordinary directory on the shared `/run`, and removing a directory makes
Linux detach whatever is mounted on it in every namespace. We found this as a bug in
0.1.0: removing the mount-point directory on exit detached the repository from another
rbtrfs process that was still running. The directories are no longer removed, and a
test runs several processes against the same path to keep it that way. The repository does not
record where it is mounted (its config holds a version, an ID and a chunker value;
snapshots hold the host and the source paths), so the mount path could also differ per
run without affecting restic.

### One snapshot per subvolume, then merge

`as_path` accepts only one path per backup. So each subvolume is backed up on its own,
and `merge_snapshots` joins the results into one snapshot with every subvolume at its
real path.

The per-subvolume snapshots (the "parts", tagged `rbtrfs:part` and labelled
`rbtrfs-part:<key>`) stay in the repository, because the next run uses them as its
parent. Both kinds are stamped with the time the snapshots were taken.

The repository has to be re-opened between the per-subvolume backups and the merge.
Otherwise the in-memory index does not contain the trees that were just written, and
the merge fails.

### Consistency

btrfs cannot snapshot several subvolumes atomically, and freezing the filesystem
would deadlock the transaction that creates a snapshot. So rbtrfs takes all the
snapshots back to back, with every path and option prepared beforehand and no other
work in between. That is about 2 ms per subvolume. The hooks around it always run
and hold termination signals until they finish.

### btrfs behind a trait

The `BtrfsOps` trait, implemented with `libbtrfsutil`, keeps FFI out of the rest of
the code. It also lets the snapshot loop and the local cleanup be unit-tested
against a fake.

### Generic discovery

Mounts come from `/proc/self/mountinfo`, grouped per filesystem; nothing assumes an
`@`/`@home` naming scheme. Subvolume keys are derived from the mount point with `-`
and `%` escaped so they never collide.

### Snapshots outlive the process

Snapshots are on-disk subvolumes, not part of the mount namespace. When a run is
killed, the mount disappears but the snapshots stay, so `rbtrfs gc` is needed to
remove them.

## Code map

| module | role |
|---|---|
| `main`, `cli` | argument parsing, namespace decision, commands |
| `config`, `excludes` | TOML profiles; user excludes to rustic globs |
| `mountinfo`, `discover`, `select` | find btrfs mounts and match `subvolumes` |
| `btrfs` | `BtrfsOps` trait and the libbtrfsutil implementation |
| `ns`, `snapshot`, `gc` | namespace and mounts, staging, taking snapshots, local cleanup |
| `hooks`, `signals`, `lock` | hooks, signal handling, the run lock |
| `repo`, `backup`, `restore`, `forget` | repository access and the commands on it |

## Tests

Unit tests cover parsing, selection, excludes (against the real matcher), snapshot loop and local
cleanup against a fake backend, and the forget planner. `tests/e2e_loopback.rs`
builds real btrfs filesystems on loop devices; when not root each test re-runs in
its own privileged container (testcontainers). It covers snapshot isolation,
metadata fidelity, signals and SIGKILL, concurrent writers, retention and safety
against concurrent prunes. The `spikes/` examples are the original proofs of
concept.

## Known limitations

- No restic repository lock (see [Maintenance](maintenance.md)).
- Only local repositories and OpenDAL's `fs` service are tested; `rest:`, `rclone:` and
  the other OpenDAL services are accepted but untested.
- `extra_paths` are read live, not from a snapshot, so they are not consistent with
  the snapshotted subvolumes.
- No mountable (FUSE) restore view; nested subvolumes restore as plain directories.
- Merged snapshots chain via `parent` for listings; incremental detection uses the parts.
