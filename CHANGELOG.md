# Changelog

All notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.0] - 2026-10-04

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

[Unreleased]: https://github.com/spion06/restic-btrfs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/spion06/restic-btrfs/releases/tag/v0.1.0
