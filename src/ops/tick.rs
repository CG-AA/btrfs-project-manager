//! `bpm tick`: the scheduled run. One failing project never blocks the others.

use crate::cli::TickArgs;
use crate::config::{AdoptMode, RootCfg};
use crate::ctx::Ctx;
use crate::error::BpmError;
use crate::hooks::{self, HookCtx, HookEvent};
use crate::mechanics::banlist::{self, BanAction};
use crate::mechanics::snapshot::{self, DeleteMode, SnapOpts};
use crate::mechanics::{Target, adopt, guard, recompress};
use crate::output::emit;
use crate::policy::change::{self, SnapDecision, SnapInputs};
use crate::policy::{lifecycle, thin};
use crate::project::{self, Found, ProjectRef};
use crate::store::{CONTAINER, ErrorRecord, Frozen, SnapshotKind, SnapshotMeta, Stage, Store, Unit, newest};
use crate::util::time::age;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Serialize, Default, Debug)]
pub struct ProjectTick {
    pub name: String,
    pub stage: String,
    pub snapshot: Option<u64>,
    pub deleted: Vec<u64>,
    pub banlist: Vec<BanAction>,
    pub froze: bool,
    pub error: Option<String>,
}

#[derive(Serialize, Default, Debug)]
pub struct RootTick {
    pub root: PathBuf,
    pub skipped: Option<String>,
    pub projects: Vec<ProjectTick>,
    pub container_snapshot: Option<u64>,
    pub container_deleted: Vec<u64>,
    pub renamed: Vec<(String, String)>,
    pub orphaned: Vec<String>,
    pub heavy: Option<String>,
    pub heavy_result: Option<String>,
}

#[derive(Debug)]
enum Heavy {
    Convert { name: String, rel: String },
    Adopt { name: String },
    Collapse { name: String },
    Recompress { name: String },
}

impl Heavy {
    fn priority(&self) -> u8 {
        match self {
            Heavy::Convert { .. } => 0,
            Heavy::Adopt { .. } => 1,
            Heavy::Collapse { .. } => 2,
            Heavy::Recompress { .. } => 3,
        }
    }
    fn describe(&self) -> String {
        match self {
            Heavy::Convert { name, rel } => format!("convert {name}/{rel}"),
            Heavy::Adopt { name } => format!("adopt {name}"),
            Heavy::Collapse { name } => format!("collapse {name}"),
            Heavy::Recompress { name } => format!("recompress {name}"),
        }
    }
}

const TMP_MAX_AGE: Duration = Duration::from_secs(600);
const PROJECT_LOCK: Duration = Duration::from_secs(5);

pub fn run(ctx: &Ctx, a: TickArgs) -> Result<()> {
    let mut reports = Vec::new();
    let mut failed = 0;
    for root in ctx.roots() {
        let (rt, f) = tick_root(ctx, &root, &a)?;
        failed += f;
        reports.push(rt);
    }
    emit(ctx, &reports, || {
        let mut lines = Vec::new();
        for r in &reports {
            if let Some(s) = &r.skipped {
                lines.push(format!("{}: skipped ({s})", r.root.display()));
                continue;
            }
            let snaps = r.projects.iter().filter(|p| p.snapshot.is_some()).count();
            let deleted: usize = r.projects.iter().map(|p| p.deleted.len()).sum();
            lines.push(format!(
                "{}: {} projects, {snaps} snapshots, {deleted} deleted{}{}",
                r.root.display(),
                r.projects.len(),
                r.heavy
                    .as_ref()
                    .map(|h| format!(", {h}: {}", r.heavy_result.clone().unwrap_or_default()))
                    .unwrap_or_default(),
                if failed > 0 { format!(", {failed} errors") } else { String::new() }
            ));
        }
        lines.join("\n")
    });
    if failed > 0 {
        return Err(BpmError::Partial { failed }.into());
    }
    Ok(())
}

