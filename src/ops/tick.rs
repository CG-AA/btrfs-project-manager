//! `bpm tick`: the scheduled run. One failing project never blocks the others.
//!
//! The light part (discovery, banlists, snapshots, guard, retention) runs under the store lock
//! every few minutes. Heavy operations (adopt, convert, collapse, recompress) run after that lock
//! is released, under a separate heavy lock, so a long copy or defragment never delays the
//! snapshots of other projects. The systemd units run the two parts as separate services
//! (`tick --no-heavy` and `tick --heavy-only`); a plain `bpm tick` does both.

use crate::cli::TickArgs;
use crate::config::{AdoptMode, EffectiveConfig, RootCfg};
use crate::ctx::Ctx;
use crate::error::BpmError;
use crate::hooks::{self, HookCtx, HookEvent};
use crate::mechanics::banlist::{self, BanAction};
use crate::mechanics::observe::{self, Walk};
use crate::mechanics::snapshot::{self, DeleteMode, SnapOpts};
use crate::mechanics::{Target, adopt, recompress};
use crate::output::emit;
use crate::policy::change::{self, SnapDecision, SnapInputs};
use crate::policy::thin;
use crate::project::{self, Found, ProjectRef};
use crate::store::{
    CONTAINER, ErrorRecord, Frozen, LockedUnit, ProjectState, SnapshotKind, SnapshotMeta, Stage, Store, Unit, newest,
};
use crate::util::time::age;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone)]
pub enum Heavy {
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
    /// Key for retry backoff.
    fn key(&self) -> String {
        match self {
            Heavy::Convert { name, rel } => format!("convert:{name}/{rel}"),
            Heavy::Adopt { name } => format!("adopt:{name}"),
            Heavy::Collapse { name } => format!("collapse:{name}"),
            Heavy::Recompress { name } => format!("recompress:{name}"),
        }
    }
}

const TMP_MAX_AGE: Duration = Duration::from_secs(3600);
const PROJECT_LOCK: Duration = Duration::from_secs(5);
const BACKOFF_BASE: Duration = Duration::from_secs(3600);
const BACKOFF_MAX: Duration = Duration::from_secs(24 * 3600);

fn is_busy(e: &anyhow::Error) -> bool {
    crate::error::exit_code_for(e) == 4
}

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
    let store_lock = match store.lock(a.lock_timeout) {
        Ok(l) => l,
        Err(e) if matches!(e.downcast_ref::<BpmError>(), Some(BpmError::Locked { .. })) => {
            tracing::info!("{}: another tick is running ({e})", root.path.display());
            rt.skipped = Some("locked".into());
            return Ok((rt, 0));
        }
        Err(e) => return Err(e),
    };
    let mut failed = 0;
    let mut heavy: Vec<Heavy> = Vec::new();
    if a.heavy_only {
        let disc = project::discover(ctx, root, &store)?;
        if let Err(e) = ctx.btrfs.sync(&root.path) {
            tracing::warn!("{}: sync failed: {e:#}", root.path.display());
        }
        for f in &disc.managed {
            match heavy_for_project(ctx, root, &store, f) {
                Ok(h) => heavy.extend(h),
                Err(e) => tracing::warn!("{}: {e:#}", f.name),
            }
        }
        heavy.extend(adopt_candidate(ctx, root, &store, &disc));
    } else {
        failed += light_part(ctx, root, &store, a, &mut rt, &mut heavy)?;
    }
    drop(store_lock);
    if !a.no_heavy {
        failed += heavy_part(ctx, root, &store, heavy, &mut rt);
    }
    Ok((rt, failed))
}

