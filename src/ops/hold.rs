//! `bpm hold` / `bpm unhold`.

use crate::cli::HoldArgs;
use crate::ctx::Ctx;
use crate::output::emit;
use crate::project;
use anyhow::Result;

pub fn run(ctx: &Ctx, a: HoldArgs, hold: bool) -> Result<()> {
    let pref = project::resolve(ctx, &a.project)?;
    let _l = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    let snaps = pref.unit.snapshots()?;
    let mut changed = Vec::new();
    for sel in &a.snapshots {
        let mut m = super::select(ctx, &pref.unit, &snaps, sel)?.clone();
        if m.hold != hold {
            m.hold = hold;
            m.hold_note = if hold {
                if a.note.is_empty() { format!("held by {}", ctx.invoker.user) } else { a.note.clone() }
            } else {
                String::new()
            };
            pref.unit.update_meta(&m)?;
            changed.push(m.id);
        }
    }
    let verb = if hold { "held" } else { "released" };
    emit(ctx, &changed, || {
        changed.iter().map(|id| format!("{}: #{id} {verb}", pref.name())).collect::<Vec<_>>().join("\n")
    });
    Ok(())
}
