# Repository

`repository` in the profile says where the restic repository lives.

| Value | Backend |
|---|---|
| `/path/to/repo` | local directory |
| `rest:https://host/path` | restic REST server |
| `rclone:remote:path` | any rclone remote (needs `rclone` installed) |
| `opendal:<service>` | an OpenDAL service such as `s3`, `b2`, `sftp`, `gcs` or `azblob` |

Only local paths, and the OpenDAL `fs` service, are covered by the tests. The others
are accepted but untested. restic's own `sftp:` and `s3:` URLs are not recognised.
Use `opendal:sftp` or `opendal:s3`, or an rclone remote.

`backup` creates the repository on the first run. To create it yourself, run
`rbtrfs init`, which fails if one already exists. Set `auto_init = false` (or pass
`backup --no-init`) to make `backup` fail on a missing repository instead, which
catches a wrong path or an unmounted share before it turns into a new, empty
repository.

The password comes from exactly one of `password`, `password_file` or
`password_command`.

## Backend options

Most backends need more than a path. `backend_options` is a table of settings that
rbtrfs hands to the backend unchanged:

```toml
[profile.default]
repository = "opendal:s3"

[profile.default.backend_options]
bucket            = "my-backups"
region            = "eu-west-1"
root              = "/restic/mybox"
access_key_id     = "AKIA..."
secret_access_key = "..."
```

For `opendal:` the keys are those of the OpenDAL service. See the
[OpenDAL service docs](https://docs.rs/opendal/latest/opendal/services/) for the
list. All OpenDAL services also take `retry` and `connections`.

For `rclone:` the useful key is `rclone-command`, which replaces the command
rbtrfs starts (`rclone serve restic --addr localhost:0`). Use it to pass a config
file:

```toml
[profile.default]
repository = "rclone:mynas:backups/mybox"

[profile.default.backend_options]
rclone-command = "rclone serve restic --addr localhost:0 --config /etc/rclone/rclone.conf"
```

For `rest:` the keys are `retry` and `timeout`. A local path takes none.

The option names belong to rustic, not rbtrfs, and they can change when rustic is
updated. If a key is wrong the backend reports it.

NB: this table often holds credentials. Keep the config file readable by root only.
rbtrfs warns if it contains a key that looks like a credential (`key`, `secret`,
`token`, `pass`) and the file is readable by others.

### rclone

rbtrfs does not read or copy your rclone config. rustic starts `rclone` as a child
process, and rclone looks for its config the usual way: `--config`, then
`$RCLONE_CONFIG`, then `~/.config/rclone/rclone.conf`. `backup`, `gc` and `forget`
run as root, so that last path is root's home, not yours. Either put the config
there, set `RCLONE_CONFIG` for the process (for systemd,
`Environment=RCLONE_CONFIG=/etc/rclone/rclone.conf`), or pass `--config` with
`rclone-command` as above. `sudo` clears the environment by default, so with
`sudo` write `sudo RCLONE_CONFIG=/etc/rclone/rclone.conf rbtrfs backup`.

`rclone` must be on root's `PATH`. Unless you set `rclone-command`, rustic also checks
that it is version 1.52.2 or newer.

rclone runs inside the same private mount namespace as rbtrfs.

### Hot and cold repositories

rustic can split a repository into a hot part (metadata, snapshots) and a cold part
(data), for example on cheap archive storage. Set `repository_hot` to the hot
location. `backend_options_hot` and `backend_options_cold` apply to one part, and
`backend_options` to both. `backend_options_hot` needs `repository_hot`. This is
passed to rustic as is and has not been tested with rbtrfs.

## A share that is not mounted on the host

If the repository is on NFS, CIFS or similar and the share is not mounted on the
host, rbtrfs can mount it for the run:

```toml
[profile.default]
repository = "restic/mybox"                     # relative to the mount target

[profile.default.repository_mount]
type    = "nfs"
source  = "nas.local:/export/backups"
options = "vers=4.2"                            # optional
target  = "/run/rbtrfs/repo"                    # optional, this is the default
```

rbtrfs runs `mount -t <type> [-o <options>] <source> <target>` before it opens the
repository, so `mount.nfs` and the other helpers resolve hostnames and options as
usual. `type` can be anything `mount(8)` accepts.

`repository` is relative to `target`, so `"restic/mybox"` means
`/run/rbtrfs/repo/restic/mybox`. Use `"."` for the root of the share. You can also
write the full path, but it has to be below `target`, and a relative path cannot
contain `..`. The directory is created on the first backup if it does not exist.

The mount is private to rbtrfs: it never appears in the host's mount table. It goes
away when the last process in rbtrfs' mount namespace exits (rbtrfs and anything it
started, such as `rclone` or a hook), even if rbtrfs is killed. The empty mount-point
directory under `/run` stays.

NB: because the mount lives in a private mount namespace, every command that opens
the repository then needs root, including `snapshots`, `ls`, `dump`, `restore`
and `backup --dry-run`.

The test suite uses a local filesystem as a stand-in for the share. NFS has been used
by hand against a real server, but is not part of the tests.

If the share is already mounted on the host, skip all this and point `repository`
at the mounted path.
