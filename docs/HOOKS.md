# Hooks

Hooks are executables that bpm runs around its operations.

- Global hooks: `/etc/bpm/hooks.d/<event>/NN-name`, run as root. They must be owned by root and
  not writable by group or others.
- Project hooks: `<project>/.bpm/hooks/<event>/NN-name`, run as the project owner with no
  supplementary groups. They are **off by default** because anything inside a repository can be
  written by an agent working in it. Enable them with `global.project_hooks = "allowlist"` plus
  `project_hooks_allow = ["name"]`, or `"on"`.

Files run in name order, global before project. `bpm hooks list [--project X]` shows each hook and
whether it would run. `bpm hooks run <event> <project>` runs them by hand.

## Events

| Event | When | Can veto |
|---|---|---|
| `pre-snapshot`, `post-snapshot` | every snapshot (tick, snap, hook, guard snapshots) | pre |
| `pre-delete`, `post-delete` | every snapshot deletion | pre |
| `pre-adopt`, `post-adopt` | adopt (manual and automatic) | pre |
| `pre-rollback`, `post-rollback` | rollback | pre |
| `pre-recompress`, `post-recompress` | recompress | pre |
| `pre-archive`, `post-archive` | archive | pre |
| `stage-change` | active/dormant/cold transitions | no |
| `on-shrink-detected` | the shrink guard fired | no |
| `on-freeze` | a project became frozen | no |
| `on-orphaned` | a project directory disappeared | no |

A `pre-*` hook that exits non-zero, or times out after `global.hook_timeout`, cancels that operation
for that project only. A tick continues with other projects. Other hook failures are logged.

## Environment

Hooks start with a clean environment (`PATH`, `HOME`, `LANG`) plus:

| Variable | Content |
|---|---|
| `BPM_EVENT` | event name |
| `BPM_HOOK_SCOPE` | `global` or `project` |
| `BPM_PROJECT`, `BPM_PROJECT_PATH`, `BPM_ROOT` | project name, live path, root |
| `BPM_SNAPSHOT_ID`, `BPM_SNAPSHOT_PATH`, `BPM_SNAPSHOT_KIND` | for snapshot events |
| `BPM_STAGE_FROM`, `BPM_STAGE_TO` | for `stage-change` |
| `BPM_REASON` | human-readable reason |
| `BPM_REF_SNAPSHOT`, `BPM_TRIGGER_SNAPSHOT`, `BPM_FILES_BEFORE`, `BPM_FILES_AFTER`, `BPM_BYTES_BEFORE`, `BPM_BYTES_AFTER` | for `on-shrink-detected` |
| `BPM_DRY_RUN` | `1` under `--dry-run` |
| `BPM_INVOKER` | user who ran the command |

The same variables arrive as one JSON object on stdin. The working directory is the project.

## Examples

Desktop notification when the guard fires (`/etc/bpm/hooks.d/on-shrink-detected/10-notify`):

```sh
#!/bin/sh
sudo -u lamb DISPLAY=:0 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
  notify-send -u critical "bpm: $BPM_PROJECT shrank" "$BPM_REASON — last good snapshot #$BPM_REF_SNAPSHOT"
```

Flush a SQLite database before snapshots of one project (`/etc/bpm/hooks.d/pre-snapshot/20-sqlite`):

```sh
#!/bin/sh
[ "$BPM_PROJECT" = "glowing-glass-admin" ] || exit 0
sqlite3 "$BPM_PROJECT_PATH/data/app.db" 'PRAGMA wal_checkpoint(TRUNCATE);'
```

Never delete snapshots of a project on Fridays (`/etc/bpm/hooks.d/pre-delete/50-friday`):

```sh
#!/bin/sh
[ "$(date +%u)" != 5 ]
```

## Claude Code

`bpm setup --print-claude-hook` prints:

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash",
        "hooks": [ { "type": "command", "command": "/usr/local/bin/bpm snap --claude-hook", "timeout": 20 } ] }
    ]
  }
}
```

Put it in `~/.claude/settings.json` for every project, or in a project's `.claude/settings.json`.
It also fires for subagents.

For every shell command the agent is about to run, `bpm snap --claude-hook`:

1. reads the hook JSON on stdin (`cwd`, `tool_input.command`);
2. finds the projects involved: the one containing `cwd` and any project paths in the command;
3. treats the command as destructive when it matches `global.destructive_patterns` (`rm -r`,
   `git clean`, `git reset --hard`, `git checkout .`, `find -delete`, `rsync --delete`, `mv`, …);
4. for non-destructive commands, skips projects snapshotted within `global.hook_throttle`;
5. runs `sudo -n bpm snap <project> --kind hook --if-changed --quick`, which flushes the filesystem
   and snapshots only if the project changed since its newest snapshot;
6. always exits 0, so a failure never blocks the agent.

If a tick holds the project lock for a long file count, the hook snapshot goes ahead without the
lock and the next tick reconciles.
