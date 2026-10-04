# Config file format

rbtrfs is configured with one [TOML](https://toml.io) file. This page describes the
format. The individual settings are in [Configuration](configuration.md).

## Location

The default path is `/etc/rbtrfs/config.toml`. Use `--config FILE`, or set
`$RBTRFS_CONFIG`, to read a different file. The flag wins over the variable.

The file is read once when the command starts. `backup`, `gc` and `forget` run as
root, so a file you pass with `--config` has to be readable by root. If it holds a
password or other credentials, make it readable by root only. rbtrfs prints a
warning if anyone else can read it.

## Profiles

Everything lives under a profile. A profile is a table named `profile.NAME`, and a
file can hold as many as you like:

```toml
[profile.default]
repository    = "/mnt/backup/restic"
password_file = "/etc/rbtrfs/repo.pw"
subvolumes    = ["/", "/home"]
keep_local    = 2

[profile.offsite]
repository    = "rclone:mynas:backups/box"
password_file = "/etc/rbtrfs/repo.pw"
subvolumes    = ["/home"]
staging_name  = ".rbtrfs-offsite"
```

Commands use the profile called `default` unless you pass `--profile NAME`:

    sudo rbtrfs backup --profile offsite

Profiles are independent. Nothing is inherited from `default`. If two profiles back
up the same subvolumes, give them different `staging_name` values.

## Sub-tables

Some settings are grouped in a table of their own. Write the table name after the
profile name:

```toml
[profile.default.hooks]
pre  = ["systemctl stop mydb"]
post = ["systemctl start mydb"]

[profile.default.retention]
keep_last  = 3
keep_daily = 7

[profile.default.backend_options]
retry = "3"
```

The sub-tables are `hooks`, `retention`, `repository_mount`, `backend_options`,
`backend_options_hot` and `backend_options_cold`. A sub-table has to come after the
plain keys of its profile, because TOML puts every key that follows a `[table]`
header inside that table.

## Value types

| Type | Used for | Example |
|---|---|---|
| string | paths, commands, durations, option values | `"/mnt/backup"`, `"14d"` |
| number | counts and days | `keep_local = 2` |
| array of strings | lists | `subvolumes = ["/", "/home"]`, `extra_paths = ["/boot"]` |
| string or array | `subvolumes` only | `"all"` or `["/home"]` |
| table | grouped settings | `[profile.default.hooks]` |

Strings must be quoted, including numbers that are really option values:
`backend_options` values are always strings, so write `retry = "3"`. Lines that
start with `#` are comments.

## Errors

rbtrfs checks the whole file before it does anything, including for `--dry-run`, so
you can test a change safely:

    rbtrfs backup --dry-run

A key it does not recognise is an error at every level, which catches typos. The
message includes the line:

```
Error: parsing config c.toml
Caused by:
    TOML parse error at line 5, column 1
      |
    5 | keep_loca = 1
      | ^^^^^^^^^
```

Other checks are about values that do not make sense together:

| Problem | Message |
|---|---|
| more or fewer than one password source | `exactly one of password / password_file / password_command must be set (got 2)` |
| no subvolumes | `` `subvolumes` must not be empty `` |
| wrong type for a key | a TOML parse error pointing at the value |
| `--profile` names a profile that is not there | `no profile named [nope] in config` |
| the file cannot be read | `reading config /path` |

Errors from a profile name it, for example `profile [default]`.
