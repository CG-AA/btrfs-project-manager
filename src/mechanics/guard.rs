//! Apply the shrink guard to a snapshot: freeze cleanup and pin the last good state.

use super::Target;
use super::snapshot::count_stats;
use crate::config::EffectiveConfig;
use crate::ctx::Ctx;
use crate::hooks::{self, HookCtx, HookEvent};
use crate::policy::shrink;
use crate::store::{Frozen, ProjectState, RefStats, SnapshotMeta};

/// How many snapshots without stats, between the last counted one and the trigger, are walked
/// when looking for the last good state.
const MAX_UNCOUNTED_CANDIDATES: usize = 5;

/// Evaluate `new` (normally the newest snapshot). Returns true when it tripped the guard.
pub fn apply(
    ctx: &Ctx,
    t: &Target,
    eff: &EffectiveConfig,
    st: &mut ProjectState,
    snaps: &mut [SnapshotMeta],
    new: &SnapshotMeta,
) -> bool {
    st.guard_seen = st.guard_seen.max(new.id);
    let Some(new_stats) = new.complete_stats() else {
        if new.stats.is_some() && !st.warned.contains_key("guard-incomplete") {
            tracing::warn!(
                "{}: counting snapshot #{} exceeded snapshot.stats_budget; the shrink guard cannot check it",
                t.name,
                new.id
            );
            st.warned.insert("guard-incomplete".into(), ctx.now());
        }
        return false;
    };
    st.warned.remove("guard-incomplete");
    let prev = snaps.iter().filter(|m| m.id < new.id && m.complete_stats().is_some()).max_by_key(|m| m.id).cloned();
    let reference = match (&st.frozen, &st.ref_stats, &prev) {
        // while frozen, only a further drop relative to the previous snapshot matters
        (Some(_), _, Some(p)) => shrink::reset(p.complete_stats().unwrap(), p.id),
        (Some(_), _, None) => return false,
        (None, Some(r), _) => r.clone(),
        (None, None, _) => {
            st.ref_stats = Some(shrink::reset(new_stats, new.id));
            return false;
        }
    };
    let reasons = shrink::evaluate(
        &reference,
        prev.as_ref().and_then(|p| p.complete_stats()),
        new_stats,
        &eff.policy.shrink_guard,
        &eff.sentinels,
    );
    if reasons.is_empty() {
        if st.frozen.is_none() {
            if let Some(r) = st.ref_stats.as_mut() {
                shrink::raise(r, new_stats, new.id);
            }
        }
        return false;
    }
    let last_good_id = last_good(ctx, t, eff, snaps, &reference, prev.as_ref(), new.id);
    if let Some(lg) = last_good_id.and_then(|id| snaps.iter_mut().find(|m| m.id == id)) {
        if !lg.hold {
            lg.hold = true;
            lg.hold_note = format!("auto-held: last state before shrink detected in #{}", new.id);
            if let Err(e) = t.unit.update_meta(lg) {
                tracing::error!("{}: could not hold snapshot #{}: {e:#}", t.name, lg.id);
            }
        }
    }
    let first = st.frozen.is_none();
    match st.frozen.as_mut() {
        None => {
            st.frozen = Some(Frozen {
                since: ctx.now(),
                reasons: reasons.clone(),
                trigger_snap: Some(new.id),
                ref_snap: last_good_id,
            });
        }
        Some(f) => {
            for r in &reasons {
                let tagged = format!("#{}: {r}", new.id);
                if !f.reasons.contains(&tagged) {
                    f.reasons.push(tagged);
                }
            }
        }
    }
    let ref_snap = st.frozen.as_ref().and_then(|f| f.ref_snap);
    tracing::error!(
        "{}: shrink detected in snapshot #{}: {}. Cleanup is frozen; last good snapshot #{} is held. Resolve with `bpm rollback {} {}` or `bpm unfreeze {}`.",
        t.name,
        new.id,
        reasons.join("; "),
        ref_snap.map(|i| i.to_string()).unwrap_or_else(|| "?".into()),
        t.name,
        ref_snap.map(|i| i.to_string()).unwrap_or_else(|| "<snapshot>".into()),
        t.name
    );
    let before = reference.files.to_string();
    let hctx = HookCtx {
        project: Some(t.name),
        project_path: Some(t.live),
        owner: t.owner,
        root: Some(t.root),
        snapshot: Some((new.id, t.unit.snapshot_path(new.id), new.kind)),
        reason: reasons.join("; "),
        extra: vec![
            ("BPM_REF_SNAPSHOT", ref_snap.map(|i| i.to_string()).unwrap_or_default()),
            ("BPM_TRIGGER_SNAPSHOT", new.id.to_string()),
            ("BPM_FILES_BEFORE", before),
            ("BPM_FILES_AFTER", new_stats.files.to_string()),
            ("BPM_BYTES_BEFORE", reference.bytes.to_string()),
            ("BPM_BYTES_AFTER", new_stats.bytes.to_string()),
        ],
        ..Default::default()
    };
    let _ = hooks::run(ctx, HookEvent::OnShrinkDetected, &hctx);
    if first {
        let _ = hooks::run(ctx, HookEvent::OnFreeze, &hctx);
    }
    true
}

/// The newest snapshot before `trigger` that passes the guard. Snapshots taken without stats
/// (hook snapshots, throttled counts) often already contain the loss, so they are counted and
/// checked instead of being trusted for being "the previous one".
fn last_good(
    ctx: &Ctx,
    t: &Target,
    eff: &EffectiveConfig,
    snaps: &mut [SnapshotMeta],
    reference: &RefStats,
    prev: Option<&SnapshotMeta>,
    trigger: u64,
) -> Option<u64> {
    let lower = prev.map(|p| p.id).unwrap_or(0);
    let mut between: Vec<usize> = (0..snaps.len()).filter(|&i| snaps[i].id > lower && snaps[i].id < trigger).collect();
    between.sort_by_key(|&i| std::cmp::Reverse(snaps[i].id));
    for i in between.into_iter().take(MAX_UNCOUNTED_CANDIDATES) {
        if snaps[i].stats.is_none() {
            if let Err(e) = count_stats(ctx, t, &mut snaps[i], eff.policy.snapshot.stats_budget) {
                tracing::warn!("{}: {e:#}", t.name);
                continue;
            }
        }
        let Some(s) = snaps[i].complete_stats() else { continue };
        let prev_stats = prev.and_then(|p| p.complete_stats());
        if shrink::evaluate(reference, prev_stats, s, &eff.policy.shrink_guard, &eff.sentinels).is_empty() {
            return Some(snaps[i].id);
        }
    }
    prev.map(|p| p.id)
}
