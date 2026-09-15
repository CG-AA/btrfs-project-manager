//! Restore paths from a snapshot, roll a whole project back, or recreate a deleted project.

use super::Target;
use super::snapshot::{self, SnapOpts};
use crate::config::EffectiveConfig;
use crate::ctx::Ctx;
use crate::error::refused;
use crate::hooks::{self, HookCtx, HookEvent};
use crate::project::ProjectRef;
use crate::store::journal::Journal;
use crate::store::{SnapshotKind, SnapshotMeta};
use crate::util::fs::{lchown, rename_strict, safe_relative, sibling};
use crate::util::proc;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize)]
pub struct RestoreReport {
    pub restored: Vec<PathBuf>,
    pub pre_snapshot: Option<u64>,
    pub warnings: Vec<String>,
}

fn normalize_rel(live: &Path, p: &str) -> Result<PathBuf> {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        let abs = std::path::absolute(&pb)?;
        return abs
            .strip_prefix(live)
            .map(Path::to_path_buf)
            .map_err(|_| refused(format!("{p} is not inside {}", live.display())));
    }
    safe_relative(p).ok_or_else(|| refused(format!("invalid relative path {p:?}")))
}

/// Copy paths out of a snapshot into the live project (or `to`).
pub fn restore_paths(
    ctx: &Ctx,
    pref: &ProjectRef,
    eff: &EffectiveConfig,
    meta: &SnapshotMeta,
    paths: &[String],
    to: Option<&Path>,
    overwrite: bool,
) -> Result<RestoreReport> {
    let snap = pref.unit.snapshot_path(meta.id);
    let live = pref.path().to_path_buf();
    let mut report = RestoreReport { restored: vec![], pre_snapshot: None, warnings: vec![] };
    let mut plan = Vec::new();
    for p in paths {
        let rel = normalize_rel(&live, p)?;
        let src = snap.join(&rel);
        let smd = std::fs::symlink_metadata(&src)
            .map_err(|_| crate::error::not_found(format!("{} is not in snapshot #{}", rel.display(), meta.id)))?;
        if eff.banlist.iter().any(|b| rel.starts_with(b)) {
            report.warnings.push(format!(
                "{} is a banned (unsnapshotted) directory; the snapshot only has an empty placeholder",
                rel.display()
            ));
            if smd.is_dir() {
                continue;
            }
        }
        let dst = match to {
            Some(d) => d.join(rel.file_name().unwrap_or(rel.as_os_str())),
            None => live.join(&rel),
        };
        if dst.symlink_metadata().is_ok() && !overwrite {
            return Err(refused(format!(
                "{} exists; pass --overwrite (a pre-restore snapshot is taken first)",
                dst.display()
            )));
        }
        plan.push((rel, src, dst));
    }
    if plan.iter().any(|(_, _, d)| d.symlink_metadata().is_ok() && d.starts_with(&live)) {
        let target = Target::for_project(pref, eff);
        let m = snapshot::take(
            ctx,
            &target,
            &SnapOpts::new(SnapshotKind::PreRestore, format!("before restoring from #{}", meta.id)),
        )?;
        report.pre_snapshot = Some(m.id);
    }
    for (rel, src, dst) in plan {
        if ctx.opts.dry_run {
            tracing::info!("[dry-run] restore {} -> {}", src.display(), dst.display());
            continue;
        }
        if let Some(parent) = dst.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
                if dst.starts_with(&live) {
                    let mut p = parent.to_path_buf();
                    while p.starts_with(&live) && p != live {
                        let _ = lchown(&p, pref.record.owner_uid, pref.record.owner_gid);
                        p.pop();
                    }
                }
            }
        }
        if let Ok(dmd) = std::fs::symlink_metadata(&dst) {
            if dmd.is_dir() && !dmd.file_type().is_symlink() {
                if ctx.btrfs.is_subvolume(&dst)? {
                    bail!("{} is a subvolume; refusing to replace it", dst.display());
                }
                std::fs::remove_dir_all(&dst)?;
            } else {
                std::fs::remove_file(&dst)?;
            }
        }
        crate::util::fs::cp_a(&src, &dst, ctx.reflink)?;
        if to.is_some() && !crate::privilege::is_root() {
            // copying out as a normal user already has the right owner
        } else if to.is_some() {
            let _ = std::process::Command::new("chown")
                .arg("-R")
                .arg(format!("{}:{}", ctx.invoker.uid, ctx.invoker.gid))
                .arg(&dst)
                .status();
        }
        report.restored.push(rel);
    }
    Ok(report)
}

