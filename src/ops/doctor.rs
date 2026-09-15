//! `bpm doctor`: find problems; `--fix` repairs the safe ones.

use crate::cli::DoctorArgs;
use crate::ctx::Ctx;
use crate::mechanics::adopt;
use crate::mechanics::retire::{self, Expect};
use crate::output::{Table, emit};
use crate::project::{self, Leftover};
use crate::store::journal::Journal;
use crate::store::{SnapshotKind, SnapshotMeta, Stage, Unit};
use crate::util::relpath::RelPath;
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};
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
        if self.fix && self.ctx.opts.dry_run {
            // the fix runs against the dry-run backends, so it only logs what it would do
            if let Err(e) = f() {
                self.out.push(Finding {
                    level: "error",
                    what: format!("[dry-run] fix would fail for {what}: {e:#}"),
                    fix: None,
                    fixed: false,
                });
            }
            self.out.push(Finding { level, what, fix: Some(format!("[dry-run] would run: {fix}")), fixed: false });
            return;
        }
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
    let out = findings(ctx, a.fix);
    let problems = out.iter().filter(|f| matches!(f.level, "warn" | "error") && !f.fixed).count();
    emit(ctx, &out, || {
        let mut t = Table::new(&["", "FINDING", "FIX"]);
        for f in &out {
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
        if problems > 0 && !a.fix && out.iter().any(|f| f.fix.as_deref().is_some_and(|x| x.starts_with("doctor --fix")))
        {
            s += "\nrun `bpm doctor --fix` to repair the items marked `doctor --fix`\n";
        }
        s
    });
    Ok(())
}

/// Run every check, applying repairs when `fix`. Separate from rendering so that tests and
/// callers that only want the results do not have to parse the table.
pub fn findings(ctx: &Ctx, fix: bool) -> Vec<Finding> {
    let mut d = Doc { ctx, fix, out: vec![] };
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
    for unit in super::setup::TIMERS {
        let state = Command::new("systemctl")
            .args(["is-enabled", unit])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if state == "enabled" {
            d.ok(format!("{unit} enabled"));
        } else {
            d.warn(
                format!("{unit} is {}", if state.is_empty() { "not installed".into() } else { state }),
                Some("bpm setup".into()),
            );
        }
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
                    Some(format!(
                        "bpm status {0}; then bpm rollback {0} {1} or bpm unfreeze {0}",
                        f.name,
                        fr.ref_snap.map(|id| id.to_string()).unwrap_or_else(|| "held".into())
                    )),
                );
            }
            if let Some(e) = &st.last_error {
                d.warn(format!("{}: last tick error: {}", f.name, e.message), None);
            }
            if let Ok(eff) = project::effective(ctx, &root, &f.name, &f.path) {
                check_nested_leftovers(&mut d, &f.path, &eff.banlist);
                check_kept_leftovers(&mut d, &f.path, &eff.banlist_paths());
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
    check_snap_sudo(&mut d);
    d.out
}

/// The original tree left by an adopt interrupted after the swap: delete it only if it matches
/// the adopt snapshot of the project now at `orig`.
fn adopt_leftover_expect(
    ctx: &Ctx,
    store: &crate::store::Store,
    root: &crate::config::RootCfg,
    base: &str,
) -> Option<Expect<'static>> {
    let unit = store.unit(base);
    let rec = unit.read_record().ok()??;
    let snap =
        unit.snapshots().ok()?.into_iter().find(|m| m.kind == SnapshotKind::Adopt && rec.owns(Some(m.source_uuid)))?;
    let eff = project::effective(ctx, root, base, &root.path.join(base)).ok()?;
    let probe = |p: &Path, ino: u64| ctx.is_subvol(p, ino);
    let exclude = eff.banlist_paths();
    let stats = crate::util::walk::tree_stats(
        &unit.snapshot_path(snap.id),
        &probe,
        &exclude,
        &[],
        std::time::Duration::from_secs(24 * 3600),
    )
    .ok()?;
    Some(Expect::Tree { stats: Some(stats), not_after: snap.created, exclude })
}

