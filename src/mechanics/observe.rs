//! The one place that updates a project's derived state: user activity and idle stage,
//! snapshot bookkeeping, and shrink-guard evaluation of every snapshot, including snapshots
//! taken without stats. Operations take and delete snapshots, then report here.

use super::Target;
use super::guard;
use super::snapshot::count_stats;
use crate::btrfs::SubvolInfo;
use crate::config::EffectiveConfig;
use crate::ctx::Ctx;
use crate::hooks::{self, HookCtx, HookEvent};
use crate::policy::{change, lifecycle, shrink};
use crate::store::{ProjectState, SnapshotMeta, Stage, newest};
use crate::util::time::age;
use anyhow::Result;
use jiff::Timestamp;
use std::path::Path;

/// Whether `evaluate_guard` may walk a snapshot that was taken without stats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Walk {
    /// Only evaluate snapshots that already have stats (commands that must stay fast).
    Never,
    /// Walk when `snapshot.stats_min_interval` allows it (the tick).
    Throttled,
    /// Always walk (explicit commands such as `unfreeze`).
    Always,
}

/// Count live changes since the previous observation as user activity. Returns true when the
/// project changed.
pub fn note_live(st: &mut ProjectState, live: &SubvolInfo, now: Timestamp) -> bool {
    let changed = change::user_changed(live, st);
    if changed {
        st.last_change_at = Some(live.ctime.unwrap_or(now).min(now));
    }
    st.seen_ctransid = st.seen_ctransid.max(live.ctransid);
    changed
}

/// bpm itself changed the live tree (banlist subvolumes, conversion, recompression). `before`
/// is read just before the change, so user changes made earlier still count.
pub fn note_tool_change(st: &mut ProjectState, before: &SubvolInfo, after: &SubvolInfo, now: Timestamp) {
    note_live(st, before, now);
    st.tool_ctransid = st.tool_ctransid.max(after.ctransid);
    st.seen_ctransid = st.seen_ctransid.max(after.ctransid);
}

pub fn note_snapshot(st: &mut ProjectState, m: &SnapshotMeta) {
    st.last_snapshot_at = Some(m.created);
    if m.stats.is_some() {
        st.last_stats_at = Some(m.created);
    }
}

/// The user chose this snapshot's state (rollback, recreate): it is evaluated and becomes the
/// shrink-guard reference.
pub fn accept_as_reference(st: &mut ProjectState, m: &SnapshotMeta) {
    mark_evaluated(st, m);
    if let Some(s) = m.complete_stats() {
        st.ref_stats = Some(shrink::reset(s, m.id));
    }
}

/// Treat `m` as checked without comparing it (a state the user explicitly asked for).
pub fn mark_evaluated(st: &mut ProjectState, m: &SnapshotMeta) {
    st.guard_seen = st.guard_seen.max(m.id);
}

/// A rollback replaced the live subvolume: counters start over and the rollback is activity.
pub fn note_replaced_live(st: &mut ProjectState, live: &SubvolInfo, now: Timestamp) {
    st.seen_ctransid = live.ctransid;
    st.tool_ctransid = live.ctransid;
    st.last_change_at = Some(now);
    st.collapse_incomplete_ctransid = None;
}

/// Fresh derived state for a new live subvolume (adopt, recreate, unarchive) whose first
/// snapshot is `meta`.
pub fn init_new_live(
    ctx: &Ctx,
    eff: &EffectiveConfig,
    st: &mut ProjectState,
    meta: &SnapshotMeta,
    live: &Path,
    last_change: Timestamp,
) -> Result<()> {
    let now = ctx.now();
    ctx.btrfs.sync(live)?;
    let info = ctx.btrfs.subvol_info(live)?;
    st.seen_ctransid = info.ctransid;
    st.tool_ctransid = info.ctransid;
    st.collapse_incomplete_ctransid = None;
    note_snapshot(st, meta);
    accept_as_reference(st, meta);
    st.last_change_at = Some(last_change.min(now));
    st.stage = lifecycle::stage_for_idle(age(now, last_change), &eff.policy.lifecycle);
    st.stage_since = Some(now);
    st.frozen = None;
    st.missing_since = None;
    Ok(())
}

/// Move the idle stage to where the time since the last change puts it.
pub fn update_stage(
    ctx: &Ctx,
    t: &Target,
    eff: &EffectiveConfig,
    adopted: Timestamp,
    st: &mut ProjectState,
    now: Timestamp,
) -> Option<(Stage, Stage)> {
    let idle = age(now, st.last_change_at.unwrap_or(adopted));
    let (from, to) = st.set_stage(lifecycle::stage_for_idle(idle, &eff.policy.lifecycle), now)?;
    tracing::info!("{}: {from} -> {to}", t.name);
    let h = HookCtx {
        project: Some(t.name),
        project_path: Some(t.live),
        owner: t.owner,
        root: Some(t.root),
        stage: Some((from, to)),
        reason: "idle".into(),
        ..Default::default()
    };
    let _ = hooks::run(ctx, HookEvent::StageChange, &h);
    Some((from, to))
}

/// Count stats for trees that are cheap to walk every time, others at most every
/// `snapshot.stats_min_interval`.
pub fn want_stats(eff: &EffectiveConfig, st: &ProjectState, snaps: &[SnapshotMeta], now: Timestamp) -> bool {
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

/// Evaluate the shrink guard on the newest snapshot unless that already happened, counting it
/// first when `walk` allows. Returns true when the guard froze the project.
pub fn evaluate_guard(
    ctx: &Ctx,
    t: &Target,
    eff: &EffectiveConfig,
    st: &mut ProjectState,
    snaps: &mut [SnapshotMeta],
    now: Timestamp,
    walk: Walk,
) -> bool {
    let Some(mut n) = newest(snaps).cloned() else {
        return false;
    };
    if n.id <= st.guard_seen {
        return false;
    }
    if n.stats.is_none() {
        let go = match walk {
            Walk::Never => false,
            Walk::Throttled => want_stats(eff, st, snaps, now),
            Walk::Always => true,
        };
        if !go {
            return false;
        }
        if let Err(e) = count_stats(ctx, t, &mut n, eff.policy.snapshot.stats_budget) {
            tracing::warn!("{}: {e:#}", t.name);
            return false;
        }
        st.last_stats_at = Some(now);
        if let Some(slot) = snaps.iter_mut().find(|m| m.id == n.id) {
            *slot = n.clone();
        }
    }
    guard::apply(ctx, t, eff, st, snaps, &n)
}

/// Retention and collapse may delete snapshots only once the guard has seen the newest one:
/// otherwise a loss captured by a snapshot without stats would thin away the good state.
pub fn thin_allowed(st: &ProjectState, snaps: &[SnapshotMeta]) -> bool {
    newest(snaps).is_none_or(|n| n.id <= st.guard_seen)
}

/// After a command outside the tick took snapshots or changed the live tree: record activity,
/// update the stage and evaluate the guard on snapshots that already have stats.
pub fn after_command(
    ctx: &Ctx,
    t: &Target,
    eff: &EffectiveConfig,
    adopted: Timestamp,
    st: &mut ProjectState,
    snaps: &mut [SnapshotMeta],
) -> Result<bool> {
    let now = ctx.now();
    ctx.btrfs.sync(t.live)?;
    let live = ctx.btrfs.subvol_info(t.live)?;
    note_live(st, &live, now);
    update_stage(ctx, t, eff, adopted, st, now);
    Ok(evaluate_guard(ctx, t, eff, st, snaps, now, Walk::Never))
}