fn tick_root(ctx: &Ctx, root: &RootCfg, a: &TickArgs) -> Result<(RootTick, usize)> {
    let mut rt = RootTick { root: root.path.clone(), ..Default::default() };
    let store = ctx.store(root);
    if !store.exists() {
        tracing::warn!("{}: store {} missing; run `bpm setup`", root.path.display(), store.dir.display());
        rt.skipped = Some("store missing".into());
        return Ok((rt, 0));
    }
    let _lock = match store.lock(a.lock_timeout) {
        Ok(l) => l,
        Err(e) if matches!(e.downcast_ref::<BpmError>(), Some(BpmError::Locked { .. })) => {
            tracing::info!("{}: another tick is running ({e})", root.path.display());
            rt.skipped = Some("locked".into());
            return Ok((rt, 0));
        }
        Err(e) => return Err(e),
    };
    let mut failed = 0;
    let now = ctx.now();
    for unit in store.units()?.into_iter().map(|(u, _)| u).chain(std::iter::once(store.container())) {
        if let Err(e) = cleanup_unit(ctx, &unit) {
            tracing::warn!("{}: cleanup: {e:#}", unit.name);
        }
    }
    let mut disc = project::discover(ctx, root, &store)?;
    if relink_renamed(ctx, &store, &disc, &mut rt)? {
        disc = project::discover(ctx, root, &store)?;
    }
    for (unit, rec) in &disc.missing {
        match handle_missing(ctx, root, unit, rec) {
            Ok(true) => rt.orphaned.push(unit.name.clone()),
            Ok(false) => {}
            Err(e) => {
                failed += 1;
                tracing::error!("{}: {e:#}", unit.name);
            }
        }
    }
    if a.projects.is_empty() {
        match container_step(ctx, root, &store) {
            Ok((s, d)) => {
                rt.container_snapshot = s;
                rt.container_deleted = d;
            }
            Err(e) => {
                failed += 1;
                tracing::error!("{}: container: {e:#}", root.path.display());
            }
        }
    }
    // Buffered writes reach the subvolume counters only at writeback: flush before deciding.
    if let Err(e) = ctx.btrfs.sync(&root.path) {
        tracing::warn!("{}: sync failed: {e:#}", root.path.display());
    }
    let mut heavy: Vec<Heavy> = Vec::new();
    for f in &disc.managed {
        if !a.projects.is_empty() && !a.projects.contains(&f.name) {
            continue;
        }
        let pref =
            ProjectRef { root: root.clone(), store: store.clone(), unit: f.unit.clone(), record: f.record.clone() };
        match project_step(ctx, &pref, f, &mut heavy) {
            Ok(pt) => rt.projects.push(pt),
            Err(e) => {
                failed += 1;
                let msg = format!("{e:#}");
                tracing::error!("{}: {msg}", f.name);
                if let Ok(mut st) = f.unit.read_state() {
                    st.last_error = Some(ErrorRecord { at: now, message: msg.clone() });
                    let _ = f.unit.write_state(&st);
                }
                rt.projects.push(ProjectTick { name: f.name.clone(), error: Some(msg), ..Default::default() });
            }
        }
    }
    if root.adopt == AdoptMode::Auto && a.projects.is_empty() {
        let mut attempts: BTreeMap<String, jiff::Timestamp> = read_attempts(&store);
        let mut cands: Vec<_> = disc
            .unadopted
            .iter()
            .filter(|c| {
                let probe = |p: &std::path::Path, ino: u64| ctx.is_subvol(p, ino);
                crate::util::walk::is_quiet(&c.path, &probe, root.adopt_min_age, 5000)
                    && project::effective(ctx, root, &c.name, &c.path).map(|e| e.managed).unwrap_or(false)
            })
            .collect();
        cands.sort_by_key(|c| attempts.get(&c.name).copied());
        if let Some(c) = cands.first() {
            attempts.insert(c.name.clone(), now);
            write_attempts(&store, &attempts);
            heavy.push(Heavy::Adopt { name: c.name.clone() });
        }
    }
    if !a.no_heavy && !heavy.is_empty() {
        let free = ctx.free_bytes(&root.path).unwrap_or(0);
        heavy.sort_by_key(|h| h.priority());
        let op = heavy.remove(0);
        rt.heavy = Some(op.describe());
        if free < ctx.cfg.global.heavy_min_free.0 {
            rt.heavy_result = Some(format!("skipped: only {} free", crate::util::bytes::fmt_bytes(free)));
        } else {
            rt.heavy_result = Some(match run_heavy(ctx, root, &store, &op) {
                Ok(msg) => {
                    tracing::info!("{}: {msg}", op.describe());
                    msg
                }
                Err(e) => {
                    let refused = matches!(crate::error::exit_code_for(&e), 7);
                    if refused {
                        tracing::warn!("{}: {e:#}", op.describe());
                    } else {
                        failed += 1;
                        tracing::error!("{}: {e:#}", op.describe());
                    }
                    format!("failed: {e:#}")
                }
            });
        }
    }
    Ok((rt, failed))
}

