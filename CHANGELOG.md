# Changelog

All notable changes are documented here. Entries from 0.1.1 on are generated from
commit messages when a release is made; see [CONTRIBUTING.md](CONTRIBUTING.md#releasing).

## 0.1.2 - 2026-10-04

### Added

- Add 'rbtrfs init' and an option to disable auto-creating the repository
- Restore root metadata and numeric ids, exit 3 for vanished files, forget per profile
- Optionally list directories left out by CACHEDIR.TAG and other markers

### Documentation

- Correct claims about signals, leftover snapshots, globs and extra paths

### Fixed

- Harden backup runs found in review

## 0.1.1 - 2026-10-04

### Added

- backup: Report what exclude patterns skip in the dry run
- backup: Skip directories marked with CACHEDIR.TAG; add exclude_if_present and exclude_if_xattr

### Documentation

- Describe hooks accurately and document the hourly timer and shutdown backup
- Correct statements that did not match what the code does
- Say which release had the mount-point removal bug
- Describe the mount-point bug plainly

### Fixed

- mount: Don't remove the shared mount-point directory on exit

## 0.1.0 - 2026-10-04

First release.

### Backing up
- `backup` takes read-only snapshots of all selected subvolumes back to back, then
  backs up from the snapshots into one restic snapshot with each subvolume at its
  real mount path. Unchanged files are not read again.
- Snapshots are staged under the top-level subvolume, mounted inside a private mount
  namespace, or with `staging = "in-subvolume"` inside each subvolume.
- `subvolumes` takes mount points, globs, or `"all"`; `exclude_subvolumes` removes
  some. Bind mounts and nested subvolumes are detected and reported.
- `extra_paths` adds directories from other filesystems, such as `/boot`, read live.
- `exclude` patterns work like restic's, including absolute paths as recorded.
- `pre` and `post` hooks always run around the snapshots and survive
  SIGINT, SIGTERM and SIGHUP.
- `backup`, `forget` and `gc` run at low priority by default: `nice`, `io_priority`,
  `cpu_weight` and `io_weight`, with cgroup weights applied through a transient
  systemd scope.
- `compression` sets the zstd level of the repository.

### Repository
- Plain restic repository, readable by `restic` and `rustic`.
- `repository` can be a local path, `rest:`, `rclone:` or `opendal:<service>`, with
  `backend_options`, `backend_options_hot`, `backend_options_cold` and
  `repository_hot` to configure the backend.
- `repository_mount` mounts an NFS, CIFS or other share privately for a run; a
  relative `repository` is taken below the mount.
- `forget [--prune]` applies a retention policy per host. `--instant-delete` asks
  for confirmation, or `--allow-unsafe` when not on a terminal.

### Reading and restoring
- `snapshots`, `ls`, `dump` and `restore`, with `latest` meaning the newest backup
  from this host (`--host`, `--any-host`).
- `restore --as-subvolume` restores into a new btrfs subvolume.

### Other
- `gc` removes local snapshots left by old or crashed runs; `keep_local` and
  `keep_local_days` control what a backup keeps.
- `discover` lists btrfs mounts and subvolumes.
- `man` and `completions <shell>` print the man page and shell completions.
- Config file format documented; unknown keys are errors at every level.
- Documentation site, command reference generated from the CLI, release tarballs for
  Linux x86_64 and aarch64.
