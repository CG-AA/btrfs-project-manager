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
    let unit = env.unit("demo").lock(Duration::ZERO).unwrap();
    let mut m = env.snaps("demo")[0].clone();
    m.stats.as_mut().unwrap().walk_ms = 5000; // pretend counting is slow
    unit.update_meta(&m).unwrap();
    let mut st = env.state("demo");
    st.last_stats_at = Some(env.clock_now());
    unit.write_state(&st).unwrap();
    drop(unit);
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

// ---------------- nothing is deleted without proof ----------------

/// An edit saved while adopt copies a large build directory (after its first verification).
#[test]
fn adopt_edit_during_build_copy_is_not_lost() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    write_files(&env.p("demo/target/debug"), 4000, "big");
    env.advance(10);
    let root = env.root.clone();
    let writer = std::thread::spawn(move || {
        let marker = root.join("demo.bpm-tmp/target");
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(60) {
            if marker.exists() {
                fs::write(root.join("demo/src/src0.txt"), "EDIT DURING ADOPT").unwrap();
                return true;
            }
            std::thread::sleep(Duration::from_micros(50));
        }
        false
    });
    let r = env.run(&["adopt", "demo"]);
    assert!(writer.join().unwrap(), "writer ran");
    let live = fs::read_to_string(env.p("demo/src/src0.txt")).unwrap();
    assert_eq!(live, "EDIT DURING ADOPT", "adopt result: {:?}", r.err());
    assert!(root_entries(&env, ".bpm-").is_empty(), "{:?}", root_entries(&env, ".bpm-"));
}

/// A write into the original tree after the swap (a shell whose working directory is inside).
#[test]
fn adopt_keeps_original_written_after_swap() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.advance(10);
    install_hook(
        &env,
        "post-snapshot",
        "10-late",
        "[ \"$BPM_SNAPSHOT_KIND\" = adopt ] && echo late > \"$BPM_PROJECT_PATH.bpm-old/src/late.txt\"",
    );
    env.run(&["adopt", "demo"]).unwrap();
    let kept = root_entries(&env, ".bpm-keep-adopt-");
    assert_eq!(kept.len(), 1, "{:?}", root_entries(&env, ".bpm-"));
    assert_eq!(fs::read_to_string(kept[0].join("src/late.txt")).unwrap(), "late\n");
}

/// Hardlinks between top-level entries are split by the per-entry copy; adoption still verifies.
#[test]
fn adopt_with_hardlinks_across_entries() {
    let env = Env::new("");
    make_project(&env, "demo", 5);
    fs::create_dir_all(env.p("demo/bin")).unwrap();
    fs::hard_link(env.p("demo/src/src0.txt"), env.p("demo/bin/tool")).unwrap();
    env.advance(10);
    env.run(&["adopt", "demo"]).unwrap();
    assert!(env.fake.is_subvolume(&env.p("demo")).unwrap());
    assert!(env.p("demo/bin/tool").exists());
}

/// A write that reaches the old tree after rollback's safety snapshot.
#[test]
fn rollback_keeps_previous_tree_written_after_safety_snapshot() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    env.advance(600);
    fs::write(env.p("demo/src/extra.txt"), "x").unwrap();
    env.run(&["snap", "demo"]).unwrap();
    install_hook(
        &env,
        "post-snapshot",
        "10-late",
        "[ \"$BPM_SNAPSHOT_KIND\" = rollback ] && echo late > \"$BPM_PROJECT_PATH/src/late.txt\"",
    );
    env.run(&["rollback", "demo", "1"]).unwrap();
    let kept = root_entries(&env, ".bpm-keep-rollback-");
    assert_eq!(kept.len(), 1, "{:?}", root_entries(&env, ".bpm-"));
    assert_eq!(fs::read_to_string(kept[0].join("src/late.txt")).unwrap(), "late\n");
    assert!(!env.p("demo/src/extra.txt").exists(), "rolled back");
    let rec = env.unit("demo").read_record().unwrap().unwrap();
    assert_eq!(rec.uuid, env.fake.subvol_info(&env.p("demo")).unwrap().uuid, "record follows the new subvolume");
}