fn read_attempts(store: &Store) -> BTreeMap<String, jiff::Timestamp> {
    std::fs::read_to_string(store.dir.join("adopt-attempts.toml"))
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_attempts(store: &Store, m: &BTreeMap<String, jiff::Timestamp>) {
    if !store.dry_run {
        if let Ok(t) = toml::to_string(m) {
            let _ = crate::util::fs::write_atomic(&store.dir.join("adopt-attempts.toml"), t.as_bytes(), 0o644);
        }
    }
}

/// Remove stale `<id>.tmp` dirs and metadata whose snapshot is gone.
pub fn cleanup_unit(ctx: &Ctx, unit: &Unit) -> Result<()> {
    if ctx.opts.dry_run {
        return Ok(());
    }
    let scan = unit.scan()?;
    for tmp in scan.tmp_dirs {
        let old = std::fs::metadata(&tmp)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or_default() > TMP_MAX_AGE)
            .unwrap_or(false);
        if !old {
            continue;
        }
        let snap = tmp.join("snapshot");
        if snap.exists() && ctx.btrfs.is_subvolume(&snap).unwrap_or(false) {
            ctx.btrfs.delete_subvolume(&snap, false)?;
        }
        std::fs::remove_dir_all(&tmp)?;
        tracing::info!("{}: removed stale {}", unit.name, tmp.display());
    }
    for id in scan.without_snapshot {
        std::fs::remove_dir_all(unit.snapshot_dir(id))?;
        tracing::info!("{}: removed metadata of vanished snapshot #{id}", unit.name);
    }
    Ok(())
}

fn relink_renamed(ctx: &Ctx, store: &Store, disc: &project::Discovery, rt: &mut RootTick) -> Result<bool> {
    let mut changed = false;
    for f in disc.managed.iter().filter(|f| f.name != f.unit.name) {
        let target = store.unit(&f.name);
        if target.dir.exists() {
            match target.read_record()? {
                Some(rec) => {
                    let aside = store.unit(&format!("{}@{}", f.name, rec.uuid.short()));
                    if !ctx.opts.dry_run {
                        std::fs::rename(&target.dir, &aside.dir)?;
                        let mut rec = rec;
                        rec.name = aside.name.clone();
                        aside.write_record(&rec)?;
                    }
                    tracing::warn!("store entry {} moved to {} to make room for renamed project", f.name, aside.name);
                }
                None => {
                    tracing::error!(
                        "cannot relink {} -> {}: {} exists without project.toml",
                        f.unit.name,
                        f.name,
                        target.dir.display()
                    );
                    continue;
                }
            }
        }
        if !ctx.opts.dry_run {
            std::fs::rename(&f.unit.dir, &target.dir)?;
            let mut rec = f.record.clone();
            rec.name = f.name.clone();
            rec.path = f.path.clone();
            target.write_record(&rec)?;
        }
        tracing::info!("project renamed: {} -> {}", f.unit.name, f.name);
        rt.renamed.push((f.unit.name.clone(), f.name.clone()));
        changed = true;
    }
    Ok(changed)
}

