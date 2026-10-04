# Maintenance and safety

## Local snapshots

Each run leaves read-only snapshots on the btrfs filesystem. `backup` deletes old
ones per `keep_local` / `keep_local_days`. `rbtrfs gc` does the same on demand and
also reclaims snapshots orphaned by a crashed or killed run (they are real
subvolumes and survive the process). `--all-keys` also sweeps subvolumes the
profile no longer selects. Incremental backups do not depend on local
snapshots: the previous run is found through the repository.

## Repository retention

`rbtrfs forget` keeps merged snapshots per your `[retention]` policy and drops the
internal per-subvolume *part* snapshots older than the newest run. Snapshots rbtrfs
did not create are never touched. `--prune` also frees unreferenced data; by
default rustic only marks it and deletes it on a later prune. `--dry-run` shows
what would go.

`--instant-delete` frees it immediately but skips that safety. It asks you to type
`yes` on a terminal; elsewhere (cron, systemd) it refuses unless `--allow-unsafe`
is given.

## Concurrency

One `backup`, `gc` or `forget` runs at a time per machine, enforced with a lock at
`/run/rbtrfs/rbtrfs.lock`. rustic_core is lock-free by design and cannot write
restic's repository lock, so other tools are not excluded. What is safe alongside a
running `rbtrfs backup` (each row was tested):

| other activity | safe? |
|---|---|
| `rbtrfs forget`/`gc`/`backup` on the same machine | yes: refused by the run lock |
| rustic forget/prune with default options | yes: two-phase pruning marks unneeded packs and deletes them only after 23 h, recovering any that turn out to be used. Also held with the delay set to 0 |
| `restic backup`, readers (`restore`, `ls`, `check`) | yes |
| `restic prune` / `restic forget --prune` | no. restic relies on locks rbtrfs cannot take; reproduced a repository with a missing pack while the backup exited 0 |
| `--instant-delete` (any tool) | no. Reproduced a backup crashing inside rustic_core |

So prune with `rbtrfs forget --prune` (or rustic), and run restic's own prune only
when no backup can be running. Delayed-deletion state is accepted by
`restic check --read-data`.

## Restoring

`restore` writes plain files; run it as root to keep ownership. `--as-subvolume`
creates the target as a new btrfs subvolume instead (root; the target must be on
btrfs and not exist). Subvolumes that were nested in the original come back as
plain directories. `latest` is the newest merged snapshot from *this host*; use
`--host NAME` or `--any-host` when restoring onto a rebuilt machine.
