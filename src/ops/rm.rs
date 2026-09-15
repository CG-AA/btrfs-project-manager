//! `bpm rm` and `bpm forget`.

use crate::cli::{ForgetArgs, RmArgs};
use crate::ctx::Ctx;
use crate::error::{BpmError, refused, usage};
use crate::mechanics::snapshot::{self, DeleteMode};
use crate::output::emit;
use crate::project;
use crate::store::Stage;
use anyhow::Result;

pub fn run(ctx: &Ctx, a: RmArgs) -> Result<()> {
    let (unit, record) = if a.container {
        let root = ctx.roots().into_iter().next().ok_or_else(|| usage("no root configured"))?;
        let store = ctx.store(&root);
        let unit = store.container();
        let rec = unit.read_record()?.ok_or_else(|| usage("no container store"))?;
        (unit, rec)
    } else {
        let pref = project::resolve(ctx, &a.project)?;
        (pref.unit, pref.record)
    };
    let _l = unit.lock(ctx.cfg.global.lock_timeout)?;
    let mut snaps = unit.snapshots()?;
    let mut targets = Vec::new();
    for sel in &a.snapshots {
        targets.push(super::select(ctx, &snaps, sel)?.clone());
    }
    let st = unit.read_state()?;
    let mut deleted = Vec::new();
    let mut failed = 0;
    for m in targets {
        if let Some(f) = &st.frozen {
            if m.created <= f.since && !a.force_held {
                tracing::error!(
                    "#{}: project is frozen and this snapshot predates the freeze; pass --force-held to delete it anyway",
                    m.id
                );
                failed += 1;
                continue;
            }
        }
        match snapshot::delete(ctx, &unit, &record, &snaps, &m, DeleteMode::User { force_held: a.force_held }, "bpm rm")
        {
            Ok(()) => {
                deleted.push(m.id);
                snaps.retain(|s| s.id != m.id);
            }
            Err(e) => {
                failed += 1;
                tracing::error!("#{}: {e:#}", m.id);
            }
        }
    }
    emit(ctx, &deleted, || deleted.iter().map(|id| format!("deleted #{id}")).collect::<Vec<_>>().join("\n"));
    if failed > 0 {
        return Err(BpmError::Partial { failed }.into());
    }
    Ok(())
}

pub fn forget(ctx: &Ctx, a: ForgetArgs) -> Result<()> {
    let pref = project::resolve(ctx, &a.project)?;
    let live_ok = ctx.btrfs.subvol_info(pref.path()).map(|i| i.uuid == pref.record.uuid).unwrap_or(false);
    let st = pref.unit.read_state()?;
    if live_ok && st.stage != Stage::Archived {
        tracing::warn!(
            "{} still exists; after `forget` the next tick will adopt it again unless it is ignored or has managed = false",
            pref.path().display()
        );
    }
    let snaps = pref.unit.snapshots()?;
    if !snaps.is_empty() && !a.delete_snapshots {
        return Err(refused(format!(
            "{} has {} snapshots; pass --delete-snapshots --yes to delete them",
            pref.name(),
            snaps.len()
        )));
    }
    if a.delete_snapshots && !a.yes {
        return Err(refused("deleting all snapshots needs --yes"));
    }
    let _l = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    let mut remaining = snaps.clone();
    for m in &snaps {
        snapshot::delete(
            ctx,
            &pref.unit,
            &pref.record,
            &remaining,
            m,
            DeleteMode::User { force_held: true },
            "forget",
        )?;
        remaining.retain(|s| s.id != m.id);
    }
    let scan = pref.unit.scan()?;
    if !scan.without_meta.is_empty() {
        return Err(refused(format!(
            "{} contains snapshots without metadata; inspect with `bpm doctor`",
            pref.unit.dir.display()
        )));
    }
    if !ctx.opts.dry_run {
        std::fs::remove_dir_all(&pref.unit.dir)?;
    }
    emit(ctx, &serde_json::json!({"forgot": pref.name(), "deleted_snapshots": snaps.len()}), || {
        format!("{}: removed from the store ({} snapshots deleted)", pref.name(), snaps.len())
    });
    Ok(())
}
