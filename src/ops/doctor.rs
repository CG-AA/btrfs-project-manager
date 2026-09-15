//! `bpm doctor`: find problems; `--fix` repairs the safe ones.

use crate::cli::DoctorArgs;
use crate::ctx::Ctx;
use crate::mechanics::adopt;
use crate::output::{Table, emit};
use crate::project;
use crate::store::journal::Journal;
use crate::store::{SnapshotKind, SnapshotMeta, Stage, Unit};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[derive(Serialize, Clone, Debug)]
pub struct Finding {
    pub level: &'static str,
    pub what: String,
    pub fix: Option<String>,
    pub fixed: bool,
}

struct Doc<'a> {
    ctx: &'a Ctx,
    fix: bool,
    out: Vec<Finding>,
}

impl Doc<'_> {
    fn ok(&mut self, what: impl Into<String>) {
        self.out.push(Finding { level: "ok", what: what.into(), fix: None, fixed: false });
    }
    fn info(&mut self, what: impl Into<String>) {
        self.out.push(Finding { level: "info", what: what.into(), fix: None, fixed: false });
    }
    fn warn(&mut self, what: impl Into<String>, fix: Option<String>) {
        self.out.push(Finding { level: "warn", what: what.into(), fix, fixed: false });
    }
    fn error(&mut self, what: impl Into<String>, fix: Option<String>) {
        self.out.push(Finding { level: "error", what: what.into(), fix, fixed: false });
    }
    /// Record a fixable problem and run `f` when --fix was given.
    fn fixable(&mut self, level: &'static str, what: String, fix: String, f: impl FnOnce() -> Result<()>) {
        let mut fixed = false;
        if self.fix {
            match f() {
                Ok(()) => fixed = true,
                Err(e) => {
                    self.out.push(Finding {
                        level: "error",
                        what: format!("fix failed for {what}: {e:#}"),
                        fix: None,
                        fixed: false,
                    });
                }
            }
        }
        self.out.push(Finding { level, what, fix: Some(fix), fixed });
    }
}

fn kernel_version() -> Option<(u32, u32)> {
    let r = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
    let mut it = r.trim().split(['.', '-']);
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn snapper_configs_for(root: &Path) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/etc/snapper/configs") else {
        return out;
    };
    for e in rd.flatten() {
        let Ok(t) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let get =
            |k: &str| t.lines().find_map(|l| l.strip_prefix(&format!("{k}=")).map(|v| v.trim_matches('"').to_string()));
        if get("SUBVOLUME").as_deref() == Some(&root.display().to_string()) {
            out.push((e.file_name().to_string_lossy().into_owned(), get("TIMELINE_CREATE").as_deref() == Some("yes")));
        }
    }
    out
}

