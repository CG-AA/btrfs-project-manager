//! Regression tests for the review findings, one per failure scenario, on the fake backend.

mod common;
use common::*;

// ---------------- shrink guard sees every snapshot ----------------

/// A hook snapshot without stats captures `rm -rf src`; ticks must still freeze and keep the
/// pre-wipe state instead of thinning it away.
#[test]
fn hook_snapshot_of_wipe_is_guarded_and_good_state_kept() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.advance(10);
    env.run(&["adopt", "demo"]).unwrap();
    for i in 0..30 {
        // same size as the original, so the edits themselves never look like a shrink
        fs::write(env.p(&format!("demo/src/src{}.txt", i % 40)), format!("edit {i:03} {}", "more content ".repeat(40)))
            .unwrap();
        env.advance(600);
        env.run(&["tick"]).unwrap();
    }
    assert!(env.state("demo").frozen.is_none(), "{}", describe(&env, "demo"));
    fs::write(env.p("demo/src/final.txt"), "final work").unwrap();
    env.advance(600);
    hook_snap(&env, "demo", true);
    let good = env.snaps("demo").last().unwrap().id;
    assert!(snap_has(&env, "demo", good, "src/final.txt"));
    fs::remove_dir_all(env.p("demo/src")).unwrap();
    env.advance(60);
    env.run(&["tick"]).unwrap();
    env.advance(120);
    hook_snap(&env, "demo", false);
    let wiped = env.snaps("demo").last().unwrap().id;
    assert!(wiped > good, "{}", describe(&env, "demo"));
    for _ in 0..(3 * 24) {
        env.advance(3600);
        env.run(&["tick"]).unwrap();
    }
    let st = env.state("demo");
    let frozen = st.frozen.expect("the wipe froze the project");
    assert_eq!(frozen.ref_snap, Some(good), "{}", describe(&env, "demo"));
    for _ in 0..(17 * 4) {
        env.advance(6 * 3600);
        env.run(&["tick"]).unwrap();
    }
    assert!(env.snaps("demo").iter().any(|m| m.id == good && m.hold), "{}", describe(&env, "demo"));
    env.run(&["rollback", "demo", "held"]).unwrap();
    assert_eq!(fs::read_to_string(env.p("demo/src/final.txt")).unwrap(), "final work");
}

/// Large trees are counted at most every 30 minutes, so a tick snapshot may have no stats.
#[test]
fn tick_snapshot_without_stats_is_guarded_before_thinning() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.advance(10);
    env.run(&["adopt", "demo"]).unwrap();
    let unit = env.unit("demo");
    let mut m = env.snaps("demo")[0].clone();
    m.stats.as_mut().unwrap().walk_ms = 5000; // pretend counting is slow
    unit.update_meta(&m).unwrap();
    let mut st = env.state("demo");
    st.last_stats_at = Some(env.clock_now());
    unit.write_state(&st).unwrap();
    fs::write(env.p("demo/src/src1.txt"), "edit").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    let edit = env.snaps("demo").last().unwrap().id;
    for i in 0..35 {
        fs::remove_file(env.p(&format!("demo/src/src{i}.txt"))).unwrap();
    }
    env.advance(600);
    env.run(&["tick"]).unwrap();
    for _ in 0..24 {
        env.advance(3600);
        env.run(&["tick"]).unwrap();
    }
    let st = env.state("demo");
    assert!(st.frozen.is_some(), "{}", describe(&env, "demo"));
    assert!(
        env.snaps("demo").iter().any(|m| m.id == edit),
        "last edit before the loss kept: {}",
        describe(&env, "demo")
    );
}

/// The guard holds the last snapshot that passes, not the stats-less one that already has the loss.
#[test]
fn guard_holds_verified_good_snapshot_not_previous_id() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.advance(10);
    env.run(&["adopt", "demo"]).unwrap();
    env.advance(600);
    fs::write(env.p("demo/src/src39.txt"), "edit").unwrap();
    hook_snap(&env, "demo", true);
    let good = env.snaps("demo").last().unwrap().id;
    for i in 0..35 {
        fs::remove_file(env.p(&format!("demo/src/src{i}.txt"))).unwrap();
    }
    env.advance(180);
    // the hook snapshot is taken without the project lock here, so nothing evaluates it yet
    let unit = env.unit("demo");
    let _l = unit.lock(Duration::from_secs(1)).unwrap();
    hook_snap(&env, "demo", false);
    drop(_l);
    let wiped = env.snaps("demo").last().unwrap().id;
    assert!(wiped > good);
    env.advance(400);
    fs::write(env.p("demo/src/src38.txt"), "edit2").unwrap();
    env.run(&["tick"]).unwrap();
    let st = env.state("demo");
    assert_eq!(st.frozen.as_ref().and_then(|f| f.ref_snap), Some(good), "{}", describe(&env, "demo"));
    let held: Vec<u64> = env.snaps("demo").iter().filter(|m| m.hold).map(|m| m.id).collect();
    assert!(held.contains(&good), "{held:?}");
    env.run(&["rollback", "demo", "held"]).unwrap();
    assert!(env.p("demo/src/src0.txt").exists());
}

