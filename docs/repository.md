# Repository

`repository` in the profile says where the restic repository lives.

| Value | Backend |
|---|---|
| `/path/to/repo` | local directory |
| `rest:https://host/path` | restic REST server |
| `rclone:remote:path` | any rclone remote (needs `rclone` installed) |

Only local paths are covered by the tests. `rest:` and `rclone:` are accepted but
untested. restic's own `sftp:` and `s3:` URLs are not supported; use an rclone
remote for those.

The password comes from exactly one of `password`, `password_file` or
`password_command`.

## A share that is not mounted on the host

If the repository is on NFS, CIFS or similar and the share is not mounted on the
host, rbtrfs can mount it for the run:

```toml
[profile.default]
repository = "/run/rbtrfs/repo/restic/mybox"   # a path below the mount target

[profile.default.repository_mount]
type    = "nfs"
source  = "nas.local:/export/backups"
options = "vers=4.2"                            # optional
target  = "/run/rbtrfs/repo"                    # optional, this is the default
```

rbtrfs runs `mount -t <type> [-o <options>] <source> <target>` before it opens the
repository, so `mount.nfs` and the other helpers resolve hostnames and options as
usual. `type` can be anything `mount(8)` accepts, and `repository` must be a path
below `target`.

The mount is private to the rbtrfs process: it never appears in the host's mount
table and goes away when the process exits, even if it is killed.

NB: because the mount lives in a private mount namespace, every command that opens
the repository then needs root, including `snapshots`, `ls`, `dump`, `restore`
and `backup --dry-run`.

A local filesystem stands in for the share in the tests. A real NFS server is not
covered.

If the share is already mounted on the host, skip all this and point `repository`
at the mounted path.
