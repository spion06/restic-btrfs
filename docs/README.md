# rbtrfs documentation

rbtrfs backs up btrfs subvolumes together into a restic repository. It takes
read-only snapshots of all the subvolumes you choose back to back, then backs up from
those snapshots, so the result is one consistent restic snapshot with each subvolume
at its real path.

New here? Start with [Install](install.md) and [Usage](usage.md).

- [Install](install.md)
- [Usage](usage.md): configuring, running, scheduling, exit codes
- [Config file format](config-file.md): profiles, sub-tables, types, errors
- [Configuration](configuration.md): every config key
- [Filtering](filtering.md): excluding files
- [Repository](repository.md): where the repository lives, NFS and other shares
- [Maintenance and safety](maintenance.md): local snapshots, retention, concurrency
- [Command reference](commands/index.md)
- [Architecture](architecture.md): how it works and why
