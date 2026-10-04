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

Files that are not in a selected subvolume are never backed up. See
[Configuration](configuration.md#subvolumes) for how subvolumes are chosen.