#[derive(Clone, Debug, Serialize)]
pub struct RollbackReport {
    pub safety_snapshot: u64,
    pub moved_nested: Vec<PathBuf>,
    pub dropped_nested: Vec<PathBuf>,
    pub old_uuid: String,
    pub new_uuid: String,
    pub leftover: Option<PathBuf>,
}

fn nested_under(ctx: &Ctx, dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut it = walkdir::WalkDir::new(dir).follow_links(false).min_depth(1).into_iter();
    while let Some(entry) = it.next() {
        let Ok(e) = entry else { continue };
        if crate::util::walk::entry_is_subvol(&e, &|p, i| ctx.is_subvol(p, i)) {
            out.push(e.path().strip_prefix(dir).unwrap().to_path_buf());
            it.skip_current_dir();
        }
    }
    out
}

/// Place a writable copy of `meta` at `live` and fix up nested subvolume placeholders.
fn materialize(ctx: &Ctx, pref: &ProjectRef, meta: &SnapshotMeta, live: &Path) -> Result<()> {
    ctx.btrfs.snapshot(&pref.unit.snapshot_path(meta.id), live, false)?;
    let md = std::fs::symlink_metadata(live)?;
    use std::os::unix::fs::MetadataExt;
    if (md.uid(), md.gid()) != (pref.record.owner_uid, pref.record.owner_gid) {
        lchown(live, pref.record.owner_uid, pref.record.owner_gid)?;
    }
    Ok(())
}

