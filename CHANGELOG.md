# Changelog

All notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- `backup`: one consistent burst of read-only snapshots across subvolumes, merged
  into a single restic snapshot with each subvolume at its real mount path.
- `restore` (optionally `--as-subvolume`), `ls`, `dump`, `snapshots`, `discover`.
- `forget [--prune]` repository retention, and `gc` with `keep_local` /
  `keep_local_days`.
- Hooks that always run after the snapshot and survive SIGINT/SIGTERM.
- `backend_options`, `backend_options_hot`, `backend_options_cold` and `repository_hot` to configure the storage backend (rclone, OpenDAL, REST).
- `repository_mount` to mount an NFS/CIFS/etc. repository privately for a run.
- `staging = "in-subvolume"` for hosts that cannot mount the top-level subvolume.
- `man` and `completions <shell>` commands.
- Release workflow: tagged builds for x86_64 and aarch64 Linux with man page and completions.
- End-to-end test suite on loopback btrfs, run in containers when not root.