/// An in-place edit that has not been flushed must not make rollback refuse as "identical".
#[test]
fn rollback_sees_unflushed_changes() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    fs::write(env.p("demo/src/src0.txt"), "corrupted by an agent").unwrap();
    env.run(&["rollback", "demo", "latest"]).unwrap();
    assert!(fs::read_to_string(env.p("demo/src/src0.txt")).unwrap().contains("lorem"));
}

/// Archive with --delete-live: an edit during the (long) compression keeps the live project.
#[test]
fn archive_delete_live_refuses_after_concurrent_edit() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    install_hook(&env, "pre-archive", "10-edit", "echo late > \"$BPM_PROJECT_PATH/src/late.txt\"");
    let err =
        env.run(&["archive", "demo", "--level", "3", "--delete-live", "--delete-snapshots", "--yes"]).unwrap_err();
    assert_eq!(bpm::error::exit_code_for(&err), 7, "{err:#}");
    assert!(env.p("demo/src/late.txt").exists());
    assert!(!env.snaps("demo").is_empty());
}

/// Archive of an older snapshot with --delete-live would delete newer work.
#[test]
fn archive_delete_live_refuses_older_snapshot() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.advance(10);
    env.run(&["adopt", "demo"]).unwrap();
    fs::write(env.p("demo/src/new-work.txt"), "new work").unwrap();
    env.advance(60);
    assert!(env.run(&["archive", "demo", "--snapshot", "1", "--level", "1", "--delete-live", "--yes"]).is_err());
    assert!(env.p("demo/src/new-work.txt").exists());
}

/// restore --overwrite of a directory must not destroy a nested subvolume inside it.
#[test]
fn restore_overwrite_keeps_nested_subvolume() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.advance(10);
    env.run(&["adopt", "demo"]).unwrap();
    fs::create_dir_all(env.p("demo/data")).unwrap();
    fs::write(env.p("demo/data/keep"), "snap content").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    let id = env.snaps("demo").last().unwrap().id.to_string();
    env.fake.create_subvolume(&env.p("demo/data/db")).unwrap();
    fs::write(env.p("demo/data/db/precious"), "only copy").unwrap();
    env.advance(600);
    env.run(&["restore", "demo", &id, "data", "--overwrite"]).unwrap();
    assert_eq!(fs::read_to_string(env.p("demo/data/keep")).unwrap(), "snap content");
    let found = walkdir::WalkDir::new(&env.root).into_iter().flatten().any(|e| e.file_name() == "precious");
    assert!(found, "the nested subvolume's data survives");
}

/// A user change during recompression keeps the pre-recompress snapshot and counts as activity.
#[test]
fn recompress_keeps_old_snapshot_after_concurrent_change() {
    let env = Env::new("");
    make_project(&env, "demo", 50);
    fs::write(env.p("demo/important.txt"), "only copy").unwrap();
    env.run(&["adopt", "demo"]).unwrap();
    env.advance(61 * 86400);
    for _ in 0..3 {
        env.run(&["tick"]).unwrap();
        env.advance(600);
    }
    let mut st = env.state("demo");
    st.recompress = None;
    env.unit("demo").lock(Duration::ZERO).unwrap().write_state(&st).unwrap();
    let hook = install_hook(&env, "pre-recompress", "10-user", "rm -f \"$BPM_PROJECT_PATH/important.txt\"");
    env.run(&["tick"]).unwrap();
    fs::remove_file(hook).unwrap();
    assert!(env.fake.ops().iter().any(|o| o.starts_with("defrag")), "recompress ran");
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert!(any_snap_has(&env, "demo", "important.txt"), "{}", describe(&env, "demo"));
    assert_eq!(env.state("demo").stage, Stage::Active);
}

// ---------------- no path resolution through symlinks ----------------

fn victim_files(env: &Env, dir: &str) -> Vec<String> {
    let mut v: Vec<String> = walkdir::WalkDir::new(env.p(dir))
        .min_depth(1)
        .into_iter()
        .flatten()
        .map(|e| e.path().strip_prefix(env.p(dir)).unwrap().display().to_string())
        .collect();
    v.sort();
    v
}