fn check_root_leftovers(d: &mut Doc, root: &crate::config::RootCfg, store: &crate::store::Store) {
    let ctx = d.ctx;
    let Ok(rd) = std::fs::read_dir(&root.path) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let Some(leftover) = Leftover::parse(&name) else { continue };
        let orig = root.path.join(leftover.base());
        let is_sub = ctx.btrfs.is_subvolume(&path).unwrap_or(false);
        let orig_sub = ctx.btrfs.is_subvolume(&orig).unwrap_or(false);
        match leftover {
            Leftover::Keep { op, .. } => d.error(
                format!(
                    "{}: kept by an interrupted or unverified {op} because it may hold changes that exist nowhere else",
                    path.display()
                ),
                Some(format!("compare with {} (bpm diff), merge by hand, then delete it", orig.display())),
            ),
            Leftover::Tmp { .. } if is_sub && orig.is_dir() && !orig_sub => {
                let p = path.clone();
                d.fixable(
                    "warn",
                    format!("{}: adopt was interrupted before the swap", path.display()),
                    "doctor --fix deletes the partial copy (the original is untouched)".into(),
                    || ctx.btrfs.delete_subvolume(&p, true),
                );
            }
            Leftover::Tmp { ref base } | Leftover::Old { ref base } if !is_sub && orig_sub => {
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
                    "doctor --fix registers the project, then removes the old copy if it matches the adopt snapshot (otherwise keeps it as .bpm-keep-adopt-*)".into(),
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
                        match adopt_leftover_expect(ctx, store, &root_c, &base_s) {
                            Some(expect) => retire::retire(ctx, &p, &expect, &[], "adopt").map(|_| ()),
                            None => {
                                retire::retire(ctx, &p, &Expect::Tree { stats: None, not_after: jiff::Timestamp::MIN, exclude: vec![] }, &[], "adopt")
                                    .map(|_| ())
                            }
                        }
                    },
                );
            }
            Leftover::Tmp { .. } | Leftover::Old { .. } => {
                d.error(format!("{}: unexpected leftover; inspect manually", path.display()), None);
            }
            Leftover::Rollback { .. } if !orig.exists() => {
                let (p, o) = (path.clone(), orig.clone());
                d.fixable(
                    "error",
                    format!("{}: rollback was interrupted before the new tree existed", path.display()),
                    format!("doctor --fix renames it back to {}", orig.display()),
                    || ctx.fs.rename(&p, &o),
                );
            }
            Leftover::Rollback { ref base } => {
                // deletable only when a rollback snapshot of exactly this tree exists
                let uuid = ctx.btrfs.subvol_info(&path).ok().map(|i| i.uuid);
                let safety = store
                    .unit(base)
                    .snapshots()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|m| Some(m.source_uuid) == uuid && m.kind == SnapshotKind::Rollback)
                    .max_by_key(|m| m.id);
                let p = path.clone();
                d.fixable(
                    "warn",
                    format!(
                        "{}: pre-rollback tree left behind{}",
                        path.display(),
                        if safety.is_some() { " (it is also saved as a rollback snapshot)" } else { "" }
                    ),
                    "doctor --fix deletes it if it is identical to its rollback snapshot, otherwise keeps it as .bpm-keep-rollback-*".into(),
                    move || match &safety {
                        Some(m) => retire::retire(ctx, &p, &Expect::Snapshot { kept: m }, &[], "rollback").map(|_| ()),
                        None => retire::retire(ctx, &p, &Expect::Tree { stats: None, not_after: jiff::Timestamp::MIN, exclude: vec![] }, &[], "rollback").map(|_| ()),
                    },
                );
            }
        }
    }
}

fn check_nested_leftovers(d: &mut Doc, live: &Path, banlist: &[RelPath]) {
    let ctx = d.ctx;
    for rel in banlist {
        let Ok(full) = rel.under(live) else { continue };
        let full_sub = ctx.btrfs.is_subvolume(&full).unwrap_or(false);
        for suffix in [".bpm-tmp", ".bpm-old"] {
            let p = crate::util::fs::sibling(&full, suffix);
            if p.symlink_metadata().is_err() {
                continue;
            }
            let p_sub = ctx.btrfs.is_subvolume(&p).unwrap_or(false);
            let pc = p.clone();
            if p_sub && !full_sub {
                d.fixable(
                    "warn",
                    format!("{}: interrupted banlist conversion (copy not swapped)", p.display()),
                    "doctor --fix deletes the partial copy".into(),
                    || ctx.btrfs.delete_subvolume(&pc, true),
                );
            } else if !p_sub && full_sub {
                // unchanged since the nested subvolume was created: its contents were copied there
                let created = ctx.btrfs.subvol_info(&full).ok().and_then(|i| i.otime).unwrap_or(jiff::Timestamp::MIN);
                d.fixable(
                    "warn",
                    format!("{}: old build directory left after conversion", p.display()),
                    "doctor --fix removes it if unchanged since the conversion, otherwise keeps it as .bpm-keep-convert-*".into(),
                    move || {
                        retire::retire(ctx, &pc, &Expect::Tree { stats: None, not_after: created, exclude: vec![] }, &[], "convert")
                            .map(|_| ())
                    },
                );
            }
        }
    }
}

/// Trees and files `retire` kept anywhere in the project because it could not prove they were
/// already saved. They may hold the only copy of something, so doctor only reports them.
fn check_kept_leftovers(d: &mut Doc, live: &Path, exclude: &[PathBuf]) {
    let ctx = d.ctx;
    let (found, complete) = crate::util::walk::find_marker(
        live,
        &|p, i| ctx.is_subvol(p, i),
        exclude,
        retire::KEEP_MARKER,
        std::time::Duration::from_secs(30),
    );
    for p in found {
        d.error(
            format!("{}: kept because it may hold changes that exist nowhere else", p.display()),
            Some("inspect, merge by hand, then delete it".into()),
        );
    }
    if !complete {
        d.info(format!("{}: took too long to scan for kept leftovers; some may be unreported", live.display()));
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
                || ctx.fs.remove_file(&Journal::path_in(&dir)),
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
    let unit = unit.lock(std::time::Duration::ZERO)?;
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
    unit.write_meta_in(&unit.snapshot_dir(id), &meta)
}

/// As a normal user: can the Claude hook's `sudo -n bpm snap …` run without a password?
fn check_snap_sudo(d: &mut Doc) {
    if crate::privilege::is_root() || d.ctx.opts.no_sudo || !d.ctx.cfg.global.sudo {
        return;
    }
    let args: Vec<std::ffi::OsString> = ["snap", "doctor-probe", "--kind", "hook"].iter().map(Into::into).collect();
    let Ok(probe) = crate::privilege::self_command(&args, d.ctx.opts.config.as_deref(), true) else {
        return;
    };
    // `sudo -n -l <command…>` only checks whether the command is allowed
    let argv: Vec<std::ffi::OsString> = probe.get_args().map(|a| a.to_os_string()).collect();
    let allowed = Command::new("sudo")
        .arg("-n")
        .arg("-l")
        .args(argv.iter().skip(2))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if allowed {
        d.ok("passwordless sudo allows `bpm snap` (hook and wrap snapshots work)");
    } else {
        d.warn(
            "`sudo -n bpm snap …` needs a password: Claude hook and `bpm wrap` snapshots are skipped",
            Some("see docs/OPERATIONS.md (sudoers line for `bpm snap *`)".into()),
        );
    }
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
