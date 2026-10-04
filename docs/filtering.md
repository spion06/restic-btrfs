# Filtering

The `exclude` list in a profile leaves files and directories out of the backup.
Patterns work like restic's `--exclude`.

```toml
exclude = ["**/.cache", "*.tmp", "node_modules/", "/home/alice/Downloads"]
```

| Pattern | Excludes |
|---|---|
| `*.tmp` | anything ending in `.tmp`, at any depth |
| `.cache` | anything named `.cache`, at any depth |
| `alice/.cache` | `.cache` directly inside any directory named `alice` |
| `node_modules/` | directories named `node_modules`, at any depth |
| `/home/alice/Downloads` | that one path |
| `/home/*/tmp` | `tmp` in each directory under `/home` |

A pattern that starts with `/` is a full path, written the way the file appears on
your running system, such as `/home/alice/Downloads`. rbtrfs works out which
subvolume it belongs to from the mount point at the start of the path. You do not
need to know where the snapshots are kept.

The mount point part has to be written out. Wildcards work after it, as in
`/home/*/tmp`, but not inside it: `/ho*/alice` matches nothing.

A pattern that does not start with `/` matches at any depth, in every subvolume.

A trailing `/` limits a pattern to directories.

NB: do not start a pattern with `!`. Patterns are exclusions already, and rbtrfs
rejects a leading `!`.

## Checking what your patterns do

A dry run walks the selected paths with the same matcher a backup uses and reports
how much would be stored and which paths are skipped, largest first. It reads file
metadata only, so it is quick (about 6 seconds for 700,000 files), and it shows the
effect of an `exclude` before you commit to a backup:

    sudo rbtrfs backup --dry-run

```
/home: would store 698185 files (137.2 GB); excluded 12 path(s) (183.2 GB)
      84.0 GB  /home/alice/.local/share/containers
      47.7 GB  /home/alice/projects/rbtrfs/target
      ...
```

Sizes are the files' apparent sizes, before compression and deduplication. Run it as
root to include directories your own user cannot read. Add `--no-scan` to skip the
walk and print only the plan.

Files that are not in a selected subvolume are never backed up. See
[Configuration](configuration.md#subvolumes) for how subvolumes are chosen.
