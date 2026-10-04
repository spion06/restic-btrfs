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

A pattern that starts with `/` is a path as it is recorded in the backup, so it
includes the mount point. It applies to the subvolume that contains it. Glob
characters work after the mount point but not inside it: `/ho*/alice` matches
nothing. Patterns that do not start with `/` match at any depth.

A trailing `/` limits a pattern to directories.

NB: do not start a pattern with `!`. Patterns are exclusions already, and rbtrfs
rejects a leading `!`.

Files that are not in a selected subvolume are never backed up. See
[Configuration](configuration.md#subvolumes) for how subvolumes are chosen.
