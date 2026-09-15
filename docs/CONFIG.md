# Configuration

bpm reads `/etc/bpm/config.toml` (or `--config FILE`, or `$BPM_CONFIG`). Without a file, the
embedded default (`bpm config --default`) is used. Unknown keys are errors.

`bpm config <project> --show-origin` prints the merged result for one project with the layer each
value came from.

## Precedence

Per-project policy is merged key by key, later layers winning:

1. built-in defaults (`assets/builtin-policy.toml`)
2. `[defaults]` and its sub-tables
3. `[profiles.<name>.policy]` of each matched profile, in `global.profile_order`
4. `[root.policy]` of the root containing the project
5. `[projects."<name>".policy]` in the global file
6. `<project>/.bpm.toml` `[policy]` (only when `global.project_config = true`)

Banlists are unions: `defaults.banlist` + matched profile banlists + `root.banlist_add` +
`projects.<name>.banlist_add` + `.bpm.toml` `banlist_add`, minus both `banlist_remove` lists.
Entries are paths relative to the project root, like `target` or `web/node_modules`. bpm never
follows a symlink in the middle of an entry: if `web` is a symlink, `web/node_modules` is left
alone (and stays in snapshots). The same applies to paths given to `bpm restore`.

## `[global]`

| Key | Default | Meaning |
|---|---|---|
| `store_dir` | `".bpm"` | store subvolume name inside each root |
| `lock_timeout` | `"30s"` | how long manual commands wait for a project lock (tick never waits) |
| `sudo` | `true` | re-exec through `sudo -n` when a command needs root |
| `hooks_dir` | `"/etc/bpm/hooks.d"` | global hooks, see HOOKS.md |
| `hook_timeout` | `"60s"` | hooks are killed after this |
| `project_hooks` | `"off"` | `off`, `allowlist`, or `on` for hooks inside project repositories |
| `project_hooks_allow` | `[]` | project names allowed when `project_hooks = "allowlist"` |
| `project_config` | `true` | honour `<project>/.bpm.toml` |
| `archive_dir` | `"/archives/bpm"` | where `bpm archive` writes |
| `archive_zstd_level` | `19` | zstd level for archives |
| `heavy_min_free` | `"10G"` | adopt, convert, collapse and recompress pause below this free space |
| `profile_order` | generic, python, node, cmake, rust | order in which matched profiles apply |
| `hook_throttle` | `"2m"` | Claude hook: minimum spacing for non-destructive commands |
| `destructive_patterns` | see default file | regexes; a matching shell command is snapshotted with no throttle |

## `[[root]]`

| Key | Default | Meaning |
|---|---|---|
| `path` | required | absolute path, ideally a subvolume root |
| `ignore` | `[]` | top-level names that are never projects (they stay in container snapshots) |
| `adopt` | `"manual"` (`"auto"` in the default file) | `auto`: new top-level directories and subvolumes are adopted, one at a time; one that keeps failing is retried after 1 h, doubling up to a day |
| `adopt_min_age` | `"15m"` | a new directory must be untouched this long before auto-adoption |
| `banlist_add` | `[]` | added to every project in this root |
| `policy` | `{}` | policy overrides for this root |

`[root.container]` controls snapshots of the root itself: its loose files and ignored or unadopted
directories. Project subvolumes and the store are excluded automatically.

| Key | Default |
|---|---|
| `enabled` | `true` |
| `interval` | `"6h"` (at most one snapshot per interval, only when the root changed) |
| `keep_all` | `"24h"` |
| `daily` | `"7d"` |
| `weekly` | `"4w"` |

## Policy keys

Set these under `[defaults]`, `[profiles.X.policy]`, `[root.policy]`, `[projects."X".policy]` or
`[policy]` in `.bpm.toml`.