fn handle_missing(ctx: &Ctx, root: &RootCfg, unit: &Unit, rec: &crate::store::ProjectRecord) -> Result<bool> {
    let _l = match unit.lock(PROJECT_LOCK) {
        Ok(l) => l,
        Err(e) if crate::error::exit_code_for(&e) == 4 => return Ok(false),
        Err(e) => return Err(e),
    };
    // re-check under the lock: a rollback or recreate may have just replaced the subvolume
    let rec = &unit.read_record()?.unwrap_or_else(|| rec.clone());
    if ctx.btrfs.subvol_info(&rec.path).map(|i| i.uuid == rec.uuid).unwrap_or(false)
        && ctx.btrfs.is_subvolume(&rec.path).unwrap_or(false)
    {
        return Ok(false);
    }
    let mut st = unit.read_state()?;
    if matches!(st.stage, Stage::Orphaned | Stage::Archived) {
        return Ok(false);
    }
    let now = ctx.now();
    if let Ok(Some(p)) = ctx.btrfs.find_by_uuid(&root.path, rec.uuid) {
        if !st.warned.contains_key("moved") {
            tracing::warn!(
                "{}: project subvolume moved out of {} (now at {}); not snapshotted until it returns",
                rec.name,
                root.path.display(),
                p.display()
            );
            st.warned.insert("moved".into(), now);
            unit.write_state(&st)?;
        }
        return Ok(false);
    }
    let snaps = unit.snapshots()?;
    let newest_id = newest(&snaps).map(|m| m.id);
    if let Some(mut m) = newest(&snaps).cloned() {
        if !m.hold {
            m.hold = true;
            m.hold_note = "auto-held: project directory disappeared".into();
            unit.update_meta(&m)?;
        }
    }
    st.set_stage(Stage::Orphaned, now);
    st.missing_since = Some(now);
    if st.frozen.is_none() {
        st.frozen = Some(Frozen {
            since: now,
            reasons: vec!["live project directory disappeared".into()],
            trigger_snap: None,
            ref_snap: newest_id,
        });
    }
    unit.write_state(&st)?;
    tracing::error!(
        "{}: project directory {} disappeared; its {} snapshots are kept. Recreate it with `bpm restore {} --recreate`",
        rec.name,
        rec.path.display(),
        snaps.len(),
        rec.name
    );
    let h = HookCtx {
        project: Some(&rec.name),
        project_path: Some(&rec.path),
        owner: Some((rec.owner_uid, rec.owner_gid)),
        root: Some(&root.path),
        reason: "project directory disappeared".into(),
        ..Default::default()
    };
    let _ = hooks::run(ctx, HookEvent::OnOrphaned, &h);
    let _ = hooks::run(ctx, HookEvent::OnFreeze, &h);
    Ok(true)
}

fn container_step(ctx: &Ctx, root: &RootCfg, store: &Store) -> Result<(Option<u64>, Vec<u64>)> {
    if !root.container.enabled {
        return Ok((None, vec![]));
    }
    let unit = store.container();
    unit.ensure_dir()?;
    let _l = unit.lock(PROJECT_LOCK)?;
    let rec = super::snap::container_record(ctx, root, &unit)?;
    let now = ctx.now();
    let info = ctx.btrfs.subvol_info(&root.path)?;
    let mut snaps = unit.snapshots()?;
    let mut took = None;
    let due = newest(&snaps).is_none_or(|n| age(now, n.created) >= root.container.interval);
    if due && change::changed_since(&info, newest(&snaps)) {
        let t = Target {
            unit: &unit,
            name: CONTAINER,
            live: &root.path,
            root: &root.path,
            expected_uuid: Some(rec.uuid),
            owner: None,
            stats_exclude: vec![],
            sentinels: vec![],
        };
        let mut o = SnapOpts::new(SnapshotKind::Auto, "container");
        o.stats = false;
        let m = snapshot::take(ctx, &t, &o)?;
        took = Some(m.id);
        snaps.push(m);
    }
    let mut deleted = Vec::new();
    for id in thin::select_deletions(&snaps, now, &thin::Windows::from(&root.container), None, true) {
        let Some(m) = snaps.iter().find(|m| m.id == id).cloned() else {
            continue;
        };
        match snapshot::delete(ctx, &unit, &rec, &snaps, &m, DeleteMode::Policy, "container retention") {
            Ok(()) => {
                deleted.push(id);
                snaps.retain(|s| s.id != id);
            }
            Err(e) => tracing::warn!("container: could not delete #{id}: {e:#}"),
        }
    }
    Ok((took, deleted))
}

fn want_stats(
    eff: &crate::config::EffectiveConfig,
    st: &crate::store::ProjectState,
    snaps: &[SnapshotMeta],
    now: jiff::Timestamp,
) -> bool {
    if !eff.policy.snapshot.stats {
        return false;
    }
    let last_walk_ms = snaps.iter().rev().find_map(|m| m.stats.as_ref().map(|s| s.walk_ms));
    match (st.last_stats_at, last_walk_ms) {
        (None, _) | (_, None) => true,
        (_, Some(ms)) if ms < 2000 => true,
        (Some(t), _) => age(now, t) >= eff.policy.snapshot.stats_min_interval,
    }
}

