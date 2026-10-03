# btrfs-restic backup tool — design

## Context

This document records the design decisions behind `rbtrfs`; it started as the planning document for
a greenfield project and has been kept in sync with the implementation (see the milestone notes and
"Open items" at the end). The goal is a general-purpose CLI
that takes atomic-as-possible btrfs snapshots of a set of subvolumes and backs them up into a
restic-format repository, with three hard requirements from the user:

1. **One consistent filesystem view per run** — all selected subvolumes snapshotted together,
   backed up as one logical unit, not as unrelated per-subvolume jobs.
2. **restic as the on-disk storage format** — not a bespoke format.
3. **Snapshots should not need to be mounted** — the user floated a FUSE/VFS layer to achieve this.
4. **Usable by other people**, not hardcoded to this machine's layout.

The interesting design problem is (3) combined with (1). The naive approach — back up
`/mnt/snapshots/@home/<ts>` — works, but restic records *the path it was given*, so the repository
ends up full of timestamped staging paths instead of `/home`. That wrecks restore ergonomics and,
if the path changes per run, defeats parent-snapshot change detection so every file is re-read
every run.

Research during planning found a solution that needs neither FUSE nor bind mounts. Decisions below
are recorded so the implementation has a fixed target.

---

## Decision 1: Rust + `rustic_core` as an embedded library

**Chosen.** `rustic_core` 0.13.0 (2026-08-16, MSRV 1.91; local toolchain is 1.98) is the engine
behind `rustic-rs` and writes restic-compatible repositories. It is used as a library, in-process —
not by shelling out to the `restic` binary.

The decisive reason is `BackupOptions::as_path: Option<PathBuf>` — *"Manually set backup path in
snapshot"*. This lets us read from the staging path while recording the **original mountpoint** in
the snapshot tree. The official `restic` CLI has no equivalent, which is why the alternative designs
all required bind mounts or FUSE. With `as_path` those disappear entirely.

Verified API surface (docs.rs/rustic_core/latest):

```rust
Repository::backup(&self, opts: &BackupOptions, source: &PathList, snap: SnapshotFile)
    -> RusticResult<SnapshotFile>
Repository::merge_snapshots(&self, snaps: &[SnapshotFile],
    cmp: &impl Fn(&Node, &Node) -> Ordering, snap: SnapshotFile) -> RusticResult<SnapshotFile>
Repository::restore(...), ::ls(...), ::dump(...)
BackupOptions { as_path, parent_opts, excludes, ignore_filter_opts,
                ignore_save_opts, no_scan, dry_run, stdin_* }   // #[non_exhaustive], builder-style
```

**Known constraint:** `as_path` is documented as restricted to a *single* source path per backup
call. This directly shapes Decision 3.

**Accepted risk:** rustic writes a restic-*compatible* repo, not one produced by restic itself.
Mitigated by a mandatory compatibility spike (see Milestone 0).

## Decision 2: snapshots staged under the top-level subvolume

Read-only snapshots are created under a staging subvolume at the filesystem root (subvolid 5),
default name `.rbtrfs-snapshots`, laid out as `<staging>/<subvol-key>/<run-id>`. This keeps source
subvolumes clean — no `.snapshots` directory inside `/home` needing an exclude rule, and no nested
subvolume showing up as a stray empty directory in future snapshots.

The top-level subvolume is generally not mounted (it is not on this machine — `/` is `subvol=/@`),
so reaching the staging area requires a mount. To honour requirement (3) as closely as physically
possible, that mount happens **inside a private mount namespace**:

- At the very top of `main()`, before any thread or async runtime starts, call
  `unshare(CLONE_NEWNS)` then `mount(None, "/", MS_REC | MS_PRIVATE, None)`.
- Mount `subvolid=5` (or `subvol=<staging>`) at a temp dir inside that namespace.

The mount is invisible to the rest of the system, cannot leak into the host mount table, and is
torn down by the kernel when the process exits — including on crash or SIGKILL. Functionally this
delivers what the FUSE idea was reaching for, with none of the overhead or code.

