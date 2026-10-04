# Usage

rbtrfs backs up btrfs subvolumes into a restic repository. Everything is driven by a
config file with one or more named profiles.

## Configure

Write `/etc/rbtrfs/config.toml`. A minimal profile:

```toml
[profile.default]
repository    = "/mnt/backup/restic"
password_file = "/etc/rbtrfs/repo.pw"
subvolumes    = ["/", "/home", "/srv"]
exclude       = ["**/.cache", "*.tmp"]
```

`subvolumes` are mount points. Run `rbtrfs discover` to see what is mounted. Every
key is described in [Configuration](configuration.md). The file format is in
[Config file format](config-file.md). Use `--config FILE` or
`$RBTRFS_CONFIG` for a different file, and `--profile NAME` to pick another
profile.

NB: if the file holds a password, make it readable by root only. rbtrfs warns if
it is not.

## Back up

Check the plan first. This needs no root:

    rbtrfs backup --dry-run

Then run it as root. The first run creates the repository.

    sudo rbtrfs backup

Each run takes read-only snapshots of all the subvolumes back to back, backs them
up, and stores one restic snapshot with every subvolume at its real path. Old local
snapshots are deleted according to `keep_local`.

## List and restore

    rbtrfs snapshots
    rbtrfs ls latest /home
    rbtrfs dump latest /etc/fstab > fstab
    sudo rbtrfs restore latest --subvol /home --target /mnt/restore

`latest` is the newest backup from this host. To restore onto a different
machine, use `--host OLDNAME` or `--any-host`.

`restore` writes plain files. Run it as root to get the original owners back. Add
`--as-subvolume` to create the target as a new btrfs subvolume instead; the target
must be on btrfs and must not exist yet. Subvolumes that were nested inside the one
you restore come back as plain directories.

## Hooks

Hooks pause things that write to the subvolumes. They run before and after the
snapshots are taken, not during the backup, so a database is only paused for a
moment:

```toml
[profile.default.hooks]
pre  = ["systemctl stop mydb"]
post = ["systemctl start mydb"]
```

`post` hooks always run, even if a `pre` hook or the snapshot failed, and
SIGINT, SIGTERM and SIGHUP are held until they finish.

## Schedule

rbtrfs has no daemon. Run it from a systemd timer or cron. For example, a service
that backs up and then applies the retention policy:

```ini
# /etc/systemd/system/rbtrfs.service
[Unit]
Description=rbtrfs backup

[Service]
Type=oneshot
ExecStart=/usr/local/bin/rbtrfs backup
ExecStart=/usr/local/bin/rbtrfs forget --prune
```

```ini
# /etc/systemd/system/rbtrfs.timer
[Unit]
Description=Daily rbtrfs backup

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

To keep it out of the way of other programs, add `Nice=10`, `CPUWeight=20` and
`IOWeight=20` to the `[Service]` section. rbtrfs does not start a scope of its own
when it runs inside a service. See [Priority](configuration.md#priority).

Enable it with `systemctl enable --now rbtrfs.timer`. This example has not been
tested as shipped; adjust the paths to your install.

## Exit codes

- `0`: success
- `1`: the command failed
- `2`: bad command line

A backup that fails part-way leaves any local snapshots it already took. Run
`rbtrfs gc` to remove them.

## Environment variables

- `RBTRFS_CONFIG`: path of the config file, used when `--config` is not given.

For the test suite, see [CONTRIBUTING](https://github.com/spion06/restic-btrfs/blob/main/CONTRIBUTING.md).
