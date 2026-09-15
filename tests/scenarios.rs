//! End-to-end command scenarios on a temporary directory, using the fake btrfs backend and a
//! controllable clock. Real-btrfs coverage lives in tests/e2e.rs (root + loop device).

use bpm::btrfs::Btrfs;
use bpm::btrfs::fake::FakeBtrfs;
use bpm::cli::Cli;
use bpm::clock::FakeClock;
use bpm::ctx::{Ctx, Invoker, Opts};
use bpm::store::{ProjectState, SnapshotKind, SnapshotMeta, Stage, Store, Unit};
use bpm::util::fs::ReflinkMode;
use clap::Parser;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

struct Env {
    _dir: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    fake: Arc<FakeBtrfs>,
    clock: Arc<FakeClock>,
}

impl Env {
    fn new(extra: &str) -> Env {
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

    fn ctx(&self) -> Ctx {
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

    fn run(&self, args: &[&str]) -> anyhow::Result<()> {
        let mut argv = vec!["bpm"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv)?;
        bpm::ops::dispatch(&self.ctx(), cli.cmd)
    }

    fn advance(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
    }

    fn store(&self) -> Store {
        Store::new(&self.root, ".bpm", false)
    }
    fn unit(&self, name: &str) -> Unit {
        self.store().unit(name)
    }
    fn snaps(&self, name: &str) -> Vec<SnapshotMeta> {
        self.unit(name).snapshots().unwrap()
    }
    fn state(&self, name: &str) -> ProjectState {
        self.unit(name).read_state().unwrap()
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

fn write_files(dir: &Path, n: usize, prefix: &str) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..n {
        fs::write(dir.join(format!("{prefix}{i}.txt")), format!("{prefix} file {i} {}\n", "lorem ipsum ".repeat(20)))
            .unwrap();
    }
}

fn make_rust_project(env: &Env, name: &str) {
    let p = env.p(name);
    write_files(&p.join("src"), 40, "src");
    fs::write(p.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
    fs::create_dir_all(p.join(".git")).unwrap();
    fs::write(p.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    write_files(&p.join("target/debug"), 10, "obj");
}

#[test]
fn adopt_excludes_build_dirs_and_snapshots_on_change() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();

    assert!(env.fake.is_subvolume(&env.p("demo")).unwrap(), "project is a subvolume");
    assert!(env.fake.is_subvolume(&env.p("demo/target")).unwrap(), "target is a nested subvolume");
    assert!(env.p("demo/target/debug/obj0.txt").exists(), "build contents kept by reflink copy");
    assert!(!env.p("demo.bpm-old").exists() && !env.p("demo.bpm-tmp").exists());

    let snaps = env.snaps("demo");
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].kind, SnapshotKind::Adopt);
    let snap_dir = env.unit("demo").snapshot_path(snaps[0].id);
    assert!(snap_dir.join("src/src0.txt").exists());
    assert!(
        snap_dir.join("target").is_dir() && fs::read_dir(snap_dir.join("target")).unwrap().next().is_none(),
        "target is an empty placeholder"
    );
    let stats = snaps[0].stats.as_ref().unwrap();
    assert_eq!(stats.files, 41, "40 sources + Cargo.toml, .git excluded, target excluded");

    // no change: no snapshot
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.snaps("demo").len(), 1);

    // writes inside the nested build dir are invisible
    fs::write(env.p("demo/target/debug/new.o"), "x").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.snaps("demo").len(), 1, "build output does not trigger snapshots");

    // a source change triggers exactly one snapshot, respecting min_interval
    fs::write(env.p("demo/src/src0.txt"), "changed").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.snaps("demo").len(), 2);
    fs::write(env.p("demo/src/src1.txt"), "changed").unwrap();
    env.advance(60);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.snaps("demo").len(), 2, "min_interval not yet elapsed");
    env.advance(300);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.snaps("demo").len(), 3);

    // container snapshot of the root exists
    assert!(!env.store().container().snapshots().unwrap().is_empty());
}

