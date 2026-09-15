//! Real btrfs end-to-end tests. They need root and a scratch btrfs mount:
//!     scripts/e2e.sh            (creates a loop device, builds, runs these with sudo)
//! Every test works in its own subvolume under $BPM_E2E_MNT.

use bpm::btrfs::ioctl::get_subvol_info;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct T {
    base: PathBuf,
    root: PathBuf,
    cfg: PathBuf,
    uid: u32,
    gid: u32,
}

fn sh(cmd: &str) -> Output {
    let o = Command::new("bash").arg("-c").arg(cmd).output().unwrap();
    if !o.status.success() {
        panic!("command failed: {cmd}\n{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }
    o
}

fn ino(p: &Path) -> u64 {
    fs::symlink_metadata(p).map(|m| m.ino()).unwrap_or(0)
}

impl T {
    fn new(name: &str) -> Option<T> {
        let mnt = std::env::var("BPM_E2E_MNT").ok()?;
        assert!(unsafe { libc::geteuid() } == 0, "e2e tests must run as root");
        let uid: u32 = std::env::var("BPM_E2E_UID").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
        let gid: u32 = std::env::var("BPM_E2E_GID").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
        let base = PathBuf::from(mnt).join(name);
        if base.exists() {
            sh(&format!("btrfs subvolume delete -R {0}/root 2>/dev/null; rm -rf {0}", base.display()));
        }
        fs::create_dir_all(&base).unwrap();
        let root = base.join("root");
        sh(&format!("btrfs subvolume create {} >/dev/null && chown {uid}:{gid} {0}", root.display()));
        let cfg = base.join("config.toml");
        fs::write(
            &cfg,
            format!(
                r#"version = 1
[global]
hooks_dir = "{b}/hooks"
archive_dir = "{b}/archives"
heavy_min_free = "100M"
lock_timeout = "5s"
[[root]]
path = "{r}"
adopt = "manual"
[defaults]
banlist_settle = "0s"
[defaults.snapshot]
min_interval = "0s"
[defaults.thin]
min_free = "100M"
[defaults.recompress]
min_free = "100M"
min_expected_gain = 0.0
[defaults.shrink_guard]
min_files = 5
min_bytes = "1K"
"#,
                b = base.display(),
                r = root.display()
            ),
        )
        .unwrap();
        let t = T { base, root, cfg, uid, gid };
        t.ok(&["setup", "--no-units"]);
        Some(t)
    }

    fn bpm(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bpm"))
            .arg("--no-sudo")
            .arg("--config")
            .arg(&self.cfg)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.bpm(args);
        if !o.status.success() {
            panic!(
                "bpm {args:?} failed ({}):\n{}{}",
                o.status,
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
        }
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        serde_json::from_str(&self.ok(&a)).unwrap()
    }

    fn as_user(&self, cmd: &str) -> Output {
        Command::new("setpriv")
            .args([
                "--reuid",
                &self.uid.to_string(),
                "--regid",
                &self.gid.to_string(),
                "--clear-groups",
                "bash",
                "-c",
                cmd,
            ])
            .output()
            .unwrap()
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn snapshots(&self, project: &str) -> Vec<serde_json::Value> {
        self.json(&["list", project]).as_array().unwrap().clone()
    }

    fn make_project(&self, name: &str) {
        let p = self.p(name).display().to_string();
        sh(&format!(
            "mkdir -p {p}/src {p}/target/debug {p}/.git && echo '[package]' > {p}/Cargo.toml && echo ref > {p}/.git/HEAD \
             && for i in $(seq 1 30); do yes \"line $i\" | head -n 200 > {p}/src/f$i.rs; done \
             && ln {p}/src/f1.rs {p}/src/hardlink.rs && ln -s f2.rs {p}/src/link.rs \
             && setfattr -n user.bpm -v yes {p}/src/f3.rs 2>/dev/null || python3 -c \"import os; os.setxattr('{p}/src/f3.rs','user.bpm',b'yes')\" \
             && head -c 3000000 /dev/urandom > {p}/target/debug/big.bin && chown -R {}:{} {p}",
            self.uid, self.gid
        ));
    }
}

macro_rules! setup {
    ($name:expr) => {
        match T::new($name) {
            Some(t) => t,
            None => {
                eprintln!("BPM_E2E_MNT not set; skipping");
                return;
            }
        }
    };
}

#[test]
#[ignore]
fn e2e_adopt_snapshot_and_change_detection() {
    let t = setup!("adopt");
    t.make_project("demo");
    t.ok(&["adopt", "demo"]);
    assert_eq!(ino(&t.p("demo")), 256, "project is a subvolume");
    assert_eq!(ino(&t.p("demo/target")), 256, "target is a nested subvolume");
    let md = fs::metadata(t.p("demo/src/f5.rs")).unwrap();
    assert_eq!((md.uid(), md.gid()), (t.uid, t.gid), "ownership preserved");
    assert_eq!(fs::metadata(t.p("demo/src/f1.rs")).unwrap().nlink(), 2, "hardlinks preserved");
    assert_eq!(fs::read_link(t.p("demo/src/link.rs")).unwrap(), PathBuf::from("f2.rs"));
    let snaps = t.snapshots("demo");
    assert_eq!(snaps.len(), 1);
    let snap = t.root.join(".bpm/projects/demo/1/snapshot");
    assert!(get_subvol_info(&snap).unwrap().readonly());
    assert_eq!(fs::read_dir(snap.join("target")).unwrap().count(), 0, "target placeholder is empty");
    assert!(!t.p("demo.bpm-old").exists());

    t.ok(&["tick"]);
    assert_eq!(t.snapshots("demo").len(), 1, "unchanged: no snapshot");
    t.as_user(&format!("head -c 100000 /dev/urandom > {}/target/debug/more.bin", t.p("demo").display()));
    t.ok(&["tick"]);
    assert_eq!(t.snapshots("demo").len(), 1, "nested build write: no snapshot");
    t.as_user(&format!("echo edit >> {}/src/f4.rs", t.p("demo").display()));
    t.ok(&["tick"]);
    assert_eq!(t.snapshots("demo").len(), 2, "content change: snapshot");
    t.as_user(&format!("chmod 600 {}/src/f6.rs", t.p("demo").display()));
    t.ok(&["tick"]);
    assert_eq!(t.snapshots("demo").len(), 3, "mode-only change detected through generation");
    t.ok(&["tick"]);
    assert_eq!(t.snapshots("demo").len(), 3);

    // unprivileged users cannot remove snapshots
    let snap2 = t.root.join(".bpm/projects/demo/2/snapshot");
    assert!(!t.as_user(&format!("rm -rf {}/src", snap2.display())).status.success());
    assert!(!t.as_user(&format!("btrfs subvolume delete {}", snap2.display())).status.success());
    assert!(snap2.join("src/f1.rs").exists());

    // container snapshot of the root excludes project content
    let c = t.root.join(".bpm/container/1/snapshot");
    assert!(c.join("demo").is_dir() && fs::read_dir(c.join("demo")).unwrap().count() == 0);
}

#[test]
#[ignore]
fn e2e_cargo_clean_shrink_guard_rollback() {
    let t = setup!("guard");
    t.make_project("demo");
    t.ok(&["adopt", "demo"]);
    sh(&format!("echo keep > {}/target/debug/marker", t.p("demo").display()));

    let o = t.as_user(&format!("rm -rf {}/target", t.p("demo").display()));
    assert!(o.status.success(), "unprivileged rm -rf of the nested build dir works");
    t.ok(&["tick"]);
    assert_eq!(ino(&t.p("demo/target")), 256, "tick recreated target as a nested subvolume");
    sh(&format!(
        "mkdir -p {0}/target/debug && echo keep > {0}/target/debug/marker && chown -R {1}:{2} {0}/target/debug",
        t.p("demo").display(),
        t.uid,
        t.gid
    ));

    assert!(
        t.as_user(&format!("rm -f {}/src/f1[0-9].rs {}/src/f2[0-9].rs", t.p("demo").display(), t.p("demo").display()))
            .status
            .success()
    );
    t.ok(&["tick"]);
    let st = t.json(&["status", "demo"]);
    assert!(st["state"]["frozen"].is_object(), "{st}");
    let held: Vec<_> = t.snapshots("demo").into_iter().filter(|m| m["hold"] == true).collect();
    assert_eq!(held.len(), 1);

    t.ok(&["rollback", "demo", "held"]);
    assert!(t.p("demo/src/f15.rs").exists());
    assert_eq!(fs::read_to_string(t.p("demo/target/debug/marker")).unwrap().trim(), "keep");
    assert_eq!(ino(&t.p("demo/target")), 256);
    let st = t.json(&["status", "demo"]);
    assert!(st["state"]["frozen"].is_null(), "{st}");
    assert!(
        fs::read_dir(&t.root).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains("bpm-rollback"))
    );
    assert_eq!(fs::metadata(t.p("demo")).unwrap().uid(), t.uid);
    t.ok(&["tick"]);
}

#[test]
#[ignore]
fn e2e_deleted_project_recreated() {
    let t = setup!("orphan");
    t.make_project("demo");
    t.ok(&["adopt", "demo"]);
    let o = t.as_user(&format!("rm -rf {}", t.p("demo").display()));
    assert!(
        o.status.success(),
        "an unprivileged user can delete the whole project: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!t.p("demo").exists());
    let tick = t.bpm(&["tick"]);
    assert!(tick.status.success(), "{}", String::from_utf8_lossy(&tick.stderr));
    let st = t.json(&["status", "demo"]);
    assert_eq!(st["state"]["stage"], "orphaned");
    t.ok(&["restore", "demo", "--recreate"]);
    assert!(t.p("demo/src/f30.rs").exists());
    assert_eq!(ino(&t.p("demo")), 256);
    assert_eq!(ino(&t.p("demo/target")), 256);
    assert_eq!(fs::metadata(t.p("demo")).unwrap().uid(), t.uid);
    t.ok(&["tick"]);
    assert_eq!(t.json(&["status", "demo"])["state"]["stage"], "active");
}

#[test]
#[ignore]
fn e2e_convert_recompress_archive_hook() {
    let t = setup!("heavy");
    t.make_project("demo");
    t.ok(&["adopt", "demo"]);

    // plain build dir created by a tool -> converted by tick
    sh(&format!(
        "mkdir -p {0}/cmake-build-debug && head -c 200000 /dev/urandom > {0}/cmake-build-debug/o.bin",
        t.p("demo").display()
    ));
    fs::write(t.p("demo/.bpm.toml"), "banlist_add = ['cmake-build-debug']\n").unwrap();
    t.ok(&["tick"]);
    t.ok(&["tick"]);
    assert_eq!(ino(&t.p("demo/cmake-build-debug")), 256, "converted");
    assert_eq!(fs::metadata(t.p("demo/cmake-build-debug/o.bin")).unwrap().len(), 200000);

    // recompress on real btrfs
    let r = t.json(&["recompress", "demo", "--level", "3", "--force"]);
    assert_eq!(r[0][1]["done"], true, "{r}");
    assert_eq!(t.snapshots("demo").len(), 1);

    // claude hook: destructive command in a changed project creates a hook snapshot
    sh(&format!("echo change >> {}/src/f1.rs", t.p("demo").display()));
    let input = format!(
        r#"{{"cwd":"{}/src","tool_name":"Bash","tool_input":{{"command":"rm -rf build"}}}}"#,
        t.p("demo").display()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_bpm"))
        .arg("--config")
        .arg(&t.cfg)
        .args(["snap", "--claude-hook"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let kinds: Vec<String> = t.snapshots("demo").iter().map(|m| m["kind"].as_str().unwrap().to_string()).collect();
    assert!(kinds.contains(&"hook".to_string()), "{kinds:?} {}", String::from_utf8_lossy(&out.stderr));

    // archive with real send | zstd, delete live, unarchive with receive
    t.ok(&["archive", "demo", "--level", "3", "--delete-live", "--yes"]);
    assert!(!t.p("demo").exists());
    t.ok(&["unarchive", "demo"]);
    assert!(t.p("demo/src/f1.rs").exists());
    assert_eq!(ino(&t.p("demo/target")), 256, "nested dirs recreated from the manifest");
    assert_eq!(ino(&t.p("demo/cmake-build-debug")), 256);
    // a directory that already contains a subvolume is refused (copying would flatten it)
    sh(&format!(
        "mkdir -p {0}/other && btrfs subvolume create {0}/other/data >/dev/null && echo x > {0}/other/data/f",
        t.root.display()
    ));
    let o = t.bpm(&["adopt", "other"]);
    assert_eq!(o.status.code(), Some(8), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("nested subvolumes"));
    // diff against live does not descend into nested build subvolumes
    sh(&format!("head -c 1000 /dev/urandom > {}/target/new-object.o", t.p("demo").display()));
    let diff = t.ok(&["diff", "demo", "latest"]);
    assert!(!diff.contains("target/"), "{diff}");
    // renaming a project relinks its store entry
    sh(&format!("mv {} {}", t.p("demo").display(), t.p("demo-renamed").display()));
    t.ok(&["tick"]);
    assert!(t.root.join(".bpm/projects/demo-renamed/project.toml").exists());
    let d = t.ok(&["doctor"]);
    assert!(!d.contains("ERROR"), "{d}");
    assert!(d.contains("subvolumes not managed by bpm") || d.contains("unadopted"), "{d}");
    let _ = t.base.exists();
}

#[test]
#[ignore]
fn e2e_wrap_and_doctor_fix() {
    let t = setup!("doctor");
    t.make_project("demo");
    t.ok(&["adopt", "demo"]);

    // wrap: pre and post snapshots around a command, exit code passed through
    let o = Command::new(env!("CARGO_BIN_EXE_bpm"))
        .arg("--config")
        .arg(&t.cfg)
        .args(["wrap", "--project", "demo", "--", "bash", "-c"])
        .arg(format!("echo wrapped >> {}/src/f1.rs; exit 3", t.p("demo").display()))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(3), "{}", String::from_utf8_lossy(&o.stderr));
    let kinds: Vec<String> = t.snapshots("demo").iter().map(|m| m["kind"].as_str().unwrap().to_string()).collect();
    assert!(kinds.contains(&"pre".into()) && kinds.contains(&"post".into()), "{kinds:?}");
    let post = t.snapshots("demo").into_iter().find(|m| m["kind"] == "post").unwrap();
    assert!(post["pair"].is_u64());

    // interrupted adopt before the swap: partial copy next to the original plain dir
    sh(&format!(
        "mkdir -p {0}/half/src && echo a > {0}/half/src/a && btrfs subvolume create {0}/half.bpm-tmp >/dev/null",
        t.root.display()
    ));
    // interrupted rollback before the new tree existed
    sh(&format!(
        "btrfs subvolume create {0}/gone.bpm-rollback-20260101T000000 >/dev/null && echo b > {0}/gone.bpm-rollback-20260101T000000/b",
        t.root.display()
    ));
    // snapshot without metadata
    sh(&format!("rm {}/.bpm/projects/demo/1/meta.toml", t.root.display()));
    let before = t.ok(&["doctor"]);
    assert!(
        before.contains("adopt was interrupted")
            && before.contains("rollback was interrupted")
            && before.contains("no metadata"),
        "{before}"
    );
    t.ok(&["doctor", "--fix"]);
    assert!(!t.p("half.bpm-tmp").exists());
    assert!(t.p("half/src/a").exists(), "original untouched");
    assert!(t.p("gone/b").exists(), "rollback leftover renamed back");
    let recovered = t.snapshots("demo").into_iter().find(|m| m["id"] == 1).unwrap();
    assert_eq!(recovered["kind"], "recovered");
    assert_eq!(recovered["hold"], true);
    let after = t.ok(&["doctor"]);
    assert!(!after.contains("interrupted"), "{after}");
}

#[test]
#[ignore]
fn e2e_nocow_and_cross_entry_hardlinks_adopt() {
    let t = setup!("nocow");
    let p = t.p("vm").display().to_string();
    // a NOCOW directory (VM images, databases) and a hardlink between two top-level entries
    sh(&format!(
        "mkdir -p {p}/images {p}/bin {p}/scripts && chattr +C {p}/images && head -c 2000000 /dev/urandom > {p}/images/disk.img \
         && echo tool > {p}/scripts/tool && ln {p}/scripts/tool {p}/bin/tool && chown -R {}:{} {p}",
        t.uid, t.gid
    ));
    let sum = sh(&format!("sha256sum {p}/images/disk.img | cut -d' ' -f1")).stdout;
    t.ok(&["adopt", "vm"]);
    assert_eq!(ino(&t.p("vm")), 256);
    assert_eq!(sh(&format!("sha256sum {p}/images/disk.img | cut -d' ' -f1")).stdout, sum);
    assert_eq!(fs::read_to_string(t.p("vm/bin/tool")).unwrap(), "tool\n");
    assert!(!t.root.join("vm.bpm-old").exists() && !t.root.join("vm.bpm-tmp").exists());
}

#[test]
#[ignore]
fn e2e_unflushed_append_is_not_identical() {
    let t = setup!("flush");
    t.make_project("demo");
    t.ok(&["adopt", "demo"]);
    // an append does not move ctransid until writeback; rollback must flush before comparing
    sh(&format!("echo corrupted >> {}/src/f5.rs", t.p("demo").display()));
    t.ok(&["rollback", "demo", "latest"]);
    assert!(!fs::read_to_string(t.p("demo/src/f5.rs")).unwrap().contains("corrupted"));
}

#[test]
#[ignore]
fn e2e_readonly_snapshot_moves_between_directories() {
    use bpm::btrfs::Btrfs;
    let t = setup!("romove");
    let b = bpm::btrfs::real::RealBtrfs::default();
    sh(&format!("mkdir -p {0}/.snapshots/1 {0}/store && echo x > {0}/f", t.root.display()));
    let snap = t.root.join(".snapshots/1/snapshot");
    b.snapshot(&t.root, &snap, true).unwrap();
    let before = get_subvol_info(&snap).unwrap();
    let dst = t.root.join("store/snapshot");
    assert!(fs::rename(&snap, &dst).is_err(), "a read-only subvolume cannot change directory");
    // what `migrate-from-snapper --import` does
    b.set_readonly(&snap, false).unwrap();
    fs::rename(&snap, &dst).unwrap();
    b.set_readonly(&dst, true).unwrap();
    let after = get_subvol_info(&dst).unwrap();
    assert_eq!((after.uuid, after.parent_uuid, after.ctransid), (before.uuid, before.parent_uuid, before.ctransid));
    assert!(after.readonly());
}

#[test]
#[ignore]
fn e2e_project_hook_runs_as_owner_without_groups() {
    let t = setup!("phook");
    fs::write(&t.cfg, fs::read_to_string(&t.cfg).unwrap().replace("[global]\n", "[global]\nproject_hooks = \"on\"\n"))
        .unwrap();
    t.make_project("demo");
    t.ok(&["adopt", "demo"]);
    let dir = t.p("demo/.bpm/hooks/pre-snapshot");
    let out = t.base.join("hook-out");
    sh(&format!(
        "mkdir -p {d} && printf '#!/bin/sh\\nid -u > {o}.tmp; id -G >> {o}.tmp\\n' > {d}/10-id && chmod 755 {d}/10-id \
         && chown -R {u}:{g} {proj}/.bpm && touch {o}.tmp && chown {u}:{g} {o}.tmp",
        d = dir.display(),
        o = out.display(),
        u = t.uid,
        g = t.gid,
        proj = t.p("demo").display()
    ));
    t.ok(&["snap", "demo"]);
    let got = fs::read_to_string(format!("{}.tmp", out.display())).unwrap();
    assert_eq!(got, format!("{}\n{}\n", t.uid, t.gid), "uid, then only the primary group");
}
