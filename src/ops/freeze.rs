//! `bpm freeze` / `bpm unfreeze`.

use crate::cli::{FreezeArgs, UnfreezeArgs};
use crate::ctx::Ctx;
use crate::error::refused;
use crate::mechanics::snapshot::{self, SnapOpts};
use crate::mechanics::{Target, observe};
use crate::output::emit;
use crate::policy::change;
use crate::project;
use crate::store::{Frozen, SnapshotKind, newest};
use anyhow::Result;

pub fn freeze(ctx: &Ctx, a: FreezeArgs) -> Result<()> {
    let pref = project::resolve(ctx, &a.project)?;
    let _l = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    let mut st = pref.unit.read_state()?;
    let snaps = pref.unit.snapshots()?;
    if st.frozen.is_some() {
        return Err(refused(format!("{} is already frozen", pref.name())));
    }
    st.frozen = Some(Frozen {
        since: ctx.now(),
        reasons: vec![a.reason.clone()],
        trigger_snap: None,
        ref_snap: newest(&snaps).map(|m| m.id),
    });
    pref.unit.write_state(&st)?;
    emit(ctx, &st.frozen, || {
        format!("{}: frozen; no snapshot older than now will be deleted automatically", pref.name())
    });
    Ok(())
}

pub fn unfreeze(ctx: &Ctx, a: UnfreezeArgs) -> Result<()> {
    let pref = project::resolve(ctx, &a.project)?;
    let _l = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    let mut st = pref.unit.read_state()?;
    let Some(f) = st.frozen.clone() else {
        return Err(refused(format!("{} is not frozen", pref.name())));
    };
    st.frozen = None;
    // "the current state is the new reference": snapshot it if the newest snapshot is behind, and
    // count it if it was taken without stats (a hand-restored tree captured by a hook snapshot)
    let eff = super::effective(ctx, &pref)?;
    let target = Target::for_project(&pref, &eff);
    let mut snaps = pref.unit.snapshots()?;
    let _ = ctx.btrfs.sync(pref.path());
    let live = ctx.btrfs.subvol_info(pref.path()).ok().filter(|i| i.uuid == pref.record.uuid);
    match (live, newest(&snaps).cloned()) {
        // the live project is gone (orphaned): the newest snapshot is all there is
        (None, _) => {}
        (Some(live), Some(n)) if change::identical(&live, &n) => {
            if n.stats.is_none() {
                let mut n = n;
                snapshot::count_stats(ctx, &target, &mut n, eff.policy.snapshot.stats_budget)?;
                if let Some(slot) = snaps.iter_mut().find(|m| m.id == n.id) {
                    *slot = n;
                }
            }
        }
        _ => {
            let mut o = SnapOpts::new(SnapshotKind::Auto, "unfreeze: new shrink-guard reference");
            o.stats_budget = eff.policy.snapshot.stats_budget;
            let m = snapshot::take(ctx, &target, &o)?;
            observe::note_snapshot(&mut st, &m);
            snaps.push(m);
        }
    }
    let current = newest(&snaps).cloned();
    match current.as_ref().filter(|m| m.complete_stats().is_some()) {
        Some(m) => observe::accept_as_reference(&mut st, m),
        None => {
            tracing::warn!(
                "{}: the current state could not be counted within snapshot.stats_budget; the shrink guard keeps its old reference",
                pref.name()
            );
            if let Some(m) = &current {
                observe::mark_evaluated(&mut st, m);
            }
        }
    }
    let mut released = Vec::new();
    if a.release_holds {
        for m in &snaps {
            if m.hold && m.hold_note.starts_with("auto-held") {
                let mut m = m.clone();
                m.hold = false;
                m.hold_note.clear();
                pref.unit.update_meta(&m)?;
                released.push(m.id);
            }
        }
    }
    pref.unit.write_state(&st)?;
    emit(ctx, &serde_json::json!({"project": pref.name(), "was": f, "released_holds": released}), || {
        let mut s = format!(
            "{}: unfrozen (was: {}). The current state is the new shrink-guard reference.",
            pref.name(),
            f.reasons.join("; ")
        );
        if released.is_empty() {
            let held: Vec<String> = snaps.iter().filter(|m| m.hold).map(|m| format!("#{}", m.id)).collect();
            if !held.is_empty() {
                s += &format!(
                    "\n  still held: {} (release with `bpm unhold {} <id>` or --release-holds)",
                    held.join(" "),
                    pref.name()
                );
            }
        } else {
            s += &format!("\n  released: {}", released.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(" "));
        }
        s
    });
    Ok(())
}
