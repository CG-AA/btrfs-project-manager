//! Shared harness for the fake-backend scenario tests: a temporary root, a controllable clock and
//! `bpm` commands dispatched in-process.
#![allow(dead_code, unused_imports)]

pub use bpm::btrfs::Btrfs;
pub use bpm::btrfs::fake::FakeBtrfs;
use bpm::cli::Cli;
use bpm::clock::FakeClock;
use bpm::ctx::{Ctx, Invoker, Opts};
pub use bpm::store::{ProjectState, SnapshotKind, SnapshotMeta, Stage, Store, Unit};
use bpm::util::fs::ReflinkMode;
use clap::Parser;
pub use std::fs;
pub use std::path::{Path, PathBuf};
use std::sync::Arc;
pub use std::time::Duration;

pub struct Env {
    _dir: tempfile::TempDir,
    pub base: PathBuf,
    pub root: PathBuf,
    pub fake: Arc<FakeBtrfs>,
    pub clock: Arc<FakeClock>,
}

impl Env {
    pub fn new(extra: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let root = base.join("space");
        fs::create_dir(&root).unwrap();
        let fake = Arc::new(FakeBtrfs::new());
        fake.register_existing(&root).unwrap();
        let cfg = format!(
            r#"
version = 1
[global]
hooks_dir = "{b}/hooks"
archive_dir = "{b}/archives"
heavy_min_free = "1G"
lock_timeout = "2s"
[[root]]
path = "{r}"
ignore = ["datasets"]
adopt = "manual"
adopt_min_age = "0s"
[root.container]
interval = "6h"
[defaults]
banlist_settle = "0s"
[defaults.recompress]
min_expected_gain = 0.0
min_free = "1G"
[defaults.shrink_guard]
min_files = 5
min_bytes = "1K"
{extra}
"#,
            b = base.display(),
            r = root.display()
        );
        fs::write(base.join("config.toml"), cfg).unwrap();
        let clock = Arc::new(FakeClock::at(&jiff::Timestamp::now().to_string()));
        fake.set_clock(clock.clone());
        let env = Env { _dir: dir, base, root, fake, clock };
        env.run(&["setup", "--no-units"]).unwrap();
        env
    }

    pub fn ctx(&self) -> Ctx {
        let cfg_path = self.base.join("config.toml");
        let loaded = bpm::config::load(Some(&cfg_path)).unwrap();
        Ctx {
            cfg: loaded.config,
            cfg_path: loaded.path,
            btrfs: self.fake.clone(),
            clock: self.clock.clone(),
            tz: jiff::tz::TimeZone::UTC,
            opts: Opts { quiet: true, no_sudo: true, config: Some(cfg_path), ..Default::default() },
            invoker: Invoker {
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
                user: "tester".into(),
                argv: "test".into(),
            },
            reflink: ReflinkMode::Auto,
        }
    }

    pub fn run(&self, args: &[&str]) -> anyhow::Result<()> {
        let mut argv = vec!["bpm"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv)?;
        bpm::ops::dispatch(&self.ctx(), cli.cmd)
    }

    pub fn clock_now(&self) -> jiff::Timestamp {
        use bpm::clock::Clock;
        self.clock.now()
    }

    pub fn advance(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
    }

    pub fn store(&self) -> Store {
        Store::new(&self.root, ".bpm", false)
    }
    pub fn unit(&self, name: &str) -> Unit {
        self.store().unit(name)
    }
    pub fn snaps(&self, name: &str) -> Vec<SnapshotMeta> {
        self.unit(name).snapshots().unwrap()
    }
    pub fn state(&self, name: &str) -> ProjectState {
        self.unit(name).read_state().unwrap()
    }
    pub fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

pub fn write_files(dir: &Path, n: usize, prefix: &str) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..n {
        fs::write(dir.join(format!("{prefix}{i}.txt")), format!("{prefix} file {i} {}\n", "lorem ipsum ".repeat(20)))
            .unwrap();
    }
}

pub fn make_rust_project(env: &Env, name: &str) {
    let p = env.p(name);
    write_files(&p.join("src"), 40, "src");
    fs::write(p.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
    fs::create_dir_all(p.join(".git")).unwrap();
    fs::write(p.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    write_files(&p.join("target/debug"), 10, "obj");
}

/// A project with `n` source files and no build directory.
pub fn make_project(env: &Env, name: &str, n: usize) {
    let p = env.p(name);
    write_files(&p.join("src"), n, "src");
    fs::write(p.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
    fs::create_dir_all(p.join(".git")).unwrap();
    fs::write(p.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
}

/// What the Claude Code PreToolUse hook runs for one project.
pub fn hook_snap(env: &Env, name: &str, destructive: bool) {
    let mut a = vec!["snap", name, "--kind", "hook", "--quick", "--if-changed", "--never-fail", "--lock-timeout", "3s"];
    if !destructive {
        a.extend(["--throttle", "2m"]);
    }
    env.run(&a).unwrap();
}

/// Does snapshot `id` of `name` contain `rel`?
pub fn snap_has(env: &Env, name: &str, id: u64, rel: &str) -> bool {
    env.unit(name).snapshot_path(id).join(rel).symlink_metadata().is_ok()
}

pub fn snap_read(env: &Env, name: &str, id: u64, rel: &str) -> Option<String> {
    fs::read_to_string(env.unit(name).snapshot_path(id).join(rel)).ok()
}

pub fn any_snap_has(env: &Env, name: &str, rel: &str) -> bool {
    env.snaps(name).iter().any(|m| snap_has(env, name, m.id, rel))
}

pub fn describe(env: &Env, name: &str) -> String {
    env.snaps(name)
        .iter()
        .map(|m| {
            format!(
                "#{}({},stats={},hold={})",
                m.id,
                m.kind,
                m.complete_stats().map(|s| s.files as i64).unwrap_or(-1),
                m.hold
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Install a global hook script for `event` (e.g. "post-snapshot").
pub fn install_hook(env: &Env, event: &str, name: &str, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = env.base.join("hooks").join(event);
    fs::create_dir_all(&dir).unwrap();
    let hook = dir.join(name);
    fs::write(&hook, format!("#!/bin/sh\n{script}\nexit 0\n")).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    hook
}

/// Entries of the root whose name contains `marker`.
pub fn root_entries(env: &Env, marker: &str) -> Vec<PathBuf> {
    fs::read_dir(&env.root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().contains(marker))
        .collect()
}

impl Env {
    /// Switch the root to automatic adoption.
    pub fn auto_adopt(&self) {
        let cfgp = self.base.join("config.toml");
        let c = fs::read_to_string(&cfgp).unwrap().replace("adopt = \"manual\"", "adopt = \"auto\"");
        fs::write(&cfgp, c).unwrap();
    }

    /// Which uuid each store unit's record and snapshots belong to, by project label.
    pub fn units_by_uuid(&self, labels: &[(bpm::btrfs::Uuid, &str)]) -> Vec<(String, String, Vec<String>)> {
        let label = |u: bpm::btrfs::Uuid| {
            labels.iter().find(|(x, _)| *x == u).map(|(_, l)| l.to_string()).unwrap_or("?".into())
        };
        self.store()
            .units()
            .unwrap()
            .into_iter()
            .map(|(u, rec)| {
                let snaps = u.snapshots().unwrap().iter().map(|m| label(m.source_uuid)).collect();
                (u.name.clone(), label(rec.uuid), snaps)
            })
            .collect()
    }
}
