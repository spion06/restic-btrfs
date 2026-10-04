# Command reference

Back up btrfs subvolumes together into a restic repository.

Every command also accepts `-c, --config <CONFIG>`.

- [`rbtrfs discover`](rbtrfs_discover.md): Show btrfs filesystems, mounts and subvolumes.
- [`rbtrfs backup`](rbtrfs_backup.md): Snapshot the selected subvolumes and back them up.
- [`rbtrfs snapshots`](rbtrfs_snapshots.md): List the backups in the repository.
- [`rbtrfs restore`](rbtrfs_restore.md): Restore one subvolume from a backup.
- [`rbtrfs ls`](rbtrfs_ls.md): List the contents of a backup.
- [`rbtrfs dump`](rbtrfs_dump.md): Write one file from a backup to standard output.
- [`rbtrfs forget`](rbtrfs_forget.md): Apply the retention policy to the repository.
- [`rbtrfs gc`](rbtrfs_gc.md): Delete local snapshots left behind by old or crashed runs.
- [`rbtrfs completions`](rbtrfs_completions.md): Print a shell completion script.
- [`rbtrfs man`](rbtrfs_man.md): Print the man page.