/// While frozen, a second shrink holds another (damaged) snapshot; `held` still means the last
/// good one.
#[test]
fn held_selector_prefers_the_freeze_reference() {
    let env = Env::new("");
    make_project(&env, "demo", 100);
    fs::create_dir_all(env.p("demo/b")).unwrap();
    write_files(&env.p("demo/b"), 100, "b");
    env.advance(10);
    env.run(&["adopt", "demo"]).unwrap();
    fs::remove_dir_all(env.p("demo/src")).unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.state("demo").frozen.as_ref().and_then(|f| f.ref_snap), Some(1));
    fs::remove_dir_all(env.p("demo/b")).unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    env.run(&["rollback", "demo", "held"]).unwrap();
    assert!(env.p("demo/src/src0.txt").exists(), "rolled back to #1, not the half-deleted #2");
    assert!(env.p("demo/b/b0.txt").exists());
}

/// A cold project whose newest (and only) snapshot has no stats is still collapsed and
/// recompressed.
#[test]
fn cold_project_with_uncounted_newest_is_recompressed() {
    let env = Env::new("");
    make_project(&env, "demo", 50);
    env.run(&["adopt", "demo"]).unwrap();
    env.advance(3 * 86400);
    fs::write(env.p("demo/src/src0.txt"), "edit").unwrap();
    hook_snap(&env, "demo", true);
    for _ in 0..(65 * 4) {
        env.advance(6 * 3600);
        env.run(&["tick"]).unwrap();
    }
    assert_eq!(env.state("demo").stage, Stage::Cold);
    assert!(env.state("demo").recompress.is_some(), "{}", describe(&env, "demo"));
    assert!(env.fake.ops().iter().any(|o| o.starts_with("defrag")));
}

/// Counting exceeds the budget: collapse must not take a new snapshot every tick.
#[test]
fn incomplete_stats_do_not_loop_collapse() {
    let env = Env::new("[defaults.lifecycle]\ncold_after = \"20d\"\n");
    make_project(&env, "demo", 4200);
    env.run(&["adopt", "demo"]).unwrap();
    fs::write(env.p("demo/src/src0.txt"), "x").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    // from now on every walk exceeds the budget
    let cfg = env.base.join("config.toml");
    fs::write(&cfg, fs::read_to_string(&cfg).unwrap() + "[defaults.snapshot]\nstats_budget = \"0s\"\n").unwrap();
    fs::write(env.p("demo/src/src1.txt"), "y").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    env.advance(21 * 86400);
    env.run(&["tick"]).unwrap();
    let after_first = env.snaps("demo").len();
    for _ in 0..6 {
        env.advance(300);
        env.run(&["tick"]).unwrap();
    }
    assert_eq!(env.snaps("demo").len(), after_first, "{}", describe(&env, "demo"));
}

/// Unfreeze makes the current (hand-restored) state the reference, so a second deletion trips.
#[test]
fn unfreeze_uses_current_state_as_reference() {
    let env = Env::new("");
    make_project(&env, "demo", 200);
    env.run(&["adopt", "demo"]).unwrap();
    env.advance(600);
    let backup = env.base.join("backup-src");
    assert!(std::process::Command::new("cp").arg("-a").arg(env.p("demo/src")).arg(&backup).status().unwrap().success());
    fs::remove_dir_all(env.p("demo/src")).unwrap();
    env.run(&["tick"]).unwrap();
    assert!(env.state("demo").frozen.is_some());
    env.advance(600);
    assert!(std::process::Command::new("cp").arg("-a").arg(&backup).arg(env.p("demo/src")).status().unwrap().success());
    hook_snap(&env, "demo", true);
    env.run(&["unfreeze", "demo"]).unwrap();
    assert!(env.state("demo").ref_stats.unwrap().files >= 200);
    env.advance(3 * 3600);
    env.run(&["tick"]).unwrap();
    fs::remove_dir_all(env.p("demo/src")).unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert!(env.state("demo").frozen.is_some(), "{}", describe(&env, "demo"));
}

// ---------------- activity is measured from observed counters ----------------

/// A change captured by a hook snapshot before the tick sees it still reactivates a cold
/// project, so collapse does not delete the only pre-edit snapshot.
#[test]
fn hook_snapshot_change_counts_as_activity() {
    let env = Env::new("");
    make_project(&env, "demo", 50);
    env.run(&["adopt", "demo"]).unwrap();
    fs::write(env.p("demo/src/src0.txt"), "precious original").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    env.advance(61 * 86400);
    for _ in 0..4 {
        env.run(&["tick"]).unwrap();
        env.advance(600);
    }
    assert_eq!(env.state("demo").stage, Stage::Cold);
    fs::write(env.p("demo/src/src0.txt"), "garbage").unwrap();
    env.advance(60);
    hook_snap(&env, "demo", false);
    env.advance(240);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.state("demo").stage, Stage::Active);
    let precious = env
        .snaps("demo")
        .iter()
        .any(|m| snap_read(&env, "demo", m.id, "src/src0.txt").as_deref() == Some("precious original"));
    assert!(precious, "{}", describe(&env, "demo"));
}