/// Discovery, renames, deleted projects, container, per-project snapshots and retention.
/// Collects heavy candidates instead of running them.
fn light_part(
    ctx: &Ctx,
    root: &RootCfg,
    store: &Store,
    a: &TickArgs,
    rt: &mut RootTick,
    heavy: &mut Vec<Heavy>,
) -> Result<usize> {
    let mut failed = 0;
    let now = ctx.now();
    for unit in store.units()?.into_iter().map(|(u, _)| u).chain(std::iter::once(store.container())) {
        if let Err(e) = cleanup_unit(ctx, &unit) {
            tracing::warn!("{}: cleanup: {e:#}", unit.name);
        }
    }
    let mut disc = project::discover(ctx, root, store)?;
    if relink_renamed(store, &disc, rt)? {
        disc = project::discover(ctx, root, store)?;
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
    // Buffered writes reach the subvolume counters only at writeback: flush before deciding.
    if let Err(e) = ctx.btrfs.sync(&root.path) {
        tracing::warn!("{}: sync failed: {e:#}", root.path.display());
    }
    if a.projects.is_empty() {
        match container_step(ctx, root, store) {
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
    for f in &disc.managed {
        if !a.projects.is_empty() && !a.projects.contains(&f.name) {
            continue;
        }
        let pref =
            ProjectRef { root: root.clone(), store: store.clone(), unit: f.unit.clone(), record: f.record.clone() };
        match project_step(ctx, &pref, f, heavy) {
            Ok(pt) => rt.projects.push(pt),
            Err(e) => {
                failed += 1;
                let msg = format!("{e:#}");
                tracing::error!("{}: {msg}", f.name);
                // recorded only under the lock, so a concurrent freeze or unfreeze is never undone
                if let Ok(lu) = f.unit.lock(Duration::ZERO) {
                    if let Ok(mut st) = lu.read_state() {
                        st.last_error = Some(ErrorRecord { at: now, message: msg.clone() });
                        let _ = lu.write_state(&st);
                    }
                }
                rt.projects.push(ProjectTick { name: f.name.clone(), error: Some(msg), ..Default::default() });
            }
        }
    }
    if a.projects.is_empty() {
        heavy.extend(adopt_candidate(ctx, root, store, &disc));
    }
    Ok(failed)
}

/// Pick and run one heavy operation that is not backing off after a failure.
fn heavy_part(ctx: &Ctx, root: &RootCfg, store: &Store, heavy: Vec<Heavy>, rt: &mut RootTick) -> usize {
    let now = ctx.now();
    let mut attempts = Attempts::read(store);
    let mut ready: Vec<Heavy> = heavy.into_iter().filter(|h| !attempts.backing_off(&h.key(), now)).collect();
    if ready.is_empty() {
        return 0;
    }
    ready.sort_by_key(|h| h.priority());
    let op = ready.remove(0);
    rt.heavy = Some(op.describe());
    let free = ctx.free_bytes(&root.path).unwrap_or(0);
    if free < ctx.cfg.global.heavy_min_free.0 {
        rt.heavy_result = Some(format!("skipped: only {} free", crate::util::bytes::fmt_bytes(free)));
        return 0;
    }
    let _heavy_lock = match store.heavy_lock() {
        Ok(l) => l,
        Err(e) if is_busy(&e) => {
            rt.heavy_result = Some("skipped: another heavy operation is running".into());
            return 0;
        }
        Err(e) => {
            rt.heavy_result = Some(format!("failed: {e:#}"));
            return 1;
        }
    };
    let mut failed = 0;
    let result = run_heavy(ctx, root, store, &op);
    rt.heavy_result = Some(match &result {
        Ok(msg) => {
            tracing::info!("{}: {msg}", op.describe());
            msg.clone()
        }
        Err(e) => {
            let refused = matches!(crate::error::exit_code_for(e), 7);
            if refused {
                tracing::warn!("{}: {e:#}", op.describe());
            } else {
                failed += 1;
                tracing::error!("{}: {e:#}", op.describe());
            }
            format!("failed: {e:#}")
        }
    });
    attempts.record(&op.key(), now, result.is_ok());
    failed
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Attempt {
    at: jiff::Timestamp,
    #[serde(default)]
    failures: u32,
}

/// Last attempt per heavy operation, so a candidate that keeps failing (nested subvolumes, a
/// shell open inside) backs off instead of taking the only heavy slot every tick.
struct Attempts {
    path: PathBuf,
    map: BTreeMap<String, Attempt>,
    dry_run: bool,
}

impl Attempts {
    fn read(store: &Store) -> Attempts {
        let path = store.dir.join("heavy-attempts.toml");
        let map = std::fs::read_to_string(&path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default();
        Attempts { path, map, dry_run: store.dry_run }
    }

    fn backing_off(&self, key: &str, now: jiff::Timestamp) -> bool {
        self.map.get(key).is_some_and(|a| {
            a.failures > 0
                && age(now, a.at) < BACKOFF_BASE.saturating_mul(1 << (a.failures - 1).min(8)).min(BACKOFF_MAX)
        })
    }

    fn last(&self, key: &str) -> Option<jiff::Timestamp> {
        self.map.get(key).map(|a| a.at)
    }

    fn record(&mut self, key: &str, now: jiff::Timestamp, ok: bool) {
        let failures = if ok { 0 } else { self.map.get(key).map(|a| a.failures).unwrap_or(0) + 1 };
        self.map.insert(key.to_string(), Attempt { at: now, failures });
        if !self.dry_run {
            if let Ok(t) = toml::to_string(&self.map) {
                let _ = crate::util::fs::write_atomic(&self.path, t.as_bytes(), 0o644);
            }
        }
    }
}

/// The quiet, unmanaged top-level directory (or subvolume) adopted least recently.
fn adopt_candidate(ctx: &Ctx, root: &RootCfg, store: &Store, disc: &project::Discovery) -> Option<Heavy> {
    if root.adopt != AdoptMode::Auto {
        return None;
    }
    let attempts = Attempts::read(store);
    let now = ctx.now();
    let mut cands: Vec<_> = disc
        .unadopted
        .iter()
        // registering an existing subvolume copies nothing
        .chain(disc.foreign_subvols.iter())
        .filter(|c| !attempts.backing_off(&format!("adopt:{}", c.name), now))
        .filter(|c| {
            let probe = |p: &std::path::Path, ino: u64| ctx.is_subvol(p, ino);
            crate::util::walk::is_quiet(&c.path, &probe, root.adopt_min_age, 5000)
                && project::effective(ctx, root, &c.name, &c.path).map(|e| e.managed).unwrap_or(false)
        })
        .collect();
    cands.sort_by_key(|c| attempts.last(&format!("adopt:{}", c.name)));
    cands.first().map(|c| Heavy::Adopt { name: c.name.clone() })
}

/// Remove stale `<id>.tmp` dirs and metadata whose snapshot is gone. A unit whose lock is held
/// is skipped: its owner may still be writing into a tmp dir (a long `btrfs receive`).
pub fn cleanup_unit(ctx: &Ctx, unit: &Unit) -> Result<()> {
    if !unit.dir.exists() {
        return Ok(());
    }
    let lu = match unit.lock(Duration::ZERO) {
        Ok(l) => l,
        Err(e) if is_busy(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    let scan = lu.scan()?;
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
            // only a read-only snapshot of this unit's project is bpm's own leftover; anything else
            // (an interrupted snapper import) is kept for doctor
            let rec = lu.read_record()?;
            let imported = std::fs::read_to_string(tmp.join("meta.toml"))
                .ok()
                .and_then(|t| toml::from_str::<SnapshotMeta>(&t).ok())
                .is_some_and(|m| m.kind.class() == crate::store::KindClass::Keep);
            let ours = ctx
                .btrfs
                .subvol_info(&snap)
                .is_ok_and(|i| i.readonly() && rec.as_ref().is_some_and(|r| r.owns(i.parent_uuid)));
            if !ours || imported {
                tracing::warn!(
                    "{}: {} is not an abandoned snapshot of this project; left for `bpm doctor`",
                    lu.name,
                    tmp.display()
                );
                continue;
            }
            ctx.btrfs.delete_subvolume(&snap, false)?;
        }
        ctx.fs.remove_dir_all(&tmp)?;
        tracing::info!("{}: removed stale {}", lu.name, tmp.display());
    }
    for id in scan.without_snapshot {
        ctx.fs.remove_dir_all(&lu.snapshot_dir(id))?;
        tracing::info!("{}: removed metadata of vanished snapshot #{id}", lu.name);
    }
    Ok(())
}

/// Move store units to follow renamed project directories. Works in two passes so a chain
/// (`mv app app-old; mv app-v2 app`) or swap never writes one project's record into another's
/// unit: first every source is re-verified by uuid, gets its new record and is parked under a
/// unique name; then whatever holds each final name is moved aside and the parked unit takes it.
fn relink_renamed(store: &Store, disc: &project::Discovery, rt: &mut RootTick) -> Result<bool> {
    let mut parked: Vec<(LockedUnit, String, String)> = Vec::new();
    for f in disc.managed.iter().filter(|f| f.name != f.unit.name) {
        let lu = match f.unit.lock(PROJECT_LOCK) {
            Ok(l) => l,
            Err(e) if is_busy(&e) => {
                tracing::info!("{}: busy; relink to {} next tick", f.unit.name, f.name);
                continue;
            }
            Err(e) => return Err(e),
        };
        let Some(mut rec) = lu.read_record()?.filter(|r| r.uuid == f.info.uuid) else {
            tracing::warn!("{}: record changed since discovery; relink to {} next tick", f.unit.name, f.name);
            continue;
        };
        rec.name = f.name.clone();
        rec.path = f.path.clone();
        // written before any rename: an interruption leaves a record that still matches by uuid
        lu.write_record(&rec)?;
        let park_name = format!("{}@relink-{}", f.name, rec.uuid.short());
        let lu = if lu.name == park_name {
            lu
        } else {
            let park = store.unit(&park_name);
            if park.dir.exists() {
                tracing::error!("cannot relink {} -> {}: {} exists", f.unit.name, f.name, park.dir.display());
                continue;
            }
            lu.rename_to(park)?
        };
        parked.push((lu, f.name.clone(), f.unit.name.clone()));
    }
    let mut changed = false;
    for (lu, name, from) in parked {
        let target = store.unit(&name);
        if target.dir.exists() {
            let Some(mut rec) = target.read_record()? else {
                tracing::error!(
                    "cannot relink {from} -> {name}: {} exists without project.toml (store entry left at {})",
                    target.dir.display(),
                    lu.dir.display()
                );
                continue;
            };
            let aside = store.unit(&format!("{name}@{}", rec.uuid.short()));
            if aside.dir.exists() {
                tracing::error!("cannot relink {from} -> {name}: {} exists", aside.dir.display());
                continue;
            }
            // busy is transient: leave the entry parked and relink on a later tick rather than
            // failing every remaining project in this root
            let moved = match target.lock(PROJECT_LOCK) {
                Ok(l) => l.rename_to(aside)?,
                Err(e) if is_busy(&e) => {
                    tracing::info!("{name}: busy; relink {from} -> {name} next tick");
                    continue;
                }
                Err(e) => return Err(e),
            };
            rec.name = moved.name.clone();
            moved.write_record(&rec)?;
            tracing::warn!("store entry {name} moved to {} to make room for renamed project", moved.name);
        }
        lu.rename_to(target)?;
        tracing::info!("project renamed: {from} -> {name}");
        rt.renamed.push((from, name));
        changed = true;
    }
    Ok(changed)
}

fn handle_missing(ctx: &Ctx, root: &RootCfg, unit: &Unit, rec: &crate::store::ProjectRecord) -> Result<bool> {
    let lu = match unit.lock(PROJECT_LOCK) {
        Ok(l) => l,
        Err(e) if is_busy(&e) => return Ok(false),
        Err(e) => return Err(e),
    };
    // re-check under the lock: a rollback or recreate may have just replaced the subvolume
    let rec = &lu.read_record()?.unwrap_or_else(|| rec.clone());
    if ctx.btrfs.subvol_info(&rec.path).map(|i| i.uuid == rec.uuid).unwrap_or(false)
        && ctx.btrfs.is_subvolume(&rec.path).unwrap_or(false)
    {
        return Ok(false);
    }
    let mut st = lu.read_state()?;
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
            lu.write_state(&st)?;
        }
        return Ok(false);
    }
    let snaps = lu.snapshots()?;
    let newest_id = newest(&snaps).map(|m| m.id);
    if let Some(mut m) = newest(&snaps).cloned() {
        if !m.hold {
            m.hold = true;
            m.hold_note = "auto-held: project directory disappeared".into();
            lu.update_meta(&m)?;
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
    lu.write_state(&st)?;
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
    if !ctx.btrfs.is_subvolume(&root.path)? {
        // reported by `bpm setup` and `bpm doctor`; not an error on every tick
        tracing::debug!("{} is not a subvolume root; no container snapshots", root.path.display());
        return Ok((None, vec![]));
    }
    let unit = store.container().lock(PROJECT_LOCK)?;
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

/// Heavy work this project needs: a pending build-dir conversion, or collapse/recompress when
/// cold. Shared by the light tick and `tick --heavy-only`, so both choose the same way.
fn heavy_candidates(
    ctx: &Ctx,
    pref: &ProjectRef,
    eff: &EffectiveConfig,
    st: &ProjectState,
    snaps: &[SnapshotMeta],
    now: jiff::Timestamp,
) -> Result<Vec<Heavy>> {
    let mut out = Vec::new();
    if let Some(rel) = st.pending_convert.keys().find(|rel| banlist::ready_to_convert(ctx, pref.path(), rel, eff)) {
        out.push(Heavy::Convert { name: pref.name().to_string(), rel: rel.clone() });
    }
    if st.stage == Stage::Cold
        && st.frozen.is_none()
        && age(now, pref.record.adopted) >= eff.policy.lifecycle.adopt_grace
    {
        let live_now = ctx.btrfs.subvol_info(pref.path())?;
        let identical = newest(snaps).is_some_and(|n| change::identical(&live_now, n));
        let counted = newest(snaps).is_some_and(|n| n.complete_stats().is_some());
        if st.collapse_incomplete_ctransid == Some(live_now.ctransid) {
            // counting this state already exceeded the stats budget; wait for a change
        } else if !recompress::collapsed(ctx, snaps, eff) || !identical || !counted {
            out.push(Heavy::Collapse { name: pref.name().to_string() });
        } else if recompress::needs_recompress(eff, st, live_now.ctransid, snaps) {
            out.push(Heavy::Recompress { name: pref.name().to_string() });
        }
    }
    Ok(out)
}

fn heavy_for_project(ctx: &Ctx, root: &RootCfg, store: &Store, f: &Found) -> Result<Vec<Heavy>> {
    let pref = ProjectRef { root: root.clone(), store: store.clone(), unit: f.unit.clone(), record: f.record.clone() };
    let lu = match pref.unit.lock(PROJECT_LOCK) {
        Ok(l) => l,
        Err(e) if is_busy(&e) => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let eff = super::effective(ctx, &pref)?;
    if !eff.managed {
        return Ok(vec![]);
    }
    let st = lu.read_state()?;
    let snaps = lu.snapshots()?;
    heavy_candidates(ctx, &pref, &eff, &st, &snaps, ctx.now())
}

fn project_step(ctx: &Ctx, pref: &ProjectRef, f: &Found, heavy: &mut Vec<Heavy>) -> Result<ProjectTick> {
    let mut pt = ProjectTick { name: f.name.clone(), ..Default::default() };
    let lu = match pref.unit.lock(PROJECT_LOCK) {
        Ok(l) => l,
        Err(e) if is_busy(&e) => {
            tracing::info!("{}: busy ({e}); skipped this tick", f.name);
            pt.stage = "busy".into();
            return Ok(pt);
        }
        Err(e) => return Err(e),
    };
    // the project may have changed identity (rollback, recreate) since discovery
    let current = lu.read_record()?.unwrap_or_else(|| pref.record.clone());
    if current.uuid != f.info.uuid || ctx.btrfs.subvol_info(pref.path()).map(|i| i.uuid != current.uuid).unwrap_or(true)
    {
        tracing::info!("{}: changed during discovery; skipped this tick", f.name);
        pt.stage = "busy".into();
        return Ok(pt);
    }
    let eff = super::effective(ctx, pref)?;
    let mut st = lu.read_state()?;
    let now = ctx.now();
    if !eff.ignored_project_keys.is_empty() && !st.warned.contains_key("bpm-toml-ignored") {
        tracing::warn!(
            "{}: .bpm.toml sets {}, which only the admin config may change; ignored",
            f.name,
            eff.ignored_project_keys.join(", ")
        );
        st.warned.insert("bpm-toml-ignored".into(), now);
    }
    if !eff.managed {
        pt.stage = format!("{} (managed = false)", st.stage);
        return Ok(pt);
    }
    if st.stage == Stage::Orphaned {
        tracing::info!("{}: project directory is back", f.name);
        st.missing_since = None;
        st.set_stage(Stage::Active, now);
    }
    let mut snaps = lu.snapshots()?;
    let owner = (pref.record.owner_uid, pref.record.owner_gid);
    let target = Target::for_project(pref, &eff);
    let before = ctx.btrfs.subvol_info(pref.path())?;
    observe::note_live(&mut st, &before, now);
    observe::update_stage(ctx, &target, &eff, pref.record.adopted, &mut st, now);
    let active = st.stage == Stage::Active;
    pt.banlist = banlist::enforce_cheap(ctx, pref.path(), &eff, &mut st, owner, active)?;
    if pt.banlist.iter().any(BanAction::modified_live) {
        ctx.btrfs.sync(pref.path())?;
        let after = ctx.btrfs.subvol_info(pref.path())?;
        observe::note_tool_change(&mut st, &before, &after, now);
    }
    let live = ctx.btrfs.subvol_info(pref.path())?;
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
            o.stats = observe::want_stats(&eff, &st, &snaps, now);
            o.stats_budget = eff.policy.snapshot.stats_budget;
            let m = snapshot::take(ctx, &target, &o)?;
            observe::note_snapshot(&mut st, &m);
            pt.snapshot = Some(m.id);
            snaps.push(m);
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
    // every snapshot is checked, including hook snapshots and ones taken without stats
    pt.froze = observe::evaluate_guard(ctx, &lu, &target, &eff, &mut st, &mut snaps, now, Walk::Throttled);
    let thin_ok = observe::thin_allowed(&st, &snaps);
    if thin_ok {
        st.warned.remove("thin-pending");
    } else if !st.warned.contains_key("thin-pending") {
        tracing::info!(
            "{}: newest snapshot not yet checked by the shrink guard; retention waits until it is counted",
            f.name
        );
        st.warned.insert("thin-pending".into(), now);
    }
    let windows = thin::Windows::from(&eff.policy.thin);
    let deletions = if thin_ok {
        thin::select_deletions(&snaps, now, &windows, st.frozen.as_ref(), eff.policy.shrink_guard.thin_after_freeze)
    } else {
        vec![]
    };
    for id in deletions {
        let Some(m) = snaps.iter().find(|m| m.id == id).cloned() else {
            continue;
        };
        match snapshot::delete(ctx, &lu, &pref.record, &snaps, &m, DeleteMode::Policy, "retention") {
            Ok(()) => {
                pt.deleted.push(id);
                snaps.retain(|s| s.id != id);
            }
            Err(e) => tracing::warn!("{}: could not delete #{id}: {e:#}", f.name),
        }
    }
    for (rel, since) in st.pending_convert.clone() {
        let key = format!("convert-stuck:{rel}");
        if age(now, since) > Duration::from_secs(86400) && !st.warned.contains_key(&key) {
            tracing::warn!(
                "{}: banned directory {rel} is still a plain directory after a day (always busy?); run `bpm convert {} {rel}`",
                f.name,
                f.name
            );
            st.warned.insert(key, now);
        }
    }
    heavy.extend(heavy_candidates(ctx, pref, &eff, &st, &snaps, now)?);
    st.last_tick = Some(now);
    st.last_error = None;
    pt.stage = st.stage.to_string();
    lu.write_state(&st)?;
    Ok(pt)
}

fn run_heavy(ctx: &Ctx, root: &RootCfg, store: &Store, op: &Heavy) -> Result<String> {
    let with_project = |name: &str| -> Result<(ProjectRef, EffectiveConfig, LockedUnit)> {
        let pref = project::resolve_in(ctx, root, name)?;
        let eff = super::effective(ctx, &pref)?;
        let lu = pref.unit.lock(PROJECT_LOCK)?;
        Ok((pref, eff, lu))
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
            let (pref, eff, lu) = with_project(name)?;
            ctx.btrfs.sync(pref.path())?;
            let before = ctx.btrfs.subvol_info(pref.path())?;
            banlist::convert(ctx, pref.path(), rel, eff.policy.keep_build_on_adopt, false)?;
            let mut st = lu.read_state()?;
            st.pending_convert.remove(rel);
            ctx.btrfs.sync(pref.path())?;
            let after = ctx.btrfs.subvol_info(pref.path())?;
            observe::note_tool_change(&mut st, &before, &after, ctx.now());
            lu.write_state(&st)?;
            Ok("converted".into())
        }
        Heavy::Collapse { name } => {
            let (pref, eff, lu) = with_project(name)?;
            let mut st = lu.read_state()?;
            let mut snaps = lu.snapshots()?;
            let r = recompress::collapse(ctx, &lu, &pref, &eff, &mut st, &mut snaps)?;
            lu.write_state(&st)?;
            Ok(format!("{} deleted, {} kept", r.deleted.len(), snaps.len()))
        }
        Heavy::Recompress { name } => {
            let (pref, eff, lu) = with_project(name)?;
            let mut st = lu.read_state()?;
            let mut snaps = lu.snapshots()?;
            let r =
                recompress::recompress(ctx, &lu, &pref, &eff, &mut st, &mut snaps, eff.policy.recompress.level, false);
            lu.write_state(&st)?;
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