`unshare` must happen while the process is single-threaded; on Linux `CLONE_NEWNS` applies per-thread
and `rustic_core` uses a thread pool. Do it as the first statement in `main`, before anything else.

A `staging = "in-subvolume"` fallback (snapper-style `/home/.rbtrfs-snapshots/...`) is
supported for hosts where the top-level subvolume cannot be mounted; it needs no mount at all but
requires auto-generated excludes (implemented: the staging dir is excluded from each part).

## Decision 3: N per-subvolume backups, merged into one snapshot

Because `as_path` accepts only one source path, the run is:

```
for each selected subvolume S:
    repo.backup(
        BackupOptions::default()
            .as_path(S.original_mountpoint)          //  /home
            .parent_opts(explicit parent = previous run's part-snapshot for S),
        PathList::from(staging/<S.key>/<run-id>),     //  read from here
        SnapshotFile { tags: [rbtrfs:part, rbtrfs:run=<run-id>, rbtrfs:subvol=<key>], .. })

repo.merge_snapshots(&parts, &newest_wins, SnapshotFile { tags: config.tags, .. })
```

The merged snapshot is the user-facing artifact: a single restic snapshot whose tree contains
`home/`, `srv/`, `var/log/` … at their real paths, all captured at one instant. Paths are stable
across runs, so parent-based change detection works and unchanged files are never re-read.

Part snapshots are **retained in the repository**, not deleted — they are the parent references that
make the next run incremental (rustic groups by host + label + paths), and they cost metadata only
(data blobs are shared). They are tagged `rbtrfs:part` and filtered out of the default `snapshots`
listing and out of `restore latest`. They are never pruned by rbtrfs: use `restic forget`/`rustic`.
`keep_local` is a different thing: it is how many *local btrfs snapshot sets* are kept per subvolume
for local rollback; incremental backups do not depend on them.

Part and merged snapshots are all stamped with the instant of the snapshot burst (the point in time
the data represents), not with the time the backup finished.

**Fallback if `merge_snapshots` proves unsuitable** (Milestone 0 decides): ship the part snapshots
as the final product, grouped by a shared `rbtrfs:run=<id>` tag, and teach the CLI to present a run
as a unit. Same consistency guarantee, less elegant listing.

## Decision 4: consistency is a tight ioctl loop plus hooks

btrfs has **no atomic multi-subvolume snapshot ioctl**, and `FIFREEZE` cannot help — snapshot
creation needs a transaction commit, which a frozen filesystem would deadlock. Perfect atomicity is
not achievable and the docs will say so plainly.

What the tool does instead:

1. Run configured **pre-hooks** (quiesce databases, flush application state).
2. Issue every `SNAP_CREATE_V2` back-to-back with **zero I/O in between** — no logging, no stat
   calls, no allocation in the loop. All paths and file descriptors are resolved beforehand. The
   skew window is sub-millisecond per subvolume.
3. Run **post-hooks** (thaw).
4. Back up at leisure from the now-frozen read-only snapshots.

Hook failure policy is configurable: `abort` (default) or `warn`. Post-hooks always run once the window
opened (even after a failed pre-hook or burst), and termination signals are deferred until they have.

## Decision 5: btrfs bindings

Use the `libbtrfsutil` crate (safe bindings over `libbtrfsutil.so`, shipped with btrfs-progs — v7.1
is installed here) for: `create_snapshot`, `delete_subvolume`, `subvolume_info`, `SubvolumeIterator`,
`is_subvolume`, read-only flag handling. The C ABI is stable even though the crate has not been
republished recently.

All of it sits behind a small internal `trait BtrfsOps` (`is_subvolume`, `list_subvolumes`,
`snapshot_readonly`, `delete_subvolume`) so the backend can be swapped for raw ioctls or `btrfs(8)`
subprocess calls without touching the rest of the tool, and so the snapshot burst and GC can be unit
tested against a fake. Do not scatter FFI through the codebase.