fn project_step(ctx: &Ctx, pref: &ProjectRef, f: &Found, heavy: &mut Vec<Heavy>) -> Result<ProjectTick> {
    let mut pt = ProjectTick { name: f.name.clone(), ..Default::default() };
    let _lock = match pref.unit.lock(PROJECT_LOCK) {
        Ok(l) => l,
        Err(e) if crate::error::exit_code_for(&e) == 4 => {
            tracing::info!("{}: busy ({e}); skipped this tick", f.name);
            pt.stage = "busy".into();
            return Ok(pt);
        }
        Err(e) => return Err(e),
    };
    // the project may have changed identity (rollback, recreate) since discovery
    let current = pref.unit.read_record()?.unwrap_or_else(|| pref.record.clone());
    if current.uuid != f.info.uuid || ctx.btrfs.subvol_info(pref.path()).map(|i| i.uuid != current.uuid).unwrap_or(true)
    {
        tracing::info!("{}: changed during discovery; skipped this tick", f.name);
        pt.stage = "busy".into();
        return Ok(pt);
    }
    let eff = super::effective(ctx, pref)?;
    let mut st = pref.unit.read_state()?;
    let now = ctx.now();
    if !eff.managed {
        pt.stage = format!("{} (managed = false)", st.stage);
        return Ok(pt);
    }
    if st.stage == Stage::Orphaned {
        tracing::info!("{}: project directory is back", f.name);
        st.missing_since = None;
        st.set_stage(Stage::Active, now);
    }
    let mut snaps = pref.unit.snapshots()?;
    if let Some(n) = newest(&snaps) {
        if n.source_uuid == pref.record.uuid {
            st.snap_ctransid = n.source_ctransid;
        }
    }
    let owner = (pref.record.owner_uid, pref.record.owner_gid);
    let active = st.stage == Stage::Active;
    pt.banlist = banlist::enforce_cheap(ctx, pref.path(), &eff, &mut st, owner, active)?;
    if pt.banlist.iter().any(BanAction::modified_live) {
        ctx.btrfs.sync(pref.path())?;
        st.tool_ctransid = ctx.btrfs.subvol_info(pref.path())?.ctransid;
    }
    let live = ctx.btrfs.subvol_info(pref.path())?;
    if change::user_changed(&live, &st) {
        st.last_change_at = Some(live.ctime.unwrap_or(now).min(now));
    }
    let idle = age(now, st.last_change_at.unwrap_or(pref.record.adopted));
    if let Some((from, to)) = st.set_stage(lifecycle::stage_for_idle(idle, &eff.policy.lifecycle), now) {
        tracing::info!("{}: {from} -> {to}", f.name);
        let h = HookCtx {
            project: Some(&f.name),
            project_path: Some(pref.path()),
            owner: Some(owner),
            root: Some(&pref.root.path),
            stage: Some((from, to)),
            reason: "idle".into(),
            ..Default::default()
        };
        let _ = hooks::run(ctx, HookEvent::StageChange, &h);
    }
    let target = Target::for_project(pref, &eff);
    let free = ctx.free_bytes(pref.path())?;
    let decision = change::decide_auto_snapshot(&SnapInputs {
        live: &live,
        newest: newest(&snaps),
        now,
        min_interval: eff.policy.snapshot.min_interval,
        free,
        min_free: eff.policy.thin.min_free.0,
    });
    match decision {
        SnapDecision::Take(reason) => {
            let mut o = SnapOpts::new(SnapshotKind::Auto, reason);
            o.stats = want_stats(&eff, &st, &snaps, now);
            o.stats_budget = eff.policy.snapshot.stats_budget;
            let m = snapshot::take(ctx, &target, &o)?;
            st.snap_ctransid = m.source_ctransid;
            st.last_snapshot_at = Some(m.created);
            if m.stats.is_some() {
                st.last_stats_at = Some(m.created);
            }
            pt.snapshot = Some(m.id);
            snaps.push(m.clone());
            pt.froze = guard::apply(ctx, &target, &eff, &mut st, &mut snaps, &m);
        }
        SnapDecision::Skip("low free space") => {
            if !st.warned.contains_key("low-space") {
                tracing::warn!("{}: changed but free space is below thin.min_free; auto snapshots paused", f.name);
                st.warned.insert("low-space".into(), now);
            }
        }
        SnapDecision::Skip(_) => {
            st.warned.remove("low-space");
        }
    }
    let windows = thin::Windows::from(&eff.policy.thin);
    for id in
        thin::select_deletions(&snaps, now, &windows, st.frozen.as_ref(), eff.policy.shrink_guard.thin_after_freeze)
    {
        let Some(m) = snaps.iter().find(|m| m.id == id).cloned() else {
            continue;
        };
        match snapshot::delete(ctx, &pref.unit, &pref.record, &snaps, &m, DeleteMode::Policy, "retention") {
            Ok(()) => {
                pt.deleted.push(id);
                snaps.retain(|s| s.id != id);
            }
            Err(e) => tracing::warn!("{}: could not delete #{id}: {e:#}", f.name),
        }
    }
    for (rel, since) in &st.pending_convert {
        if banlist::ready_to_convert(ctx, pref.path(), rel, &eff) {
            heavy.push(Heavy::Convert { name: f.name.clone(), rel: rel.clone() });
            break;
        } else if age(now, *since) > Duration::from_secs(86400)
            && !st.warned.contains_key(&format!("convert-stuck:{rel}"))
        {
            tracing::warn!(
                "{}: banned directory {rel} is still a plain directory after a day (always busy?); run `bpm convert {} {rel}`",
                f.name,
                f.name
            );
        }
    }
    if st.stage == Stage::Cold
        && st.frozen.is_none()
        && age(now, pref.record.adopted) >= eff.policy.lifecycle.adopt_grace
    {
        let live_now = ctx.btrfs.subvol_info(pref.path())?;
        let identical = newest(&snaps).is_some_and(|n| change::identical(&live_now, n));
        if !recompress::collapsed(ctx, &snaps, &eff) || !identical {
            heavy.push(Heavy::Collapse { name: f.name.clone() });
        } else if recompress::needs_recompress(&eff, &st, live_now.ctransid, &snaps) {
            heavy.push(Heavy::Recompress { name: f.name.clone() });
        }
    }
    st.last_tick = Some(now);
    st.last_error = None;
    pt.stage = st.stage.to_string();
    pref.unit.write_state(&st)?;
    Ok(pt)
}

