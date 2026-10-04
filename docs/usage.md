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

Check the plan first. This needs no root, though root sees every directory:

    rbtrfs backup --dry-run

It prints the subvolumes it would snapshot and, for each, how many files and bytes
would be stored and which paths your `exclude` patterns skip
([details](filtering.md#checking-what-your-patterns-do)). Add `--no-scan` for just
the plan.

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

Hooks are commands that run just before and just after the snapshots are taken. Use
them for anything that needs to happen around the snapshotting.

```toml
[profile.default.hooks]
pre  = ["/usr/local/bin/before-snapshot"]
post = ["/usr/local/bin/after-snapshot"]
```

The backup itself runs after the `post` hooks, from the snapshots.

`post` hooks always run, even if a `pre` hook or the snapshot failed, and
SIGINT, SIGTERM and SIGHUP are held until they finish.

## Schedule

rbtrfs has no daemon. Run it from a systemd timer or cron. This service backs up and
then applies the retention policy:

```ini
# /etc/systemd/system/rbtrfs.service
[Unit]
Description=rbtrfs backup
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/rbtrfs backup
ExecStart=/usr/local/bin/rbtrfs forget --prune
# keep it out of the way of other programs (see Priority in the configuration page)
Nice=10
CPUWeight=20
IOWeight=20
```

```ini
# /etc/systemd/system/rbtrfs.timer
[Unit]
Description=Hourly rbtrfs backup

[Timer]
OnCalendar=*-*-* *:15:00
RandomizedDelaySec=5min
# run at the next boot if a scheduled time was missed while the machine was off
Persistent=true

[Install]
WantedBy=timers.target
```

Enable it with `systemctl enable --now rbtrfs.timer`. An incremental run of an ordinary
desktop (700,000 files) takes about 40 seconds and adds a few megabytes to the
repository.

rbtrfs does not start a scope of its own when it runs inside a service, so set
`CPUWeight=` and `IOWeight=` on the unit. See
[Priority](configuration.md#priority).

### Backing up at shutdown

A unit whose stop action is the backup runs it when the machine shuts down. systemd
stops units in the reverse of their start order, so listing the network and the
mounts as dependencies keeps them up until the backup is done:

```ini
# /etc/systemd/system/rbtrfs-shutdown.service
[Unit]
Description=rbtrfs backup when shutting down
After=network-online.target NetworkManager.service
Wants=network-online.target
RequiresMountsFor=/boot /home /root /srv /var/log
# stop the hourly service first, then take the final backup
Before=rbtrfs.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/bin/true
# "-": a failed final backup must not make the shutdown fail
ExecStop=-/usr/local/bin/rbtrfs backup
TimeoutStopSec=5min

[Install]
WantedBy=multi-user.target
```

Enable it with `systemctl enable --now rbtrfs-shutdown.service`. It does nothing at
boot. Adjust the network service (`NetworkManager.service` here, or
`systemd-networkd.service`) and the mounts to your system. The stop action was tested
by stopping the unit by hand, which runs exactly that command; the ordering at a real
shutdown has not been tested. A shutdown does not wait longer than `TimeoutStopSec`.

## Exit codes

- `0`: success
- `1`: the command failed
- `2`: bad command line

A backup that fails part-way leaves any local snapshots it already took. Run
`rbtrfs gc` to remove them.

## Environment variables

- `RBTRFS_CONFIG`: path of the config file, used when `--config` is not given.

For the test suite, see [CONTRIBUTING](https://github.com/spion06/restic-btrfs/blob/main/CONTRIBUTING.md).
