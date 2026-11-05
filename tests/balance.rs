//! A `btrfs balance` rewrites every tree block, so each subvolume's `generation` moves while
//! `ctransid` and the content do not. None of that is a change: no snapshot, no collapse, no
//! refused proof. A real inode change that only `generation` shows must still be caught.

mod common;
use bpm::mechanics::observe;
use bpm::policy::change::Change;
use common::*;

fn tick(env: &Env) {
    env.advance(600);
    env.run(&["tick"]).unwrap();
}

#[test]
fn balance_is_not_a_change() {
    let env = Env::new("");
    make_project(&env, "demo", 10);
    env.run(&["adopt", "demo"]).unwrap();
    tick(&env);
    assert_eq!(env.snaps("demo").len(), 1);

    env.fake.clear_ops();
    env.fake.balance();
    tick(&env);
    assert_eq!(env.snaps("demo").len(), 1, "balance alone takes no snapshot");
    let mark = env.state("demo").clean.expect("the balance check is remembered");
    assert_eq!(mark.snapshot, 1);
    assert_eq!(searches(&env, "demo"), 1);
    tick(&env);
    assert_eq!(searches(&env, "demo"), 1, "remembered: no second search");

    // real work still snapshots, through ctransid as before
    fs::write(env.p("demo/src/src0.txt"), "edit").unwrap();
    tick(&env);
    let snaps = env.snaps("demo");
    assert_eq!(snaps.len(), 2);
    assert!(snaps[1].reason.starts_with("changed (ctransid"), "{}", snaps[1].reason);
    assert_eq!(env.state("demo").stage, Stage::Active);
}

#[test]
fn balance_on_cold_project_takes_no_collapse() {
    let env = Env::new("");
    make_project(&env, "demo", 10);
    env.run(&["adopt", "demo"]).unwrap();
    env.advance(61 * 86400);
    env.run(&["tick"]).unwrap();
    for _ in 0..3 {
        tick(&env); // collapse and recompress settle, one heavy op per tick
    }
    assert_eq!(env.state("demo").stage, Stage::Cold);
    let before: Vec<u64> = env.snaps("demo").iter().map(|m| m.id).collect();
    assert_eq!(before.len(), 1);

    env.fake.clear_ops();
    env.fake.balance();
    tick(&env);
    tick(&env);
    let ops = env.fake.ops();
    assert!(!ops.iter().any(|o| o.starts_with("snapshot") || o.starts_with("defrag")), "{ops:?}");
    assert_eq!(env.snaps("demo").iter().map(|m| m.id).collect::<Vec<_>>(), before);
    assert_eq!(env.state("demo").stage, Stage::Cold, "a balance is not activity");
}

#[test]
fn inode_only_change_is_snapshotted() {
    let env = Env::new("");
    make_project(&env, "demo", 10);
    env.run(&["adopt", "demo"]).unwrap();
    tick(&env);
    // log replay after a crash: an inode item moves, ctransid does not
    env.fake.touch_inode_only(&env.p("demo")).unwrap();
    tick(&env);
    let snaps = env.snaps("demo");
    assert_eq!(snaps.len(), 2);
    assert!(snaps[1].reason.starts_with("changed (inode newer than snapshot #1"), "{}", snaps[1].reason);
    tick(&env);
    assert_eq!(env.snaps("demo").len(), 2, "and only once");
}

#[test]
fn clean_mark_belongs_to_one_snapshot() {
    let env = Env::new("");
    make_project(&env, "demo", 10);
    env.run(&["adopt", "demo"]).unwrap();
    tick(&env);
    // #2 holds an inode change that #1 does not; both have the same ctransid
    env.fake.touch_inode_only(&env.p("demo")).unwrap();
    env.run(&["snap", "demo"]).unwrap();
    env.fake.balance();
    tick(&env);
    assert_eq!(env.snaps("demo").len(), 2);
    assert_eq!(env.state("demo").clean.map(|c| c.snapshot), Some(2));

    // the reference goes back to #1: what was proven against #2 says nothing about #1
    env.run(&["rm", "demo", "2"]).unwrap();
    tick(&env);
    let snaps = env.snaps("demo");
    assert_eq!(snaps.len(), 2, "{:?}", snaps.iter().map(|m| &m.reason).collect::<Vec<_>>());
    assert!(snaps[1].reason.starts_with("changed (inode newer than snapshot #1"), "{}", snaps[1].reason);

    // a rollback replaces the live tree: nothing remembered survives it
    env.fake.balance();
    tick(&env);
    assert!(env.state("demo").clean.is_some());
    env.run(&["rollback", "demo", "1"]).unwrap();
    assert_eq!(env.state("demo").clean, None);
}

#[test]
fn without_privilege_a_balance_still_snapshots() {
    let env = Env::new("");
    make_project(&env, "demo", 10);
    env.run(&["adopt", "demo"]).unwrap();
    tick(&env);
    env.fake.set_privileged(false);
    env.fake.balance();
    tick(&env);
    let snaps = env.snaps("demo");
    assert_eq!(snaps.len(), 2, "unverified counts as a change (as before this check existed)");
    assert_eq!(snaps[1].reason, "metadata rewritten since snapshot #1; content check unavailable");
    assert_eq!(env.state("demo").clean, None);

    env.fake.balance();
    env.fake.sync(&env.root).unwrap();
    let live = env.fake.subvol_info(&env.p("demo")).unwrap();
    let c = observe::changed(&env.ctx(), &env.p("demo"), &live, env.snaps("demo").last(), None);
    assert_eq!(c, Change::Unverified);
    env.run(&["status", "demo"]).unwrap();
}

#[test]
fn deletion_proof_needs_the_inode_check() {
    let env = Env::new("");
    make_project(&env, "demo", 10);
    env.run(&["adopt", "demo"]).unwrap();
    tick(&env);
    env.fake.set_privileged(false);
    env.fake.balance();
    let err = env.run(&["archive", "demo", "--snapshot", "1", "--level", "1", "--delete-live", "--yes"]).unwrap_err();
    assert!(format!("{err:#}").contains("content check could not run"), "{err:#}");
    assert!(env.p("demo/src/src0.txt").exists(), "live kept");
}

#[test]
fn archive_after_balance_reuses_the_newest_snapshot() {
    let env = Env::new("");
    make_project(&env, "demo", 10);
    env.run(&["adopt", "demo"]).unwrap();
    tick(&env);
    env.fake.balance();
    env.run(&["archive", "demo", "--level", "3", "--delete-live", "--yes"]).unwrap();
    assert!(!env.p("demo").exists(), "live deleted: the proof accepted the balanced tree");
    assert!(
        env.snaps("demo").iter().all(|m| m.kind != SnapshotKind::Manual),
        "no extra snapshot: {:?}",
        env.snaps("demo").iter().map(|m| (m.id, m.kind)).collect::<Vec<_>>()
    );
}

#[test]
fn rollback_after_balance_is_already_identical() {
    let env = Env::new("");
    make_project(&env, "demo", 10);
    env.run(&["adopt", "demo"]).unwrap();
    tick(&env);
    env.fake.balance();
    let err = env.run(&["rollback", "demo", "1"]).unwrap_err();
    assert!(format!("{err:#}").contains("already identical"), "{err:#}");
}
