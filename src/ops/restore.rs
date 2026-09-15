//! `bpm restore` (paths or whole project) and `bpm rollback`.

use crate::cli::{RestoreArgs, RollbackArgs};
use crate::ctx::Ctx;
use crate::error::{refused, usage};
use crate::mechanics::snapshot::{self, SnapOpts};
use crate::mechanics::{Target, banlist, observe, rollback};
use crate::output::emit;
use crate::project;
use crate::store::{SnapshotKind, newest};
use anyhow::Result;

pub fn restore(ctx: &Ctx, a: RestoreArgs) -> Result<()> {
    let mut pref = project::resolve(ctx, &a.project)?;
    let _l = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    let snaps = pref.unit.snapshots()?;
    let sel = a.snapshot.clone().unwrap_or_else(|| "latest".into());
    let meta = super::select(ctx, &pref.unit, &snaps, &sel)?.clone();
    if a.recreate {
        if !a.paths.is_empty() {
            return Err(usage("--recreate restores the whole project; do not list paths"));
        }
        let new_uuid = rollback::recreate(ctx, &mut pref, &meta)?;
        if ctx.opts.dry_run {
            return Ok(());
        }
        let eff = super::effective(ctx, &pref)?;
        let mut st = pref.unit.read_state()?;
        let owner = (pref.record.owner_uid, pref.record.owner_gid);
        banlist::enforce_cheap(ctx, pref.path(), &eff, &mut st, owner, true)?;
        let target = Target::for_project(&pref, &eff);
        let post =
            snapshot::take(ctx, &target, &SnapOpts::new(SnapshotKind::Post, format!("recreated from #{}", meta.id)))?;
        observe::init_new_live(ctx, &eff, &mut st, &post, pref.path(), ctx.now())?;
        pref.unit.write_state(&st)?;
        emit(
            ctx,
            &serde_json::json!({"project": pref.name(), "recreated_from": meta.id, "uuid": new_uuid, "snapshot": post.id}),
            || {
                format!(
                    "{}: recreated {} from snapshot #{} (new snapshot #{}); build directories start empty",
                    pref.name(),
                    pref.path().display(),
                    meta.id,
                    post.id
                )
            },
        );
        return Ok(());
    }
    if a.paths.is_empty() {
        return Err(usage("name the paths to restore, or pass --recreate for the whole project"));
    }
    let eff = super::effective(ctx, &pref)?;
    let report = rollback::restore_paths(ctx, &pref, &eff, &meta, &a.paths, a.to.as_deref(), a.overwrite)?;
    for w in &report.warnings {
        tracing::warn!("{w}");
    }
    if a.to.is_none() && !report.restored.is_empty() && !ctx.opts.dry_run {
        let mut st = pref.unit.read_state()?;
        let target = Target::for_project(&pref, &eff);
        let post = snapshot::take(
            ctx,
            &target,
            &SnapOpts::new(SnapshotKind::Post, format!("after restoring from #{}", meta.id)),
        )?;
        observe::note_snapshot(&mut st, &post);
        let mut snaps = pref.unit.snapshots()?;
        observe::after_command(ctx, &target, &eff, pref.record.adopted, &mut st, &mut snaps)?;
        pref.unit.write_state(&st)?;
    }
    emit(ctx, &report, || {
        let mut s: Vec<String> = report.restored.iter().map(|p| format!("restored {}", p.display())).collect();
        if let Some(id) = report.pre_snapshot {
            s.push(format!("(previous state saved as snapshot #{id})"));
        }
        s.join("\n")
    });
    Ok(())
}

pub fn rollback(ctx: &Ctx, a: RollbackArgs) -> Result<()> {
    let mut pref = project::resolve(ctx, &a.project)?;
    let eff = super::effective(ctx, &pref)?;
    let _l = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    let snaps = pref.unit.snapshots()?;
    let meta = super::select(ctx, &pref.unit, &snaps, &a.snapshot)?.clone();
    if newest(&snaps).is_some_and(|n| n.id == meta.id) {
        let live = ctx.btrfs.subvol_info(pref.path())?;
        if crate::policy::change::identical(&live, &meta) {
            return Err(refused(format!("{} is already identical to snapshot #{}", pref.name(), meta.id)));
        }
    }
    let report = rollback::rollback(ctx, &mut pref, &eff, &meta, a.drop_build_dirs, a.force)?;
    if ctx.opts.dry_run {
        return Ok(());
    }
    let mut st = pref.unit.read_state()?;
    let owner = (pref.record.owner_uid, pref.record.owner_gid);
    banlist::enforce_cheap(ctx, pref.path(), &eff, &mut st, owner, true)?;
    let target = Target::for_project(&pref, &eff);
    let mut o = SnapOpts::new(SnapshotKind::Post, format!("rolled back to #{}", meta.id));
    o.pair = Some(report.safety_snapshot);
    let post = snapshot::take(ctx, &target, &o)?;
    let now = ctx.now();
    // a new live subvolume: its counters start over, and the rollback itself is activity
    ctx.btrfs.sync(pref.path())?;
    let live = ctx.btrfs.subvol_info(pref.path())?;
    observe::note_replaced_live(&mut st, &live, now);
    observe::note_snapshot(&mut st, &post);
    observe::update_stage(ctx, &target, &eff, pref.record.adopted, &mut st, now);
    let mut unfroze = false;
    match (&st.frozen, post.complete_stats()) {
        (Some(f), Some(stats)) => {
            let snaps = pref.unit.snapshots()?;
            let reference_files = f
                .ref_snap
                .and_then(|id| snaps.iter().find(|m| m.id == id))
                .and_then(|m| m.complete_stats())
                .map(|s| s.files)
                .or(st.ref_stats.as_ref().map(|r| r.files))
                .unwrap_or(0);
            if stats.files as f64 >= 0.7 * reference_files as f64 {
                st.frozen = None;
                unfroze = true;
                observe::accept_as_reference(&mut st, &post);
            } else {
                observe::mark_evaluated(&mut st, &post);
            }
        }
        // the user chose this state: it is the new reference
        (None, _) => observe::accept_as_reference(&mut st, &post),
        (Some(_), None) => observe::mark_evaluated(&mut st, &post),
    }
    pref.unit.write_state(&st)?;
    emit(
        ctx,
        &serde_json::json!({"project": pref.name(), "rolled_back_to": meta.id, "safety_snapshot": report.safety_snapshot, "post_snapshot": post.id, "report": report, "unfroze": unfroze}),
        || {
            let mut s = format!(
                "{}: rolled back to #{} (previous state kept as snapshot #{}; new snapshot #{})",
                pref.name(),
                meta.id,
                report.safety_snapshot,
                post.id
            );
            if !report.moved_nested.is_empty() {
                s += &format!(
                    "\n  kept build directories: {}",
                    report.moved_nested.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                );
            }
            if unfroze {
                s += "\n  project unfrozen";
            }
            if let Some(l) = &report.leftover {
                s += &format!("\n  leftover {} (run `bpm doctor --fix`)", l.display());
            }
            s
        },
    );
    Ok(())
}
