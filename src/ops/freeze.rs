//! `bpm freeze` / `bpm unfreeze`.

use crate::cli::{FreezeArgs, UnfreezeArgs};
use crate::ctx::Ctx;
use crate::error::refused;
use crate::output::emit;
use crate::policy::shrink;
use crate::project;
use crate::store::{Frozen, newest};
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
    let Some(f) = st.frozen.take() else {
        return Err(refused(format!("{} is not frozen", pref.name())));
    };
    let snaps = pref.unit.snapshots()?;
    let base = snaps.iter().rev().find(|m| m.complete_stats().is_some());
    st.ref_stats = base.map(|m| shrink::reset(m.complete_stats().unwrap(), m.id));
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