#[test]
fn shrink_guard_freezes_and_rollback_restores() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    fs::write(env.p("demo/target/debug/marker"), "keep me").unwrap();

    // an agent deletes most of the sources
    for i in 0..30 {
        fs::remove_file(env.p(&format!("demo/src/src{i}.txt"))).unwrap();
    }
    env.advance(600);
    env.run(&["tick"]).unwrap();
    let st = env.state("demo");
    let frozen = st.frozen.clone().expect("project frozen");
    assert!(frozen.reasons.iter().any(|r| r.contains("files")), "{:?}", frozen.reasons);
    let snaps = env.snaps("demo");
    let good = snaps.iter().find(|m| m.hold).expect("last good snapshot held");
    assert_eq!(Some(good.id), frozen.ref_snap);

    // long idle while frozen: nothing from before the freeze is deleted
    for _ in 0..10 {
        env.advance(86400 * 3);
        env.run(&["tick"]).unwrap();
    }
    assert!(env.snaps("demo").iter().any(|m| m.id == good.id));
    assert!(env.state("demo").frozen.is_some());
    assert!(env.run(&["archive", "demo"]).is_err(), "archive refused while frozen");

    // roll back to the held snapshot: sources return, build dir and its marker survive, unfrozen
    env.run(&["rollback", "demo", "held"]).unwrap();
    assert!(env.p("demo/src/src0.txt").exists());
    assert_eq!(fs::read_to_string(env.p("demo/target/debug/marker")).unwrap(), "keep me");
    assert!(env.fake.is_subvolume(&env.p("demo/target")).unwrap());
    let st = env.state("demo");
    assert!(st.frozen.is_none(), "rollback to the good state unfreezes");
    let rec = env.unit("demo").read_record().unwrap().unwrap();
    assert_eq!(rec.uuid_history.len(), 1);
    assert!(env.snaps("demo").iter().any(|m| m.kind == SnapshotKind::Rollback), "pre-rollback state kept");
    assert!(
        fs::read_dir(&env.root).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(".bpm-rollback-"))
    );
}

#[test]
fn drip_deletion_and_unfreeze() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    let mut next = 0;
    let mut froze_at = None;
    for step in 1..=5 {
        for _ in 0..5 {
            fs::remove_file(env.p(&format!("demo/src/src{next}.txt"))).unwrap();
            next += 1;
        }
        env.advance(600);
        env.run(&["tick"]).unwrap();
        if env.state("demo").frozen.is_some() {
            froze_at = Some(step);
            break;
        }
    }
    // 41 files, 5 deleted per step: 12%, 24%, 37% -> trips on step 3
    assert_eq!(froze_at, Some(3));
    env.run(&["unfreeze", "demo"]).unwrap();
    let st = env.state("demo");
    assert!(st.frozen.is_none());
    assert_eq!(st.ref_stats.unwrap().files, 26);
    assert!(env.run(&["unfreeze", "demo"]).is_err());
}

#[test]
fn deleted_project_is_orphaned_and_recreated() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    fs::remove_dir_all(env.p("demo")).unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    let st = env.state("demo");
    assert_eq!(st.stage, Stage::Orphaned);
    assert!(st.frozen.is_some());
    assert!(env.snaps("demo").iter().all(|m| m.hold), "newest snapshot held");

    env.advance(86400 * 30);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.snaps("demo").len(), 1, "orphan snapshots never thinned");

    env.run(&["restore", "demo", "--recreate"]).unwrap();
    assert!(env.p("demo/src/src5.txt").exists());
    assert!(env.fake.is_subvolume(&env.p("demo")).unwrap());
    assert!(env.fake.is_subvolume(&env.p("demo/target")).unwrap(), "placeholder turned back into a nested subvolume");
    let st = env.state("demo");
    assert_eq!(st.stage, Stage::Active);
    assert!(st.frozen.is_none());
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert!(env.state("demo").last_error.is_none());
}

#[test]
fn restore_paths_takes_pre_restore_snapshot() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    fs::write(env.p("demo/src/src3.txt"), "broken").unwrap();
    assert!(env.run(&["restore", "demo", "1", "src/src3.txt"]).is_err(), "refuses to overwrite without --overwrite");
    env.run(&["restore", "demo", "1", "src/src3.txt", "--overwrite"]).unwrap();
    assert!(fs::read_to_string(env.p("demo/src/src3.txt")).unwrap().contains("lorem"));
    let kinds: Vec<SnapshotKind> = env.snaps("demo").iter().map(|m| m.kind).collect();
    assert!(kinds.contains(&SnapshotKind::PreRestore) && kinds.contains(&SnapshotKind::Post));
    // copying out elsewhere
    let out = env.base.join("out");
    fs::create_dir(&out).unwrap();
    env.run(&["restore", "demo", "1", "src/src4.txt", "--to", out.to_str().unwrap()]).unwrap();
    assert!(out.join("src4.txt").exists());
}