pub fn run(ctx: &Ctx, a: DoctorArgs) -> Result<()> {
    let mut d = Doc { ctx, fix: a.fix, out: vec![] };
    match &ctx.cfg_path {
        Some(p) => d.ok(format!("config {}", p.display())),
        None => d.warn("no config file; using the embedded default", Some("bpm setup".into())),
    }
    match kernel_version() {
        Some((maj, min)) if (maj, min) >= (6, 15) => d.ok(format!("kernel {maj}.{min} supports recompression levels")),
        Some((maj, min)) => {
            d.warn(format!("kernel {maj}.{min} < 6.15: `defragment -L` unsupported; recompress will fail"), None)
        }
        None => {}
    }
    for tool in ["btrfs", "zstd", "cp"] {
        if Command::new(tool).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            d.ok(format!("{tool} available"));
        } else {
            d.error(format!("{tool} not found"), None);
        }
    }
    let timer = Command::new("systemctl")
        .args(["is-enabled", "bpm.timer"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if timer == "enabled" {
        d.ok("bpm.timer enabled");
    } else {
        d.warn(
            format!("bpm.timer is {}", if timer.is_empty() { "not installed".into() } else { timer }),
            Some("bpm setup".into()),
        );
    }
    for root in ctx.roots() {
        let store = ctx.store(&root);
        let rp = root.path.display().to_string();
        if !root.path.is_dir() {
            d.error(format!("root {rp} does not exist"), None);
            continue;
        }
        if !store.exists() {
            d.error(format!("{rp}: store {} not initialized", store.dir.display()), Some("bpm setup".into()));
            continue;
        }
        if !ctx.btrfs.is_subvolume(&store.dir).unwrap_or(false) {
            d.error(
                format!(
                    "{rp}: store {} is not a subvolume (container snapshots would include it)",
                    store.dir.display()
                ),
                None,
            );
        }
        if let (Ok(Some(info)), Ok(live)) = (store.read_info(), ctx.btrfs.subvol_info(&root.path)) {
            if info.root_uuid != live.uuid {
                d.error(
                    format!("{rp}: store was created for subvolume {} but {rp} is {}", info.root_uuid, live.uuid),
                    None,
                );
            }
        }
        for (name, timeline) in snapper_configs_for(&root.path) {
            if timeline {
                d.warn(
                    format!(
                        "{rp}: snapper config {name:?} still takes timeline snapshots (they include build directories)"
                    ),
                    Some(format!("bpm migrate-from-snapper --snapper-config {name}")),
                );
            } else {
                d.info(format!("{rp}: snapper config {name:?} exists with timeline disabled"));
            }
        }
        if let Ok(free) = ctx.free_bytes(&root.path) {
            if free < ctx.cfg.global.heavy_min_free.0 {
                d.warn(
                    format!("{rp}: only {} free; heavy operations are paused", crate::util::bytes::fmt_bytes(free)),
                    None,
                );
            }
        }
        check_root_leftovers(&mut d, &root, &store);
        let disc = match project::discover(ctx, &root, &store) {
            Ok(x) => x,
            Err(e) => {
                d.error(format!("{rp}: discovery failed: {e:#}"), None);
                continue;
            }
        };
        if !disc.unadopted.is_empty() {
            d.warn(
                format!(
                    "{rp}: {} unadopted directories: {}",
                    disc.unadopted.len(),
                    disc.unadopted.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
                ),
                Some("bpm adopt --all".into()),
            );
        }
        if !disc.foreign_subvols.is_empty() {
            d.warn(
                format!(
                    "{rp}: subvolumes not managed by bpm: {}",
                    disc.foreign_subvols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
                ),
                Some("bpm adopt <name> (registers without copying)".into()),
            );
        }
        for (unit, rec) in &disc.missing {
            let st = unit.read_state().unwrap_or_default();
            if st.stage != Stage::Archived {
                d.warn(
                    format!("{}: project directory {} is missing", unit.name, rec.path.display()),
                    Some(format!("bpm restore {} --recreate", unit.name)),
                );
            }
        }
        let units: Vec<Unit> = store
            .units()
            .unwrap_or_default()
            .into_iter()
            .map(|(u, _)| u)
            .chain(std::iter::once(store.container()))
            .collect();
        for unit in units {
            check_unit(&mut d, &unit);
        }
        for f in &disc.managed {
            let st = f.unit.read_state().unwrap_or_default();
            if let Some(fr) = &st.frozen {
                d.warn(
                    format!("{}: FROZEN: {}", f.name, fr.reasons.join("; ")),
                    Some(format!("bpm status {0}; then bpm rollback {0} held or bpm unfreeze {0}", f.name)),
                );
            }
            if let Some(e) = &st.last_error {
                d.warn(format!("{}: last tick error: {}", f.name, e.message), None);
            }
            if let Ok(eff) = project::effective(ctx, &root, &f.name, &f.path) {
                check_nested_leftovers(&mut d, &f.path, &eff.banlist);
                for rel in st.pending_convert.keys() {
                    d.info(format!(
                        "{}: banned directory {rel} is a plain directory (included in snapshots until converted)",
                        f.name
                    ));
                }
            }
        }
    }
    check_claude_hook(&mut d);
    let problems = d.out.iter().filter(|f| matches!(f.level, "warn" | "error") && !f.fixed).count();
    emit(ctx, &d.out, || {
        let mut t = Table::new(&["", "FINDING", "FIX"]);
        for f in &d.out {
            let mark = match (f.level, f.fixed) {
                (_, true) => "fixed",
                ("ok", _) => "ok",
                ("info", _) => "info",
                ("warn", _) => "WARN",
                _ => "ERROR",
            };
            t.row(vec![mark.into(), f.what.clone(), f.fix.clone().unwrap_or_default()]);
        }
        let mut s = t.render();
        if problems > 0
            && !a.fix
            && d.out.iter().any(|f| f.fix.as_deref().is_some_and(|x| x.starts_with("doctor --fix")))
        {
            s += "\nrun `bpm doctor --fix` to repair the items marked `doctor --fix`\n";
        }
        s
    });
    Ok(())
}

fn check_root_leftovers(d: &mut Doc, root: &crate::config::RootCfg, store: &crate::store::Store) {
    let ctx = d.ctx;
    let Ok(rd) = std::fs::read_dir(&root.path) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let is_sub = ctx.btrfs.is_subvolume(&path).unwrap_or(false);
        if let Some(base) = name.strip_suffix(".bpm-tmp") {
            let orig = root.path.join(base);
            let orig_sub = ctx.btrfs.is_subvolume(&orig).unwrap_or(false);
            if is_sub && orig.is_dir() && !orig_sub {
                let p = path.clone();
                d.fixable(
                    "warn",
                    format!("{}: adopt was interrupted before the swap", path.display()),
                    "doctor --fix deletes the partial copy".into(),
                    || ctx.btrfs.delete_subvolume(&p, true),
                );
            } else if !is_sub && orig_sub {
                let p = path.clone();
                d.fixable(
                    "warn",
                    format!("{}: old directory left after adopt swap", path.display()),
                    "doctor --fix removes it".into(),
                    || Ok(std::fs::remove_dir_all(&p)?),
                );
            } else {
                d.error(format!("{}: unexpected leftover; inspect manually", path.display()), None);
            }
        } else if let Some(base) = name.strip_suffix(".bpm-old") {
            let orig = root.path.join(base);
            if ctx.btrfs.is_subvolume(&orig).unwrap_or(false) {
                let registered = store.unit(base).read_record().ok().flatten().is_some();
                let p = path.clone();
                let (root_c, base_s) = (root.clone(), base.to_string());
                d.fixable(
                    "warn",
                    format!(
                        "{}: adopt finished the swap but not the cleanup{}",
                        path.display(),
                        if registered { "" } else { " or registration" }
                    ),
                    "doctor --fix registers the project and removes the old copy".into(),
                    || {
                        if !registered {
                            adopt::adopt(
                                ctx,
                                &root_c,
                                &base_s,
                                &adopt::AdoptOpts {
                                    keep_build: Some(false),
                                    force: true,
                                    verify_paths: false,
                                    require_unused: false,
                                },
                            )?;
                        }
                        Ok(std::fs::remove_dir_all(&p)?)
                    },
                );
            } else {
                d.error(
                    format!(
                        "{}: leftover next to a non-subvolume {}; inspect manually",
                        path.display(),
                        orig.display()
                    ),
                    None,
                );
            }
        } else if let Some(idx) = name.find(".bpm-rollback-") {
            let base = &name[..idx];
            let orig = root.path.join(base);
            if !orig.exists() {
                let (p, o) = (path.clone(), orig.clone());
                d.fixable(
                    "error",
                    format!("{}: rollback was interrupted before the new tree existed", path.display()),
                    format!("doctor --fix renames it back to {}", orig.display()),
                    || Ok(std::fs::rename(&p, &o)?),
                );
            } else {
                let nested = has_nested(ctx, &path);
                if nested {
                    d.error(format!("{}: rollback leftover still contains nested subvolumes; move them into {} or delete with `btrfs subvolume delete -R`", path.display(), orig.display()), None);
                } else {
                    let p = path.clone();
                    d.fixable(
                        "warn",
                        format!(
                            "{}: pre-rollback tree left behind (it is also saved as a rollback snapshot)",
                            path.display()
                        ),
                        "doctor --fix deletes it".into(),
                        || ctx.btrfs.delete_subvolume(&p, false),
                    );
                }
            }
        }
    }
}