| Key | Default | Meaning |
|---|---|---|
| `banlist_precreate` | `"primary"` | create missing banned dirs as empty subvolumes: `primary` = each profile's first entry, explicit project additions, and any dir seen before; `all`; `none` |
| `banlist_settle` | `"2m"` | a plain banned dir is converted only after no file in it changed for this long |
| `keep_build_on_adopt` | `true` | reflink build contents into the new nested subvolumes during adopt |
| `snapshot.min_interval` | `"5m"` | minimum spacing of automatic snapshots |
| `snapshot.stats` | `true` | count files and bytes in new snapshots (needed by the shrink guard) |
| `snapshot.stats_budget` | `"120s"` | give up counting after this (incomplete stats block collapse and recompress) |
| `snapshot.stats_min_interval` | `"30m"` | for trees whose count takes over 2 s, count at most this often; a snapshot taken without a count is counted later, and retention waits for that |
| `thin.keep_all` | `"6h"` | keep every automatic snapshot younger than this |
| `thin.hourly` | `"48h"` | then the oldest snapshot of each hour |
| `thin.daily` | `"14d"` | then of each day |
| `thin.weekly` | `"8w"` | then of each week |
| `thin.safety_ttl` | `"7d"` | pre, post, rollback and pre-restore snapshots are protected this long |
| `thin.min_free` | `"20G"` | below this free space, automatic snapshots pause (hook and manual ones still work) |
| `lifecycle.dormant_after` | `"14d"` | idle time before dormant |
| `lifecycle.cold_after` | `"60d"` | idle time before cold (collapse + recompress) |
| `lifecycle.adopt_grace` | `"1d"` | no collapse or recompress within this time after adoption |
| `recompress.enabled` | `true` | |
| `recompress.level` | `9` | zstd level 1 to 15 (kernel 6.15+) |
| `recompress.max_bytes` | `"50G"` | larger projects are not recompressed automatically |
| `recompress.min_free` | `"30G"` | also requires 1.1 × project size + 2 GiB free |
| `recompress.min_expected_gain` | `0.05` | skip when a 32-file sample compresses less than 5% better than zstd 1 |
| `recompress.nested` | `false` | build subvolumes are never recompressed |
| `shrink_guard.enabled` | `true` | |
| `shrink_guard.ratio` | `0.30` | freeze when files or bytes drop more than 30% below the high-water mark… |
| `shrink_guard.min_files` | `100` | …and at least this many files were lost… |
| `shrink_guard.min_bytes` | `"50M"` | …or at least this many bytes |
| `shrink_guard.catastrophic` | `0.10` | freeze regardless of floors when ≤ 10% of files remain |
| `shrink_guard.exclude` | `[".git"]` | not counted (so `git gc` does not trip the guard) |
| `shrink_guard.sentinels` | `[".git"]` | freeze when one of these paths disappears |
| `shrink_guard.thin_after_freeze` | `true` | snapshots taken after a freeze still thin normally |

Durations accept `90s`, `5m`, `6h`, `14d`, `8w` and combinations like `1h 30m`. Sizes accept
`500M`, `20G`.

## Profiles

Built in: `rust` (`Cargo.toml` → `target`), `cmake` (`CMakeLists.txt` → `build`,
`cmake-build-debug`, `cmake-build-release`, `out`), `node` (`package.json` → `node_modules`,
`dist`, `.next`, `.turbo`), `python` (`pyproject.toml`, `requirements.txt`, `setup.py` → `.venv`,
`venv`, `__pycache__`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`), `generic` (none). A project
matches every profile with a marker file at its top level. A `[profiles.<name>]` table in the
config replaces the built-in profile of that name or adds a new one:

```toml
[profiles.zig]
markers = ["build.zig"]
banlist = ["zig-out", ".zig-cache"]
[profiles.zig.policy.snapshot]
min_interval = "10m"
```

## `<project>/.bpm.toml`

```toml
managed = true                 # false: bpm ignores this project entirely
profile = "auto"               # or ["rust", "node"]
banlist_add = ["out", "data/cache"]
banlist_remove = ["dist"]
sentinels_add = ["docs", "src/core"]

[policy.lifecycle]
dormant_after = "30d"
```

An agent working in the repository can edit this file, so it cannot change hooks or sudo
behaviour, and its `[policy.shrink_guard]` and `[policy.thin]` tables are ignored (with a warning):
whether deletions freeze cleanup and how long history is kept are set in the admin config, for
example `[projects."name".policy.thin]`. Set `global.project_config = false` to ignore these files
entirely.