fn run_heavy(ctx: &Ctx, root: &RootCfg, store: &Store, op: &Heavy) -> Result<String> {
    let with_project = |name: &str| -> Result<(ProjectRef, crate::config::EffectiveConfig)> {
        let pref = project::resolve_in(ctx, root, name)?;
        let eff = super::effective(ctx, &pref)?;
        Ok((pref, eff))
    };
    let result: Result<String> = match op {
        Heavy::Adopt { name } => {
            let r = adopt::adopt(
                ctx,
                root,
                name,
                &adopt::AdoptOpts { keep_build: None, force: false, verify_paths: false, require_unused: true },
            )?;
            Ok(format!("adopted ({} files, snapshot #{})", r.files, r.snapshot))
        }
        Heavy::Convert { name, rel } => {
            let (pref, eff) = with_project(name)?;
            let _l = pref.unit.lock(PROJECT_LOCK)?;
            banlist::convert(ctx, pref.path(), rel, eff.policy.keep_build_on_adopt, false)?;
            let mut st = pref.unit.read_state()?;
            st.pending_convert.remove(rel);
            ctx.btrfs.sync(pref.path())?;
            st.tool_ctransid = ctx.btrfs.subvol_info(pref.path())?.ctransid;
            pref.unit.write_state(&st)?;
            Ok("converted".into())
        }
        Heavy::Collapse { name } => {
            let (pref, eff) = with_project(name)?;
            let _l = pref.unit.lock(PROJECT_LOCK)?;
            let mut st = pref.unit.read_state()?;
            let mut snaps = pref.unit.snapshots()?;
            let r = recompress::collapse(ctx, &pref, &eff, &mut st, &mut snaps)?;
            pref.unit.write_state(&st)?;
            Ok(format!("{} deleted, {} kept", r.deleted.len(), snaps.len()))
        }
        Heavy::Recompress { name } => {
            let (pref, eff) = with_project(name)?;
            let _l = pref.unit.lock(PROJECT_LOCK)?;
            let mut st = pref.unit.read_state()?;
            let mut snaps = pref.unit.snapshots()?;
            let r = recompress::recompress(ctx, &pref, &eff, &mut st, &mut snaps, eff.policy.recompress.level, false);
            pref.unit.write_state(&st)?;
            let r = r?;
            Ok(if r.done {
                format!("zstd:{} done", r.level)
            } else {
                format!("skipped: {}", r.skipped.unwrap_or_default())
            })
        }
    };
    result.with_context(|| format!("{} under {}", op.describe(), store.root.display()))
}
