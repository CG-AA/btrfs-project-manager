# bpm — btrfs project manager

bpm protects a directory of projects (here `/space`) against accidental destruction by people or
AI agents, without paying for build output in every snapshot.

- Every top-level project becomes its own btrfs subvolume.
- Build and cache directories (`target/`, `build/`, `node_modules/`, `.venv/`, …) become **nested
  subvolumes**, which btrfs snapshots skip. `cargo clean` and `rm -rf build` keep working; bpm
  recreates the directory afterwards.
- A systemd timer runs `bpm tick` every 5 minutes. A project is snapshotted only when its content
  actually changed, detected exactly from btrfs transaction counters.
- A **shrink guard** notices when a project suddenly loses files, bytes, or a sentinel such as
  `.git`. It pins the last good snapshot and freezes all cleanup until you decide.
- Deleting a whole project is noticed too: its snapshots are kept and `bpm restore <name> --recreate`
  brings it back.
- Idle projects age: after 14 days only a thinned history remains; after 60 days they collapse to
  one snapshot and their live tree is recompressed with zstd level 9. Archiving to a send stream
  is a manual command.
- A Claude Code `PreToolUse` hook takes a snapshot right before an agent's shell command runs.

Snapshots live in a root-owned store (`/space/.bpm`) outside the projects, so `rm -rf /space/foo`
cannot take them along and an unprivileged agent cannot delete them.

## Install

```sh
# cargo is not on PATH on this machine; either restore rustup shims or point at a toolchain:
export CARGO=$HOME/.rustup/toolchains/nightly-2026-05-26-x86_64-unknown-linux-gnu/bin/cargo
PATH=$(dirname $CARGO):$PATH scripts/install.sh      # builds release, installs /usr/local/bin/bpm
sudo bpm setup                                       # /etc/bpm/config.toml, /space/.bpm, systemd timer
bpm doctor
```

Commands that change anything re-exec themselves through `sudo -n`. Read-only commands
(`status`, `list`, `diff`, `config`, `doctor`) work as a normal user.

## Quick start

```sh
bpm status                        # managed projects, stages, freezes, unadopted directories
bpm migrate-from-snapper          # stop snapper's timeline on /space
bpm adopt --all                   # or let the timer adopt one project per tick
bpm setup --print-claude-hook     # paste into ~/.claude/settings.json
```

See [docs/OPERATIONS.md](docs/OPERATIONS.md) for the full migration of this machine and the
recovery playbook.

## Everyday commands

| Task | Command |
|---|---|
| What is protected, what is frozen | `bpm status`, `bpm status <project>` |
| Snapshots of a project | `bpm list <project>` |
| Snapshot now | `bpm snap [project]` (defaults to the project containing the current directory) |
| Snapshot around a risky command | `bpm wrap -- git rebase -i main` |
| What changed since a snapshot | `bpm diff <project> <snap> [<snap>\|live]` |
| Get a file back | `bpm restore <project> <snap> path/to/file [--overwrite] [--to DIR]` |
| Undo everything since a snapshot | `bpm rollback <project> <snap>` (current state is kept as a snapshot) |
| A project directory was deleted | `bpm restore <project> --recreate` |
| Keep a snapshot forever | `bpm hold <project> <snap>` |
| Accept a large deletion as intended | `bpm unfreeze <project>` |
| Merged configuration and where each value comes from | `bpm config <project> --show-origin` |
| Move a finished project to an archive file | `bpm archive <project> [--delete-live --yes]` |

Snapshot selectors: `42`, `latest`, `latest~2`, `held`, `2026-09-14`, `2026-09-14T10:30`.
Every command accepts `--json`, `--dry-run`, `-v`, `--root DIR`, `--config FILE`.

## How change detection works

A snapshot inherits its source subvolume's `ctransid` (last content change), and taking a snapshot
does not bump it. bpm stores the snapshot's `ctransid` and creation transaction. Each tick flushes
the filesystem and compares them with the live subvolume. Equal `ctransid` and a generation no newer
than the snapshot means nothing changed, including deletions and renames. Writes inside nested
build subvolumes do not count. These rules were verified on this machine's kernel; see
`src/policy/change.rs`.

## Lifecycle

| Stage | When | What bpm does |
|---|---|---|
| active | changed within 14 days | snapshot on change (at most every 5 min), keep all snapshots from the last 6 h, hourly for 48 h, daily for 14 d, weekly for 8 w |
| dormant | idle 14 to 60 days | retention only; the thinned history stays available |
| cold | idle 60+ days | collapse to one snapshot, then recompress the live tree at zstd 9 (once per content version) |
| orphaned | project directory disappeared | freeze, hold the newest snapshot, keep everything |
| archived | after `bpm archive --delete-live` | nothing |

Held, manual, imported and received snapshots are never deleted automatically. The newest snapshot
of a project is never deleted automatically. Nothing created before a freeze is deleted while the
project is frozen.

## Documentation

- [docs/CONFIG.md](docs/CONFIG.md): every configuration key, profiles, precedence, `.bpm.toml`
- [docs/HOOKS.md](docs/HOOKS.md): hook events, environment, veto rules, Claude Code integration
- [docs/OPERATIONS.md](docs/OPERATIONS.md): migration from snapper, recovery playbook, checks

## Development

```sh
cargo test                     # unit tests + command scenarios on a fake btrfs backend (no root)
scripts/e2e.sh                 # real btrfs on a loop device, runs as root via sudo
cargo clippy --all-targets && cargo fmt --check
```

Layout: `src/policy` is pure decision logic, `src/mechanics` performs multi-step subvolume
operations, `src/ops` has one module per command, and every btrfs side effect goes through the
`Btrfs` trait (`src/btrfs`), with a real backend, a dry-run wrapper and an in-memory fake.
