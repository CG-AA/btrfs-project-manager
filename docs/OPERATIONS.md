# Operations

## Migrating this machine from snapper

Today snapper takes hourly snapshots of all of `/space` (config `space`), including every build
directory. The steps below hand `/space` to bpm. snapper keeps managing `/`, `/home` and `/archives`.

1. Install and set up.

   ```sh
   scripts/install.sh
   sudo bpm setup
   bpm doctor
   ```

   `setup` creates `/etc/bpm/config.toml` (review `ignore`, and `adopt = "auto"` in `[[root]]`),
   the store subvolume `/space/.bpm`, and enables two timers: `bpm.timer` (snapshots, shrink guard,
   retention, every 5 minutes) and `bpm-heavy.timer` (one adopt, build-dir conversion, collapse or
   recompression at a time). A long adoption or recompression never delays snapshots of other
   projects. The units use the config file `setup` was run with. The first tick takes a container
   snapshot of `/space`, which still protects every not-yet-adopted directory.

2. Stop snapper's timeline for `/space`.

   ```sh
   sudo bpm migrate-from-snapper            # timeline off; snapper's existing snapshots stay where they are
   sudo bpm migrate-from-snapper --import   # alternative: move them into bpm's container store as held snapshots
   ```

   Importing keeps the pre-migration history visible in `bpm list --container`, but those snapshots
   keep pinning old build output until you remove them. Without `--import`, step 4 frees that space
   immediately.

3. Adopt projects. Do this when no build or editor is writing into them. Adoption refuses a
   directory with files open for writing, and warns about shells whose working directory is
   inside (they must `cd` again afterwards).

   ```sh
   bpm status                      # lists unadopted directories
   sudo bpm adopt cam-stitch       # try a small one first
   bpm status cam-stitch
   sudo bpm adopt --all            # or leave it to the timer: one project per tick, only when unused
   ```

   Adoption copies with reflinks, so it needs metadata space only. Projects with large build
   directories (`slime_os-private`: 26 GB in `build/` and `target/`) take a few minutes.

4. Once every project is adopted, delete snapper's snapshots of `/space` and its config. This is
   what frees the space pinned by old build output.

   ```sh
   sudo bpm migrate-from-snapper --delete-snapper --yes
   sudo btrfs subvolume sync /space   # optional: wait until the space is reclaimed
   ```

   If you used `--import`, the space is freed only when you remove the imported snapshots:
   `sudo bpm rm --container @container <id>... --force-held`.

5. Add the Claude Code hook: `bpm setup --print-claude-hook`, then paste into
   `~/.claude/settings.json`.

## Recovery playbook

**A file or directory inside a project was deleted or broken.**

```sh
bpm list myproj                         # find a snapshot from before the damage
bpm diff myproj 41                      # what changed since snapshot 41 (A added, D deleted, M modified)
bpm restore myproj 41 src/lost.rs       # copy it back (add --overwrite to replace an existing file)
bpm restore myproj 41 src --to /tmp/x   # or copy it somewhere else to compare
```

**Most of a project was wiped (the shrink guard fired).** `bpm status` shows `FROZEN` and the id of
the last good snapshot, which is held. Nothing from before the freeze will be deleted. The guard
checks every snapshot, including hook snapshots taken without a file count: those are counted by
the next tick, and retention waits until that has happened. The last good snapshot is the newest
one that passes the check, not simply the one before the loss was noticed. `held` refers to it
while the project is frozen.

```sh
bpm status myproj
bpm diff myproj held                    # confirm what was lost
bpm rollback myproj held                # replace the project with it; the wiped state is kept as a snapshot
```

If the deletion was intentional, `bpm unfreeze myproj` accepts the current state: it snapshots and
counts the live tree if needed and makes that the new shrink-guard reference.

**The whole project directory is gone.** The next tick marks it orphaned, freezes it and holds its
newest snapshot.

```sh
bpm restore myproj --recreate           # newest snapshot; or: bpm restore myproj 37 --recreate
```

Build directories are not in snapshots, so they start empty.

**Wrong rollback.** Every rollback first snapshots the current state as kind `rollback`:

```sh
bpm list myproj --kind rollback
bpm rollback myproj <that id>
```

**Something was interrupted (power loss, kill).** `bpm doctor` lists leftovers such as
`name.bpm-tmp`, `name.bpm-old`, `name.bpm-rollback-*`, snapshots without metadata, and stale
journals. `sudo bpm doctor --fix` repairs the safe cases and never deletes the only copy of data.

**A `name.bpm-keep-<op>-<time>` directory appeared.** bpm replaces trees in adopt, convert,
rollback and restore, and deletes the replaced tree only when it can prove the tree holds nothing
newer than what was kept (unchanged since the copy or snapshot, no nested subvolumes or mounts,
not in use). Otherwise it keeps the tree under this name and logs an error. This happens when
something wrote into the project during the operation, for example a shell whose working
directory was inside. Compare it with the project, copy back what you need, then delete it.
`bpm doctor` reports these and never removes them.

## Limits to know

- Snapshots are on the same disk. They protect against deletion and overwrites, not disk failure.
  `/archives` is on the same filesystem too; copy archive files off the machine for backups.
- Anything with passwordless sudo, including an agent running as your user, can delete snapshots.
  To close that gap, restrict the agent's sudo to `bpm snap`.
- Nested subvolumes you create inside a project yourself are not covered by its snapshots.
  `bpm status <project>` warns about them.
- Snapshots are crash-consistent. Databases that need a clean state can use a `pre-snapshot` hook.
- `bpm archive --delete-live` deletes the live project only if it is still identical to the
  archived snapshot; an edit during compression, or archiving an older `--snapshot`, keeps it.
- Changes are detected when the filesystem flushes. Every check forces a flush first.

## Checks after deployment

- `bpm doctor` shows no `ERROR`, and `systemctl list-timers 'bpm*'` lists both timers.
- `stat -c '%i %n' /space/*` prints `256` for every adopted project.
- `ls /space/.bpm/projects/slime_os-private/*/snapshot/target` is empty.
- Edit a file, wait for a tick (`journalctl -u bpm -f`), and `bpm list <project>` shows a new
  snapshot.
- Make a scratch project, adopt it, `rm -rf` it, wait for a tick, and `bpm restore <it> --recreate`.
- As your user, `btrfs subvolume delete /space/.bpm/projects/<p>/<id>/snapshot` fails.
- Run a Bash tool call in Claude Code and check `bpm list <project> --kind hook`.