#[test]
fn build_dirs_recreated_and_converted() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    fs::write(env.p("demo/CMakeLists.txt"), "project(x)").unwrap();
    env.run(&["adopt", "demo"]).unwrap();
    assert!(env.fake.is_subvolume(&env.p("demo/build")).unwrap(), "primary cmake build dir precreated");
    assert!(!env.p("demo/cmake-build-debug").exists(), "non-primary banlist entries are not precreated");

    // `cargo clean`
    fs::remove_dir_all(env.p("demo/target")).unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert!(env.fake.is_subvolume(&env.p("demo/target")).unwrap(), "target recreated after cargo clean");

    // a build tool creates a plain, non-empty banned dir
    write_files(&env.p("demo/cmake-build-debug"), 3, "o");
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert!(
        !env.fake.is_subvolume(&env.p("demo/cmake-build-debug")).unwrap_or(false)
            || env.state("demo").pending_convert.is_empty()
    );
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert!(env.fake.is_subvolume(&env.p("demo/cmake-build-debug")).unwrap(), "converted by a heavy tick op");
    assert!(env.p("demo/cmake-build-debug/o0.txt").exists());
    assert!(env.state("demo").pending_convert.is_empty());
}

#[test]
fn rename_relinks_store_and_auto_adopt() {
    let env = Env::new("");
    fs::write(
        env.base.join("config.toml"),
        fs::read_to_string(env.base.join("config.toml")).unwrap().replace("adopt = \"manual\"", "adopt = \"auto\""),
    )
    .unwrap();
    make_rust_project(&env, "one");
    make_rust_project(&env, "two");
    fs::create_dir(env.p("datasets")).unwrap();
    env.run(&["tick"]).unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert!(
        env.unit("one").read_record().unwrap().is_some() && env.unit("two").read_record().unwrap().is_some(),
        "one adoption per tick"
    );
    assert!(env.unit("datasets").read_record().unwrap().is_none(), "ignored");

    fs::rename(env.p("one"), env.p("uno")).unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert!(env.unit("uno").read_record().unwrap().is_some());
    assert!(!env.unit("one").dir.exists());
    assert!(!env.snaps("uno").is_empty());
}

#[test]
fn idle_lifecycle_thins_collapses_and_recompresses() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    // a working day: a change every 30 minutes
    for i in 0..48 {
        fs::write(env.p("demo/src/src0.txt"), format!("edit {i}")).unwrap();
        env.advance(1800);
        env.run(&["tick"]).unwrap();
    }
    let busy = env.snaps("demo").len();
    // 48 half-hourly snapshots: all of the last 6h plus one per hour before that
    assert!((28..=34).contains(&busy), "{busy}");

    // two weeks idle: dormant, thinned but not collapsed
    env.advance(15 * 86400);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.state("demo").stage, Stage::Dormant);
    let dormant = env.snaps("demo").len();
    assert!(dormant > 1 && dormant < busy, "dormant keeps a thinned history ({dormant})");

    // two months idle: cold -> collapse (if retention has not already) -> recompress, one heavy op per tick
    env.fake.clear_ops();
    env.advance(50 * 86400);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.state("demo").stage, Stage::Cold);
    let snaps = env.snaps("demo");
    assert_eq!(
        snaps.len(),
        1,
        "collapsed to one snapshot: {:?}",
        snaps.iter().map(|m| (m.id, m.kind)).collect::<Vec<_>>()
    );
    for _ in 0..2 {
        env.advance(600);
        env.run(&["tick"]).unwrap();
    }
    let defrags = env.fake.ops().iter().filter(|o| o.starts_with("defrag -czstd Some(9)")).count();
    assert_eq!(defrags, 1, "{:?}", env.fake.ops());
    let st = env.state("demo");
    let rec = st.recompress.expect("recompress recorded");
    assert!(rec.skipped.is_none());
    assert_eq!(env.snaps("demo").len(), 1, "old snapshot replaced by the recompressed one");
    assert_eq!(st.stage, Stage::Cold, "recompression does not count as activity");

    // idempotent: nothing more happens
    env.advance(600);
    env.fake.clear_ops();
    env.run(&["tick"]).unwrap();
    assert!(!env.fake.ops().iter().any(|o| o.starts_with("defrag")));

    // touching the project reactivates it
    fs::write(env.p("demo/src/src1.txt"), "back to work").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.state("demo").stage, Stage::Active);
}

#[test]
fn hooks_can_veto_and_hold_protects() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    let hookdir = env.base.join("hooks/pre-snapshot");
    fs::create_dir_all(&hookdir).unwrap();
    let hook = hookdir.join("10-deny");
    fs::write(&hook, "#!/bin/sh\necho \"no snapshots for $BPM_PROJECT\" >&2\nexit 1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let err = env.run(&["snap", "demo"]).unwrap_err();
    assert_eq!(bpm::error::exit_code_for(&err), 8, "{err:#}");
    assert_eq!(env.snaps("demo").len(), 1);
    fs::remove_file(&hook).unwrap();

    env.run(&["snap", "demo", "--reason", "before experiment"]).unwrap();
    let id = env.snaps("demo").last().unwrap().id.to_string();
    assert!(env.run(&["rm", "demo", &id]).is_ok(), "manual rm of an unheld snapshot");
    env.run(&["snap", "demo"]).unwrap();
    let id = env.snaps("demo").last().unwrap().id.to_string();
    env.run(&["hold", "demo", &id]).unwrap();
    assert!(env.run(&["rm", "demo", &id]).is_err(), "held snapshot refuses rm");
    env.run(&["unhold", "demo", &id]).unwrap();
    env.run(&["rm", "demo", &id]).unwrap();
}

