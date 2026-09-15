//! `bpm restore` (paths or whole project) and `bpm rollback`.

use crate::cli::{RestoreArgs, RollbackArgs};
use crate::ctx::Ctx;
use crate::error::{refused, usage};
use crate::mechanics::snapshot::{self, SnapOpts};
use crate::mechanics::{Target, adopt, banlist, rollback};
use crate::output::emit;
use crate::policy::shrink;
use crate::project;
use crate::store::{SnapshotKind, Stage, newest};
use anyhow::Result;

pub fn restore(ctx: &Ctx, a: RestoreArgs) -> Result<()> {
    let mut pref = project::resolve(ctx, &a.project)?;
    let _l = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    let snaps = pref.unit.snapshots()?;
    let sel = a.snapshot.clone().unwrap_or_else(|| "latest".into());
    let meta = super::select(ctx, &snaps, &sel)?.clone();
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
        st.frozen = None;
        st.missing_since = None;
        st.set_stage(Stage::Active, ctx.now());
        adopt::init_state(ctx, &mut st, &eff, &pref.record, &post, pref.path())?;
        st.last_change_at = Some(ctx.now());
        st.stage = Stage::Active;
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
        st.snap_ctransid = post.source_ctransid;
        st.last_snapshot_at = Some(post.created);
        st.last_change_at = Some(ctx.now());
        st.set_stage(Stage::Active, ctx.now());
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
    let meta = super::select(ctx, &snaps, &a.snapshot)?.clone();
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
    st.snap_ctransid = post.source_ctransid;
    st.tool_ctransid = 0;
    st.last_snapshot_at = Some(post.created);
    st.last_change_at = Some(ctx.now());
    st.set_stage(Stage::Active, ctx.now());
    let mut unfroze = false;
    if let (Some(f), Some(stats)) = (&st.frozen, post.complete_stats()) {
        let reference_files = f
            .ref_snap
            .and_then(|id| snaps.iter().find(|m| m.id == id))
            .and_then(|m| m.complete_stats())
            .map(|s| s.files)
            .or(st.ref_stats.as_ref().map(|r| r.files))
            .unwrap_or(0);
        if stats.files as f64 >= 0.7 * reference_files as f64 {
            st.frozen = None;
            st.ref_stats = Some(shrink::reset(stats, post.id));
            unfroze = true;
        }
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