## Decision 6: generic discovery, nothing hardcoded

No knowledge of `@`/`@home` naming anywhere. At runtime:

- Parse `/proc/self/mountinfo` → every btrfs mount, its mountpoint, `subvolid=`, `subvol=`, device.
- Group mounts by filesystem UUID (`BTRFS_IOC_FS_INFO`) — multiple btrfs filesystems in one run are
  supported, each with its own staging area and its own snapshot burst.
- Enumerate subvolumes per filesystem and join to mountpoints via subvolid.
- Config selects by mountpoint (explicit list or glob), with excludes. Nested subvolumes that are
  not mounted anywhere can be opted into.

Because btrfs snapshots are **not recursive**, every subvolume to be captured must be snapshotted
explicitly; a nested subvolume appears as an empty directory in its parent's snapshot. The tool
warns when a mounted subvolume under a selected tree is not itself selected — this is the single
most common way to silently back up nothing.

---

## Layout

```
restic-btrfs/
  src/
    main.rs              # arg parsing, then unshare() before anything spawns a thread
    cli.rs               # clap commands; `discover --json` via serde_json
    config.rs            # TOML profiles, password sources, permission warning
    ns.rs                # private mount namespace, TransientMount (subvolid=5)
    mountinfo.rs         # /proc/self/mountinfo parser
    discover.rs          # group mounts by filesystem; whole-subvolume vs bind mounts
    select.rs            # patterns -> subvolumes; keys; bind-mount/duplicate handling
    btrfs/{mod,util}.rs  # trait BtrfsOps + libbtrfsutil implementation
    snapshot.rs          # StagingArea, snapshot burst, local GC
    gc.rs                # standalone `gc`
    excludes.rs          # user excludes -> rustic_core override globs
    hooks.rs             # pre/post hooks and the consistency window
    signals.rs           # defer SIGINT/SIGTERM/SIGHUP during the window
    lock.rs              # host-wide run lock (flock in /run/rbtrfs)
    repo.rs              # open / init a rustic_core repository
    backup.rs            # the backup run
    restore.rs           # `snapshots`, `ls`, `dump`, `restore`
    forget.rs            # repository retention: merged snapshots by policy, stale parts
    runid.rs             # UTC run ids, no date crate
  spikes/                # Milestone 0 proofs (cargo run --example spike_*)
  tests/e2e_loopback.rs  # loopback-btrfs end-to-end tests (containerised when not root)
```

## CLI

```
rbtrfs backup   [--profile P] [--dry-run]       # --dry-run needs no root
rbtrfs snapshots [--all]                        # merged only by default; --all shows parts
rbtrfs restore  <id|latest> --subvol /home --target DIR [--as-subvolume] [--host H | --any-host]
rbtrfs ls       <id|latest> [path]
rbtrfs dump     <id|latest> <path>                # one file to stdout
rbtrfs forget   [--prune [--instant-delete]] [--dry-run]   # repository retention (root, run lock)
rbtrfs gc       [--keep-local N] [--keep-local-days D] [--all-keys]   # local snapshots, incl. crashed-run orphans
rbtrfs discover [--json]                        # detected filesystems/subvolumes; read-only
```

Config is TOML with named profiles: repository + credentials, subvolume selection, staging mode and
name, excludes, tags, hooks, local-snapshot retention. Retention/prune of the *restic repo* and
systemd units are explicitly out of scope for v1.

## Milestone 0 — spike results (DONE, 2026-09-02)

All spikes pass. Code in `spikes/`, wired as `cargo run --example spike_*`.
Locked versions: `rustic_core` 0.13.0, `rustic_backend` 0.7.0, `libbtrfsutil` 0.8.0, `nix` 0.31.
Cross-checked against official `restic` 0.18.0.