/// Adopt with a banlist entry below a symlink must not delete the link target.
#[test]
fn adopt_banlist_entry_under_symlink_leaves_target_alone() {
    let env = Env::new("");
    write_files(&env.p("victim/secret"), 3, "v");
    let before = victim_files(&env, "victim");
    let p = env.p("proj");
    write_files(&p.join("src"), 10, "s");
    std::os::unix::fs::symlink("../victim", p.join("x")).unwrap();
    fs::write(p.join(".bpm.toml"), "banlist_add = [\"x/secret\"]\n").unwrap();
    env.advance(3600);
    env.run(&["adopt", "proj"]).unwrap();
    assert_eq!(victim_files(&env, "victim"), before);
    assert!(!env.fake.is_subvolume(&env.p("victim/secret")).unwrap());
}

/// The same with a build dir from the config reached through a symlink to shared data.
#[test]
fn adopt_banlist_symlinked_parent_keeps_shared_data() {
    let env = Env::new("[projects.demo]\nbanlist_add = [\"web/node_modules\"]\n");
    make_rust_project(&env, "demo");
    let shared = env.base.join("shared/web/node_modules/pkg");
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join("local-patch.js"), "hand written").unwrap();
    std::os::unix::fs::symlink("../../shared/web", env.p("demo/web")).unwrap();
    env.advance(10);
    env.run(&["adopt", "demo"]).unwrap();
    assert!(shared.join("local-patch.js").exists());
    assert!(!env.fake.is_subvolume(&env.base.join("shared/web/node_modules")).unwrap());
}

/// Tick banlist enforcement and conversion must not create, replace or move through a symlink.
#[test]
fn tick_banlist_does_not_follow_symlinks() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    fs::create_dir_all(env.p("victim/emptydir")).unwrap();
    write_files(&env.p("victim/data"), 3, "v");
    let before = victim_files(&env, "victim");
    std::os::unix::fs::symlink("../victim", env.p("demo/s")).unwrap();
    fs::write(
        env.p("demo/.bpm.toml"),
        "banlist_add = [\"s/evil\", \"s/emptydir\", \"s/data\"]\n[policy]\nkeep_build_on_adopt = false\nbanlist_settle = \"0s\"\n",
    )
    .unwrap();
    for _ in 0..2 {
        env.advance(3600);
        env.run(&["tick"]).unwrap();
    }
    assert_eq!(victim_files(&env, "victim"), before);
    for n in ["victim/emptydir", "victim/data"] {
        assert!(!env.fake.is_subvolume(&env.p(n)).unwrap(), "{n}");
    }
    assert!(env.state("demo").pending_convert.is_empty());
}

/// restore into a directory that became a symlink must not overwrite the link target.
#[test]
fn restore_refuses_symlinked_parent() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    write_files(&env.p("demo/web/config"), 2, "snapcfg");
    env.advance(600);
    env.run(&["snap", "demo"]).unwrap();
    let id = env.snaps("demo").last().unwrap().id.to_string();
    write_files(&env.p("victim/config"), 2, "victimcfg");
    let before = victim_files(&env, "victim");
    fs::remove_dir_all(env.p("demo/web")).unwrap();
    std::os::unix::fs::symlink("../victim", env.p("demo/web")).unwrap();
    env.advance(600);
    let err = env.run(&["restore", "demo", &id, "web/config", "--overwrite"]).unwrap_err();
    assert_eq!(bpm::error::exit_code_for(&err), 7, "{err:#}");
    assert_eq!(victim_files(&env, "victim"), before);
    // single files too
    let outside = env.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("app.toml"), "system file outside project").unwrap();
    fs::remove_file(env.p("demo/web")).unwrap();
    std::os::unix::fs::symlink(&outside, env.p("demo/web")).unwrap();
    assert!(env.run(&["restore", "demo", &id, "web/config/snapcfg0.txt", "--overwrite"]).is_err());
    assert_eq!(fs::read_to_string(outside.join("app.toml")).unwrap(), "system file outside project");
}