fn has_nested(ctx: &Ctx, dir: &Path) -> bool {
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .flatten()
        .any(|e| crate::util::walk::entry_is_subvol(&e, &|p, i| ctx.is_subvol(p, i)))
}

fn check_nested_leftovers(d: &mut Doc, live: &Path, banlist: &[String]) {
    let ctx = d.ctx;
    for rel in banlist {
        let full = live.join(rel);
        for suffix in [".bpm-tmp", ".bpm-old"] {
            let p = crate::util::fs::sibling(&full, suffix);
            if p.symlink_metadata().is_err() {
                continue;
            }
            let p_sub = ctx.btrfs.is_subvolume(&p).unwrap_or(false);
            let full_sub = ctx.btrfs.is_subvolume(&full).unwrap_or(false);
            let pc = p.clone();
            if p_sub && !full_sub {
                d.fixable(
                    "warn",
                    format!("{}: interrupted banlist conversion (copy not swapped)", p.display()),
                    "doctor --fix deletes the partial copy".into(),
                    || ctx.btrfs.delete_subvolume(&pc, true),
                );
            } else if !p_sub && full_sub {
                d.fixable(
                    "warn",
                    format!("{}: old build directory left after conversion", p.display()),
                    "doctor --fix removes it".into(),
                    || Ok(std::fs::remove_dir_all(&pc)?),
                );
            }
        }
    }
}

