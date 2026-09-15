//! `bpm list`: snapshots of one project (or the root container).

use crate::cli::ListArgs;
use crate::ctx::Ctx;
use crate::output::{Table, emit};
use crate::util::bytes::fmt_bytes;
use crate::util::time::fmt_local;
use anyhow::Result;

pub fn run(ctx: &Ctx, a: ListArgs) -> Result<()> {
    let (unit, name) = if a.container {
        let root = ctx.roots().into_iter().next().ok_or_else(|| crate::error::usage("no root configured"))?;
        let store = ctx.store(&root);
        (store.container(), crate::store::CONTAINER.to_string())
    } else {
        let pref = super::resolve_or_cwd(ctx, a.project.as_deref())?;
        (pref.unit.clone(), pref.name().to_string())
    };
    let mut snaps = unit.snapshots()?;
    if let Some(k) = &a.kind {
        snaps.retain(|m| m.kind.as_str() == k);
    }
    if a.held {
        snaps.retain(|m| m.hold);
    }
    if let Some(n) = a.limit {
        let skip = snaps.len().saturating_sub(n);
        snaps.drain(..skip);
    }
    emit(ctx, &snaps, || {
        let mut t = Table::new(&["ID", "CREATED", "KIND", "HOLD", "FILES", "SIZE", "REASON"]);
        for m in &snaps {
            t.row(vec![
                m.id.to_string(),
                fmt_local(m.created, &ctx.tz),
                m.kind.to_string(),
                if m.hold { "held".into() } else { String::new() },
                m.stats
                    .as_ref()
                    .map(|s| format!("{}{}", s.files, if s.complete { "" } else { "+" }))
                    .unwrap_or_else(|| "-".into()),
                m.stats.as_ref().map(|s| fmt_bytes(s.bytes)).unwrap_or_else(|| "-".into()),
                if m.hold && !m.hold_note.is_empty() {
                    format!("{} [{}]", m.reason, m.hold_note)
                } else {
                    m.reason.clone()
                },
            ]);
        }
        if t.is_empty() {
            format!("{name}: no snapshots")
        } else {
            format!("{name}  {}\n{}", unit.dir.display(), t.render())
        }
    });
    Ok(())
}