// ---------------- store and tick robustness ----------------

/// `mv app app-old && mv app-v2 app`: each store unit keeps its own project.
#[test]
fn rename_chain_keeps_each_history_with_its_project() {
    let env = Env::new("");
    make_rust_project(&env, "app");
    make_rust_project(&env, "app-v2");
    env.advance(10);
    env.run(&["adopt", "app"]).unwrap();
    env.run(&["adopt", "app-v2"]).unwrap();
    let ua = env.unit("app").read_record().unwrap().unwrap().uuid;
    let ub = env.unit("app-v2").read_record().unwrap().unwrap().uuid;
    fs::rename(env.p("app"), env.p("app-old")).unwrap();
    fs::rename(env.p("app-v2"), env.p("app")).unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    let labels = [(ua, "A"), (ub, "B")];
    let mut units = env.units_by_uuid(&labels);
    units.sort();
    assert_eq!(
        units,
        vec![("app".into(), "B".into(), vec!["B".into()]), ("app-old".into(), "A".into(), vec!["A".into()])],
    );
    fs::write(env.p("app/src/src0.txt"), "v2 edit").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.snaps("app").len(), 2, "the renamed project is still snapshotted");
}

/// Swapping two project names.
#[test]
fn rename_swap_keeps_each_history_with_its_project() {
    let env = Env::new("");
    make_rust_project(&env, "a");
    make_rust_project(&env, "b");
    env.advance(10);
    env.run(&["adopt", "a"]).unwrap();
    env.run(&["adopt", "b"]).unwrap();
    let ua = env.unit("a").read_record().unwrap().unwrap().uuid;
    let ub = env.unit("b").read_record().unwrap().unwrap().uuid;
    fs::rename(env.p("a"), env.p("t")).unwrap();
    fs::rename(env.p("b"), env.p("a")).unwrap();
    fs::rename(env.p("t"), env.p("b")).unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    let mut units = env.units_by_uuid(&[(ua, "A"), (ub, "B")]);
    units.sort();
    assert_eq!(units, vec![("a".into(), "B".into(), vec!["B".into()]), ("b".into(), "A".into(), vec!["A".into()])]);
    let rec = env.unit("a").read_record().unwrap().unwrap();
    assert_eq!(rec.path, env.p("a"));
}

/// A long `btrfs receive` holds the unit lock; tick cleanup must not delete its tmp dir.
#[test]
fn cleanup_skips_units_that_are_busy() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.advance(10);
    env.run(&["adopt", "demo"]).unwrap();
    let unit = env.unit("demo");
    let lock = unit.lock(Duration::from_secs(1)).unwrap();
    let tmp = unit.dir.join("9.tmp");
    fs::create_dir(&tmp).unwrap();
    env.fake.create_subvolume(&tmp.join("snapshot")).unwrap();
    fs::write(tmp.join("snapshot/receiving"), "x").unwrap();
    fs::File::open(&tmp).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(7200)).unwrap();
    env.advance(600);
    env.run(&["tick", "--no-heavy"]).unwrap();
    assert!(tmp.join("snapshot/receiving").exists());
    drop(lock);
}

/// A directory whose adoption is always refused backs off and does not starve collapse.
#[test]
fn refused_adoption_does_not_starve_collapse() {
    let env = Env::new("[defaults.lifecycle]\ncold_after = \"20d\"\n");
    make_project(&env, "demo", 20);
    env.run(&["adopt", "demo"]).unwrap();
    fs::write(env.p("demo/src/src0.txt"), "x").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    env.auto_adopt();
    fs::create_dir_all(env.p("vm")).unwrap();
    fs::write(env.p("vm/readme"), "x").unwrap();
    env.fake.create_subvolume(&env.p("vm/disk")).unwrap();
    env.advance(21 * 86400);
    for _ in 0..6 {
        env.run(&["tick"]).ok();
        env.advance(300);
    }
    assert_eq!(env.snaps("demo").len(), 1, "{}", describe(&env, "demo"));
}