fn check_unit(d: &mut Doc, unit: &Unit) {
    let ctx = d.ctx;
    if let Some(j) = Journal::read(&unit.dir) {
        let stale = crate::util::time::age(ctx.now(), j.updated) > std::time::Duration::from_secs(600);
        if stale {
            let dir = unit.dir.clone();
            d.fixable(
                "warn",
                format!(
                    "{}: {} was interrupted at step {} ({})",
                    unit.name,
                    j.op,
                    j.step,
                    j.data.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ")
                ),
                "doctor --fix clears the journal after the leftover checks above".into(),
                || Ok(std::fs::remove_file(Journal::path_in(&dir))?),
            );
        }
    }
    let Ok(scan) = unit.scan() else { return };
    for (id, path) in scan.without_meta {
        let u = unit.clone();
        d.fixable(
            "warn",
            format!("{}: snapshot #{id} has no metadata", unit.name),
            "doctor --fix writes recovered metadata and holds it".into(),
            || recover_meta(ctx, &u, id, &path),
        );
    }
    if !scan.tmp_dirs.is_empty() || !scan.without_snapshot.is_empty() {
        let u = unit.clone();
        d.fixable(
            "info",
            format!(
                "{}: {} in-progress dirs, {} metadata files without snapshot",
                unit.name,
                scan.tmp_dirs.len(),
                scan.without_snapshot.len()
            ),
            "doctor --fix (or the next tick) cleans them".into(),
            || super::tick::cleanup_unit(ctx, &u),
        );
    }
}

fn recover_meta(ctx: &Ctx, unit: &Unit, id: u64, path: &Path) -> Result<()> {
    let info = ctx.btrfs.subvol_info(path)?;
    let meta = SnapshotMeta {
        format: 1,
        id,
        project: unit.name.clone(),
        created: info.otime.unwrap_or(ctx.now()),
        kind: SnapshotKind::Recovered,
        reason: "metadata recovered by doctor".into(),
        pair: None,
        hold: true,
        hold_note: "recovered".into(),
        source_uuid: info.parent_uuid.unwrap_or_default(),
        source_ctransid: info.ctransid,
        snapshot_uuid: info.uuid,
        snapshot_otransid: info.otransid,
        received_uuid: info.received_uuid,
        stats: None,
        origin: ctx.origin(),
    };
    unit.write_meta(&unit.snapshot_dir(id), &meta)
}

fn check_claude_hook(d: &mut Doc) {
    let user = &d.ctx.invoker.user;
    let home = if user.is_empty() || user == "root" {
        std::env::var("HOME").unwrap_or_default()
    } else {
        format!("/home/{user}")
    };
    let settings = Path::new(&home).join(".claude/settings.json");
    match std::fs::read_to_string(&settings) {
        Ok(t) if t.contains("snap --claude-hook") => {
            d.ok(format!("Claude Code hook configured in {}", settings.display()))
        }
        Ok(_) => d.info(format!("no bpm hook in {} (see `bpm setup --print-claude-hook`)", settings.display())),
        Err(_) => {}
    }
}
