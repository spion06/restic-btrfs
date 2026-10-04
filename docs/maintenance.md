# Maintenance and safety

## Local snapshots

Every run leaves read-only snapshots on the btrfs filesystem. A backup deletes the
old ones according to `keep_local` and `keep_local_days`, so you do not normally need
to do anything.

A run that fails removes the snapshots it took. A run that is killed cannot, and its
snapshots stay behind: they are real btrfs subvolumes and they survive the process.
They count as an ordinary set, so later backups remove them once newer runs push them
out of `keep_local`.

`rbtrfs gc` does the same cleanup on demand. It applies the same settings as a backup,
and `--keep-local` and `--keep-local-days` override them, so `rbtrfs gc --keep-local 0`
removes everything. Add `--all-keys` to also clean up subvolumes that the profile no
longer selects.

Incremental backups do not depend on local snapshots. rbtrfs finds the previous run
through the repository.

## Retention

`rbtrfs forget` thins the repository according to the profile's `retention` table.
It works per host and only looks at backups that rbtrfs made, so your other restic
snapshots are never touched. Profiles are not told apart: if several profiles back up
to one repository from the same host, `forget` applies one policy to all their
backups together. Give each profile its own repository. It also removes the per-subvolume snapshots that
`rbtrfs snapshots --all` shows, except those from the newest run (or later), which
the next backup needs.

`--dry-run` shows what would be removed. By default `forget` only removes snapshots.
Add `--prune` to also delete the data nothing refers to any more. rustic does this in
two steps: the first prune marks unneeded data and a later prune, at least 23 hours
afterwards, deletes it.

`--instant-delete` deletes it straight away. That skips the second step, and if
anything else uses the repository at the same time, it can corrupt it. On a terminal
rbtrfs asks you to type `yes`. Elsewhere, such as cron or systemd, it refuses unless
you also pass `--allow-unsafe`.

## Concurrency

Only one `backup`, `gc` or `forget` runs at a time on a machine. A second one fails
immediately. This is enforced with a lock on `/run/rbtrfs/rbtrfs.lock`.

restic protects a repository with lock files in the repository itself. rustic_core,
which rbtrfs uses, does not write them, and rbtrfs cannot add them. So other tools
that use the same repository are not stopped from running at the same time. What
happens then depends on the tool. Every row except `restic backup` and the read-only
commands was tested by running it while a backup was running. That row was tested
against a pruning rustic instead, and follows from backups only ever adding to the
repository:

| Other activity | Safe? |
|---|---|
| rbtrfs `backup`, `gc` or `forget` on the same machine | Yes. The lock refuses it. |
| rustic `forget` and `prune`, default options | Yes. Its two-step prune keeps data for 23 hours before deleting it, and recovers anything that turns out to be needed. This also held with the delay set to 0. |
| `restic backup`, and anything that only reads (`restore`, `ls`, `check`) | Yes. |
| `restic prune` or `restic forget --prune` | No. restic assumes nobody else is writing unless it sees a lock. In one run `restic check` afterwards reported a missing pack, although the backup had exited successfully. |
| `--instant-delete`, from any tool | No. In one run it crashed a backup. |

NB: do not run `restic prune` against a repository that rbtrfs backs up to unless
you are sure no backup is running. Use `rbtrfs forget --prune` or rustic instead.

The state that rustic's two-step prune leaves behind is valid to restic:
`restic check --read-data` accepts it.