/// `tick --heavy-only` runs the heavy operation the light tick found, and `--no-heavy` does not.
#[test]
fn heavy_operations_run_in_their_own_pass() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    write_files(&env.p("demo/cmake-build-debug"), 3, "o");
    fs::write(env.p("demo/CMakeLists.txt"), "").unwrap();
    env.advance(600);
    env.run(&["tick", "--no-heavy"]).unwrap();
    assert!(!env.state("demo").pending_convert.is_empty());
    env.advance(600);
    env.run(&["tick", "--no-heavy"]).unwrap();
    assert!(!env.fake.is_subvolume(&env.p("demo/cmake-build-debug")).unwrap());
    env.run(&["tick", "--heavy-only"]).unwrap();
    assert!(env.fake.is_subvolume(&env.p("demo/cmake-build-debug")).unwrap());
    assert!(env.p("demo/cmake-build-debug/o0.txt").exists());
}

/// Snapshots still happen while a heavy operation holds its lock.
#[test]
fn light_tick_runs_while_heavy_lock_is_held() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    let heavy = env.store().heavy_lock().unwrap();
    fs::write(env.p("demo/src/src0.txt"), "edit").unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    assert_eq!(env.snaps("demo").len(), 2);
    drop(heavy);
}

/// A subvolume at the top level (for example after `bpm forget`) is adopted again.
#[test]
fn foreign_subvolume_is_auto_adopted() {
    let env = Env::new("");
    env.auto_adopt();
    env.fake.create_subvolume(&env.p("newproj")).unwrap();
    write_files(&env.p("newproj/src"), 3, "s");
    env.advance(3600);
    env.run(&["tick"]).unwrap();
    assert!(env.unit("newproj").read_record().unwrap().is_some());
    assert!(!env.snaps("newproj").is_empty());
}

// ---------------- CLI, config and docs ----------------

/// `doctor --fix --dry-run` must not delete or rename leftovers.
#[test]
fn doctor_fix_dry_run_changes_nothing() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    write_files(&env.p("demo.bpm-old/src"), 2, "only-here");
    let mut ctx = env.ctx();
    ctx.opts.dry_run = true;
    ctx.btrfs = std::sync::Arc::new(bpm::btrfs::DryRunBtrfs(env.fake.clone()));
    ctx.fs = std::sync::Arc::new(bpm::util::fs::DryRunFs);
    let cli = <bpm::cli::Cli as clap::Parser>::try_parse_from(["bpm", "doctor", "--fix"]).unwrap();
    bpm::ops::dispatch(&ctx, cli.cmd).unwrap();
    assert!(env.p("demo.bpm-old/src/only-here0.txt").exists());
}

/// `rm --container 5 6` means two container snapshots; a project name is refused.
#[test]
fn rm_container_takes_only_snapshot_ids() {
    let env = Env::new("");
    fs::write(env.p("loose.txt"), "x").unwrap();
    env.run(&["snap", "--container"]).unwrap();
    fs::write(env.p("loose.txt"), "y").unwrap();
    env.run(&["snap", "--container"]).unwrap();
    let ids: Vec<String> = env.store().container().snapshots().unwrap().iter().map(|m| m.id.to_string()).collect();
    assert_eq!(ids.len(), 2);
    let err = env.run(&["rm", "myproj", &ids[0], "--container"]).unwrap_err();
    assert_eq!(bpm::error::exit_code_for(&err), 2, "{err:#}");
    env.run(&["rm", "--container", &ids[0], &ids[1]]).unwrap();
    assert!(env.store().container().snapshots().unwrap().is_empty());
}

/// `bpm --dry-run wrap -- cmd` neither snapshots nor runs the command.
#[test]
fn wrap_dry_run_runs_nothing() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    let mut ctx = env.ctx();
    ctx.opts.dry_run = true;
    let marker = env.base.join("ran");
    let cli = <bpm::cli::Cli as clap::Parser>::try_parse_from([
        "bpm",
        "wrap",
        "--project",
        "demo",
        "--",
        "touch",
        marker.to_str().unwrap(),
    ])
    .unwrap();
    bpm::ops::dispatch(&ctx, cli.cmd).unwrap();
    assert!(!marker.exists());
    assert_eq!(env.snaps("demo").len(), 1);
}