- **`spike_aspath_merge`** — `BackupOptions::as_path` remaps the recorded path
  (`staging/home` read, `/home` recorded); `Repository::merge_snapshots` produces one snapshot whose
  tree is `home/user/hello.txt` + `srv/data.bin` at real paths. **Gotcha, now a hard rule:** the
  repo must be **re-opened** (`open` → `to_indexed`) between the part `backup()` calls and
  `merge_snapshots` — the in-memory index from `to_indexed_ids()` does not contain trees written
  earlier in the same process, and merge fails with "Tree ID not found in index".
- **`spike_restic_compat`** — a password repo written by rustic_core (parts + merged): official
  `restic snapshots` lists all three, `restic check --read-data` reports "no errors were found",
  `restic restore <merged>` restores `/home` + `/srv` with correct content. **Decision 1's accepted
  risk is cleared.** Minor: rustic_core does not pre-create `locks/`; `restic` needs it to exist
  (any restic write, or `mkdir locks`, fixes it) — the tool should create it on init.
- **`spike_mount_ns`** — `unshare(CLONE_NEWNS)` on the single-threaded main thread + `MS_REC |
  MS_PRIVATE` on `/`; a tmpfs mounted afterwards is invisible to a witness process left in the
  original namespace, and **all four `std::thread` workers spawned after the unshare are in the new
  namespace and see the mount** — confirms the rustic_core thread-pool concern is handled by
  "unshare first, before anything else".
- **`spike_full_spine`** (root) — the entire pipeline on the live filesystem: private ns → mount
  `subvolid=5` → tight read-only snapshot loop over `/var/log` + `/var/tmp` → per-subvol
  `backup(as_path=mountpoint)` → `merge_snapshots` → official `restic check --read-data` clean →
  official `restic restore` of the merged snapshot yields `var/log` + `var/tmp` (25k files) intact.
  Snapshots deleted afterwards; host `/proc/1/mounts` byte-identical before/after.

  **New finding, feeds Decision 2 and the `gc` command:** btrfs snapshots are **on-disk
  subvolumes, not namespace-scoped**. When the process dies the private *mount* vanishes but the
  snapshots under the staging subvolume **persist** (invisible only because `subvolid=5` is no
  longer mounted). Orphan cleanup in `rbtrfs gc` is therefore mandatory, not optional — a killed
  run leaks real subvolumes. Recovery: mount `subvolid=5`, `btrfs subvolume delete` the leftovers.

## Milestones

**1 — Discovery. DONE.** `src/mountinfo.rs` + `src/discover.rs`; `rbtrfs discover` (read-only, JSON
optional). Groups mounts by the mountinfo `major:minor` (stable per btrfs fs on current kernels),
enumerates subvolumes via `libbtrfsutil` when root, flags mounted-but-unselected nested subvolumes.
No `@`-scheme assumptions. **Deviation:** mountinfo super-options escape the `subvol=` value too
(`/@we\040ird`); `btrfs_subvol()` unescapes it.

**2 — Snapshot burst + GC. DONE.** `src/snapshot.rs` (+ `src/ns.rs`, `src/runid.rs`). Staging under
`subvolid=5` via a `TransientMount` inside the private ns; `burst()` pre-creates every parent dir
and pre-builds `CreateSnapshotOptions`, then the final loop issues only ioctls. `runid` = UTC
`YYYYMMDDThhmmssZ`, no date crate. `gc()` deletes run-id dirs beyond `keep_local` per key and
ignores foreign subvolumes; `rbtrfs gc` re-mounts `subvolid=5` to reach orphans from crashed runs.