#[test]
fn archive_roundtrip() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    env.run(&["archive", "demo", "--level", "3", "--delete-live", "--yes"]).unwrap();
    assert!(!env.p("demo").exists());
    assert_eq!(env.state("demo").stage, Stage::Archived);
    env.advance(3600);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.state("demo").stage, Stage::Archived, "archived projects are not orphaned");
    let archives: Vec<_> = fs::read_dir(env.base.join("archives/demo"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        archives.iter().any(|n| n.ends_with(".btrfs.zst")) && archives.iter().any(|n| n.ends_with(".manifest.toml"))
    );
    env.run(&["unarchive", "demo"]).unwrap();
    assert!(env.p("demo/src/src7.txt").exists());
    assert!(env.fake.is_subvolume(&env.p("demo/target")).unwrap());
    assert!(env.snaps("demo").iter().any(|m| m.kind == SnapshotKind::Received && m.hold));
    assert_eq!(env.state("demo").stage, Stage::Active);
}

#[test]
fn diff_and_config_and_status_run() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    fs::write(env.p("demo/src/new.txt"), "n").unwrap();
    env.run(&["diff", "demo", "1"]).unwrap();
    env.run(&["config", "demo", "--show-origin"]).unwrap();
    env.run(&["status"]).unwrap();
    env.run(&["status", "demo"]).unwrap();
    env.run(&["list", "demo"]).unwrap();
    env.run(&["doctor"]).unwrap();
    let ctx = env.ctx();
    let found = bpm::ops::snap::projects_in_command(
        &ctx,
        &env.p("demo/src"),
        &format!("rm -rf ../../other {}/x", env.p("demo").display()),
    );
    assert_eq!(found.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(), vec!["demo", "other"]);
}

#[test]
fn adopt_keeps_banned_names_that_are_not_directories() {
    let env = Env::new("");
    let p = env.p("links");
    write_files(&p.join("src"), 3, "s");
    fs::write(p.join("Cargo.toml"), "").unwrap();
    std::os::unix::fs::symlink("/nonexistent/target-elsewhere", p.join("target")).unwrap();
    fs::write(p.join("build"), "a file named like a build dir").unwrap();
    fs::write(p.join("CMakeLists.txt"), "").unwrap();
    env.run(&["adopt", "links"]).unwrap();
    assert_eq!(fs::read_link(env.p("links/target")).unwrap(), PathBuf::from("/nonexistent/target-elsewhere"));
    assert_eq!(fs::read_to_string(env.p("links/build")).unwrap(), "a file named like a build dir");
}

#[test]
fn rollback_dropping_build_dirs_keeps_user_subvolumes() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    // a user-made nested subvolume whose path is a regular directory in the snapshot
    fs::create_dir_all(env.p("demo/data")).unwrap();
    fs::write(env.p("demo/data/keep"), "snapshot content").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    let with_data_dir = env.snaps("demo").last().unwrap().id.to_string();
    fs::remove_dir_all(env.p("demo/data")).unwrap();
    env.fake.create_subvolume(&env.p("demo/data")).unwrap();
    fs::write(env.p("demo/data/precious"), "only copy").unwrap();
    env.run(&["rollback", "demo", &with_data_dir, "--drop-build-dirs"]).unwrap();
    let leftovers: Vec<PathBuf> = fs::read_dir(&env.root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().contains(".bpm-rollback-"))
        .collect();
    assert_eq!(leftovers.len(), 1, "old tree kept because a user subvolume could not be moved");
    assert_eq!(fs::read_to_string(leftovers[0].join("data/precious")).unwrap(), "only copy");
    assert!(!env.p("demo/target/debug").exists(), "dropped build dir");
}

#[test]
fn archive_delete_live_refuses_unprotected_subvolumes() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    env.fake.create_subvolume(&env.p("demo/vendor-data")).unwrap();
    fs::write(env.p("demo/vendor-data/x"), "not in any snapshot").unwrap();
    let err = env.run(&["archive", "demo", "--level", "1", "--delete-live", "--yes"]).unwrap_err();
    assert_eq!(bpm::error::exit_code_for(&err), 7, "{err:#}");
    assert!(env.p("demo/vendor-data/x").exists());
    assert_eq!(env.state("demo").archives.len(), 1, "archive itself was recorded");
}