/// Restoring paths into a project whose directory is gone refuses instead of creating a stub.
#[test]
fn restore_paths_into_missing_project_refuses() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    fs::remove_dir_all(env.p("demo")).unwrap();
    env.advance(600);
    env.run(&["tick"]).unwrap();
    let err = env.run(&["restore", "demo", "latest", "src/src1.txt"]).unwrap_err();
    assert!(format!("{err:#}").contains("--recreate"), "{err:#}");
    assert!(!env.p("demo").exists(), "no stub directory");
    env.run(&["restore", "demo", "--recreate"]).unwrap();
    assert!(env.p("demo/src/src1.txt").exists());
}

/// An archive made from a received snapshot can be unarchived again.
#[test]
fn archive_of_received_snapshot_roundtrips() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    env.run(&["archive", "demo", "--level", "3", "--delete-live", "--delete-snapshots", "--yes"]).unwrap();
    env.run(&["unarchive", "demo", "--no-live"]).unwrap();
    let received = env.snaps("demo").iter().find(|m| m.kind == SnapshotKind::Received).unwrap().id.to_string();
    env.advance(5);
    env.run(&["archive", "demo", "--snapshot", &received, "--level", "3"]).unwrap();
    let newest = fs::read_dir(env.base.join("archives/demo"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".btrfs.zst"))
        .max_by_key(|p| fs::metadata(p).unwrap().modified().unwrap())
        .unwrap();
    env.run(&["unarchive", "demo", newest.to_str().unwrap()]).unwrap();
    assert!(env.p("demo/src/src7.txt").exists());
}

/// A manifest cannot place the project outside the root.
#[test]
fn unarchive_refuses_manifest_path_escape() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    env.run(&["archive", "demo", "--level", "3"]).unwrap();
    let zst = fs::read_dir(env.base.join("archives/demo"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with(".btrfs.zst"))
        .unwrap();
    let mpath = bpm::mechanics::archive::manifest_path(&zst);
    let m = fs::read_to_string(&mpath).unwrap().replace("project = \"demo\"", "project = \"../escape\"");
    fs::write(&mpath, m).unwrap();
    assert!(env.run(&["unarchive", "pwn", zst.to_str().unwrap()]).is_err());
    assert!(!env.base.join("escape").exists());
}

/// `.bpm.toml` cannot switch off the shrink guard or empty the retention windows.
#[test]
fn project_file_cannot_disable_guard_or_retention() {
    let env = Env::new("");
    make_rust_project(&env, "demo");
    env.run(&["adopt", "demo"]).unwrap();
    for i in 0..5 {
        fs::write(env.p(&format!("demo/src/src{i}.txt")), format!("changed {i} {}", "more content ".repeat(40)))
            .unwrap();
        env.advance(900);
        env.run(&["tick"]).unwrap();
    }
    let before = env.snaps("demo").len();
    fs::write(
        env.p("demo/.bpm.toml"),
        "[policy.shrink_guard]\nenabled=false\n[policy.thin]\nkeep_all=\"0s\"\nhourly=\"0s\"\ndaily=\"0s\"\nweekly=\"0s\"\nsafety_ttl=\"0s\"\n",
    )
    .unwrap();
    for i in 0..40 {
        let _ = fs::remove_file(env.p(&format!("demo/src/src{i}.txt")));
    }
    env.advance(900);
    env.run(&["tick"]).unwrap();
    assert!(env.state("demo").frozen.is_some());
    assert!(env.snaps("demo").len() > before, "{}", describe(&env, "demo"));
}

/// `bpm setup --print-claude-hook` only prints and needs no root.
#[test]
fn print_claude_hook_needs_no_root() {
    let cli = <bpm::cli::Cli as clap::Parser>::try_parse_from(["bpm", "setup", "--print-claude-hook"]).unwrap();
    assert!(!cli.cmd.needs_root());
}