**3 — Backup. DONE.** `src/backup.rs` + `src/repo.rs` + `src/config.rs` + `src/hooks.rs` +
`src/select.rs`. Per-subvol `backup(as_path=mountpoint)`, then `merge_snapshots`. Parent lineage is
kept per subvolume via `label = "rbtrfs-part:<key>"` so rustic's default host+label+paths grouping
chains each run to the last (verified: run N parts' parent = run N-1 parts). Parts tagged
`rbtrfs:part` + `rbtrfs:run=<id>`; merged tagged from `profile.tags`. `ParentOptions.ignore_inode`
set (snapshot device changes each run). `RepoHandle` auto-inits the repo and creates `locks/`.
Config is TOML profiles with `password` / `password_file` / `password_command`.

**4 — Restore + snapshots list. DONE.** `src/restore.rs`. `snapshots` hides `rbtrfs:part` unless
`--all`. `restore <id|latest> --subvol /p --target DIR` resolves the tree node at `p` (leading `/`
stripped) and streams it to a `LocalDestination`. Verified byte-for-byte in the loopback test.
`ls`/`dump` and a mountable (FUSE) restore view are still out of scope — add later with `fuse3` or
by delegating to the `rustic`/`restic` CLI.

## Verification

All of this runs under plain `cargo test`. The end-to-end tests need root; when the test process is
not root each test re-runs itself in its own privileged container (testcontainers, an Arch image with
btrfs-progs and restic, the test binary bind-mounted in), so no sudo is needed and tests run in
parallel. As root (CI) they run directly and serially.

- **Round trip** — three awkwardly-named subvolumes, two runs, restore each by `latest`; mode, uid/gid,
  mtime, symlinks and xattrs are compared, and the host mount table is asserted unchanged.
- **Snapshot isolation** — a post-hook rewrites the live data right after the burst; the backup must
  contain the pre-burst contents (this is the regression test for reading the live subvolume).
- **Excludes**, **incremental** (second part chains to the first and adds 0 bytes), **nested
  subvolumes**, **local GC**, **in-subvolume staging**, **hook failure runs post-hooks**, **run lock**,
  **wrong password does not re-init**, **`latest` ignores other hosts' snapshots**.
- **Cross-tool** — official `restic check --read-data` against the repository rbtrfs wrote.
- Unit tests (no root) cover mountinfo/discovery, selection, key escaping, exclude translation against
  the real matcher, and the burst and GC against a fake `BtrfsOps`.

- **Consistency probe** — writers churn files (atomic rename) across two subvolumes while three
  backups run; every restored file is individually intact. Cross-subvolume generation is deliberately
  not asserted: the snapshots are microseconds apart, not atomic. (Checked to fail with in-place
  writes, which can be torn.)
- **Signals / crashes** — SIGTERM during a pre-hook and during a post-hook still runs every post-hook;
  `SIGKILL` mid-run leaves the host mount table byte-identical, the snapshots survive on disk, the run
  lock dies with the process, and `gc` reclaims them.
- **Repository retention, `ls`, `dump`, `--as-subvolume`**, and merged-snapshot lineage.

## Open items

Resolved in implementation:
- Part snapshots are kept in the same repo, tagged `rbtrfs:part`, hidden from `snapshots` unless
  `--all`.
- Multi-filesystem runs produce **one** merged snapshot spanning all filesystems; the snapshot
  bursts are still per-filesystem but the ioctls across filesystems still go back-to-back.
- Unmounted nested subvolumes are not auto-selected; mounted-but-unselected nested subvolumes get a
  warning.

Resolved since v0:
- Age-based local retention: `keep_local_days` keeps any set younger than N days in addition to the
  newest `keep_local`.
- Repository retention: `rbtrfs forget [--prune]` applies a `[retention]` policy to merged snapshots
  (per host) and drops part snapshots older than the newest run; foreign snapshots are never touched.
  It runs under the same host-wide lock as `backup`.
- `ls` and `dump`.
- `restore --as-subvolume` creates the target as a btrfs subvolume (nested subvolumes come back as
  plain directories).
- Merged snapshots now carry `parent` = the previous merged snapshot of the host, so listings show
  lineage. (Parts still drive incremental detection.)

Still open:
- No restic repository lock: rustic_core 0.13 has no API to write one (`FileType` has no lock type), so
  rbtrfs cannot exclude a *different* tool's prune. Its own backups/gc/forget are serialised by the
  host-wide `flock`.
- A FUSE/mountable restore view.
- `forget --prune --instant-delete` is only safe if nothing else uses the repository; the default
  (rustic's delayed deletion) is safe but frees space on a later prune.