pub fn rollback(
    ctx: &Ctx,
    pref: &mut ProjectRef,
    eff: &EffectiveConfig,
    meta: &SnapshotMeta,
    drop_build: bool,
    force: bool,
) -> Result<RollbackReport> {
    let live = pref.path().to_path_buf();
    if !ctx.btrfs.is_subvolume(&live).unwrap_or(false) {
        return Err(refused(format!("{} does not exist; use `bpm restore --project {}`", live.display(), pref.name())));
    }
    let writers = proc::writers_under(&live);
    if !writers.is_empty() && !force {
        return Err(refused(format!(
            "files open for writing under {}: {} (or --force)",
            live.display(),
            proc::describe(&writers)
        )));
    }
    let name = pref.name().to_string();
    let root_path = pref.root.path.clone();
    let hctx = HookCtx {
        project: Some(&name),
        project_path: Some(&live),
        owner: Some((pref.record.owner_uid, pref.record.owner_gid)),
        root: Some(&root_path),
        snapshot: Some((meta.id, pref.unit.snapshot_path(meta.id), meta.kind)),
        reason: format!("rollback to #{}", meta.id),
        ..Default::default()
    };
    hooks::run(ctx, HookEvent::PreRollback, &hctx)?;
    let safety = {
        let target = Target::for_project(pref, eff);
        snapshot::take(
            ctx,
            &target,
            &SnapOpts::new(SnapshotKind::Rollback, format!("before rollback to #{}", meta.id)),
        )?
    };
    if ctx.opts.dry_run {
        tracing::info!("[dry-run] rollback {} to snapshot #{}", live.display(), meta.id);
        return Ok(RollbackReport {
            safety_snapshot: safety.id,
            moved_nested: vec![],
            dropped_nested: vec![],
            old_uuid: pref.record.uuid.to_string(),
            new_uuid: String::new(),
            leftover: None,
        });
    }
    let mut journal = Journal::begin(
        &pref.unit.dir,
        "rollback",
        &[("snapshot", meta.id.to_string()), ("path", live.display().to_string())],
        false,
    )?;
    let nested = nested_under(ctx, &live);
    let ts = ctx.now().strftime("%Y%m%dT%H%M%S").to_string();
    let aside = sibling(&live, &format!(".bpm-rollback-{ts}"));
    journal.set("aside", aside.display().to_string())?;
    journal.step(3)?;
    rename_strict(&live, &aside)?;
    journal.step(4)?;
    if let Err(e) = materialize(ctx, pref, meta, &live) {
        let _ = rename_strict(&aside, &live);
        journal.finish()?;
        return Err(e.context("rollback: could not create the new live subvolume; original restored"));
    }
    journal.step(5)?;
    let mut moved = Vec::new();
    let mut dropped = Vec::new();
    for rel in &nested {
        let banned = eff.banlist.iter().any(|b| Path::new(b) == rel);
        if drop_build && banned {
            dropped.push(rel.clone());
            continue;
        }
        let dst = live.join(rel);
        if let Ok(dmd) = std::fs::symlink_metadata(&dst) {
            if dmd.is_dir() && crate::util::fs::dir_is_empty(&dst)? {
                std::fs::remove_dir(&dst)?;
            } else {
                tracing::warn!(
                    "rollback: {} exists in the snapshot and is not an empty placeholder; nested subvolume left in {}",
                    dst.display(),
                    aside.display()
                );
                continue;
            }
        }
        if let Some(parent) = dst.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        rename_strict(&aside.join(rel), &dst).with_context(|| format!("move nested subvolume {}", rel.display()))?;
        moved.push(rel.clone());
    }
    journal.step(6)?;
    let info = ctx.btrfs.subvol_info(&live)?;
    let old_uuid = pref.record.uuid;
    pref.record.uuid_history.push(old_uuid);
    pref.record.uuid = info.uuid;
    pref.unit.write_record(&pref.record)?;
    journal.step(7)?;
    // Recursive delete only when every nested subvolume still inside is a build dir we chose to
    // drop; anything else left behind (a user subvolume that could not be moved) must survive.
    let left_behind: Vec<&PathBuf> = nested.iter().filter(|n| !moved.contains(n)).collect();
    let recursive = !dropped.is_empty() && left_behind.iter().all(|n| dropped.contains(n));
    if left_behind.iter().any(|n| !dropped.contains(n)) {
        tracing::warn!(
            "rollback: nested subvolumes left in {}: {}",
            aside.display(),
            left_behind
                .iter()
                .filter(|n| !dropped.contains(n))
                .map(|n| n.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let leftover = match ctx.btrfs.delete_subvolume(&aside, recursive) {
        Ok(()) => None,
        Err(e) => {
            tracing::warn!("rollback: could not delete {}: {e:#}; `bpm doctor --fix` will retry", aside.display());
            Some(aside)
        }
    };
    journal.finish()?;
    hooks::run(ctx, HookEvent::PostRollback, &hctx)?;
    Ok(RollbackReport {
        safety_snapshot: safety.id,
        moved_nested: moved,
        dropped_nested: dropped,
        old_uuid: old_uuid.to_string(),
        new_uuid: info.uuid.to_string(),
        leftover,
    })
}

/// Recreate a deleted (orphaned) or archived project from a snapshot.
pub fn recreate(ctx: &Ctx, pref: &mut ProjectRef, meta: &SnapshotMeta) -> Result<String> {
    let live = pref.path().to_path_buf();
    if live.symlink_metadata().is_ok() {
        return Err(refused(format!("{} exists; use `bpm rollback` instead", live.display())));
    }
    if ctx.opts.dry_run {
        tracing::info!("[dry-run] recreate {} from snapshot #{}", live.display(), meta.id);
        return Ok(String::new());
    }
    materialize(ctx, pref, meta, &live)?;
    let info = ctx.btrfs.subvol_info(&live)?;
    let old = pref.record.uuid;
    if old != info.uuid && !pref.record.uuid_history.contains(&old) {
        pref.record.uuid_history.push(old);
    }
    pref.record.uuid = info.uuid;
    pref.unit.write_record(&pref.record)?;
    Ok(info.uuid.to_string())
}
