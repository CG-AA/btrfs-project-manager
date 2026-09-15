//! Restore paths from a snapshot, roll a whole project back, or recreate a deleted project.

use super::Target;
use super::retire::{self, Expect, Outcome};
use super::snapshot::{self, SnapOpts};
use crate::config::EffectiveConfig;
use crate::ctx::Ctx;
use crate::error::refused;
use crate::hooks::{self, HookCtx, HookEvent};
use crate::project::ProjectRef;
use crate::store::journal::Journal;
use crate::store::{SnapshotKind, SnapshotMeta};
use crate::util::fs::{lchown, rename_exchange, rename_strict, safe_relative, sibling};
use crate::util::relpath::RelPath;
use crate::util::{proc, walk};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize)]
pub struct RestoreReport {
    pub restored: Vec<PathBuf>,
    pub pre_snapshot: Option<u64>,
    pub warnings: Vec<String>,
    /// Replaced paths kept because they held something the pre-restore snapshot does not.
    pub kept: Vec<PathBuf>,
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
    let mut report = RestoreReport { restored: vec![], pre_snapshot: None, warnings: vec![], kept: vec![] };
    let mut plan = Vec::new();
    for p in paths {
        let rel = normalize_rel(&live, p)?;
        let relpath =
            RelPath::parse(&rel.to_string_lossy()).ok_or_else(|| refused(format!("invalid relative path {p:?}")))?;
        // the snapshot is a copy of the project: a symlinked directory in it leads outside too
        let src = relpath.under(&snap)?;
        let smd = std::fs::symlink_metadata(&src)
            .map_err(|_| crate::error::not_found(format!("{} is not in snapshot #{}", rel.display(), meta.id)))?;
        if eff.banlist.iter().any(|b| rel.starts_with(b.as_path())) {
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
            // never create or replace through a symlink that leads out of the project
            None => relpath.under(&live)?,
        };
        if dst.symlink_metadata().is_ok() && !overwrite {
            return Err(refused(format!(
                "{} exists; pass --overwrite (a pre-restore snapshot is taken first)",
                dst.display()
            )));
        }
        plan.push((rel, src, dst));
    }
    // compared with file timestamps, so the real clock
    let started = jiff::Timestamp::now();
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
        let existing = std::fs::symlink_metadata(&dst).ok();
        if let Some(dmd) = &existing {
            if dmd.is_dir() && ctx.btrfs.is_subvolume(&dst)? {
                bail!("{} is a subvolume; refusing to replace it", dst.display());
            }
        }
        // copy next to the destination first: a failed copy leaves the destination untouched
        let tmp = sibling(&dst, ".bpm-tmp");
        if tmp.symlink_metadata().is_ok() {
            bail!("{} exists (an interrupted restore?); remove it first", tmp.display());
        }
        if let Err(e) = crate::util::fs::cp_a(&src, &tmp, ctx.reflink) {
            if std::fs::symlink_metadata(&tmp).is_ok_and(|m| m.is_dir()) {
                let _ = std::fs::remove_dir_all(&tmp);
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
            return Err(e);
        }
        if existing.is_some() {
            rename_exchange(&dst, &tmp)?;
            // `tmp` now holds what was replaced; it is deleted only if the pre-restore snapshot
            // has all of it (no nested subvolumes or mounts, nothing written since)
            let expect = Expect::Tree { stats: None, not_after: started, exclude: vec![] };
            let in_project = dst.starts_with(&live);
            match if in_project {
                retire::retire(ctx, &tmp, &expect, &[], "restore")?
            } else {
                retire::delete(
                    ctx,
                    retire::prove(ctx, &tmp, &expect, &[]).map_err(|e| {
                        refused(format!(
                            "{} was replaced but could not be removed ({e}); it is at {}",
                            dst.display(),
                            tmp.display()
                        ))
                    })?,
                )
                .map(|_| Outcome::Deleted)?
            } {
                Outcome::Deleted => {}
                Outcome::Kept(k) => report.kept.push(k),
            }
        } else {
            rename_strict(&tmp, &dst)?;
        }
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

fn nested_under(ctx: &Ctx, dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(walk::nested_subvolumes(dir, &|p, i| ctx.is_subvol(p, i))?
        .into_iter()
        .map(|p| p.strip_prefix(dir).unwrap().to_path_buf())
        .collect())
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
        return Err(refused(format!(
            "{} does not exist; use `bpm restore {} --recreate`",
            live.display(),
            pref.name()
        )));
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
    if ctx.opts.dry_run {
        tracing::info!("[dry-run] rollback {} to snapshot #{}", live.display(), meta.id);
        let target = Target::for_project(pref, eff);
        let safety = snapshot::take(
            ctx,
            &target,
            &SnapOpts::new(SnapshotKind::Rollback, format!("before rollback to #{}", meta.id)),
        )?;
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
    let nested = nested_under(ctx, &live)?;
    let ts = ctx.now().strftime("%Y%m%dT%H%M%S").to_string();
    let aside = sibling(&live, &format!(".bpm-rollback-{ts}"));
    journal.set("aside", aside.display().to_string())?;
    journal.step(3)?;
    // Move the live tree aside first and snapshot it there: a write that still lands in it
    // (a shell whose working directory is inside) is caught before it is deleted.
    rename_strict(&live, &aside)?;
    let safety = {
        let mut target = Target::for_project(pref, eff);
        target.live = &aside;
        snapshot::take(ctx, &target, &SnapOpts::new(SnapshotKind::Rollback, format!("before rollback to #{}", meta.id)))
    };
    let safety = match safety {
        Ok(m) => m,
        Err(e) => {
            let _ = rename_strict(&aside, &live);
            journal.finish()?;
            return Err(e.context("rollback: could not snapshot the current state; nothing changed"));
        }
    };
    journal.set("safety_snapshot", safety.id.to_string())?;
    journal.step(4)?;
    if let Err(e) = materialize(ctx, pref, meta, &live) {
        let _ = rename_strict(&aside, &live);
        journal.finish()?;
        return Err(e.context("rollback: could not create the new live subvolume; original restored"));
    }
    journal.step(5)?;
    // record the new identity right away, so an interruption below leaves a managed project
    let info = ctx.btrfs.subvol_info(&live)?;
    let old_uuid = pref.record.uuid;
    pref.record.uuid_history.push(old_uuid);
    pref.record.uuid = info.uuid;
    pref.unit.write_record(&pref.record)?;
    journal.step(6)?;
    // checked before nested subvolumes are moved out (which changes the aside's counters)
    let unchanged = retire::prove(ctx, &aside, &Expect::Snapshot { kept: &safety }, &nested).map(|_| ());
    let mut moved = Vec::new();
    let mut dropped = Vec::new();
    for rel in &nested {
        let banned = eff.banlist.iter().any(|b| b.as_path() == rel);
        if drop_build && banned {
            dropped.push(rel.clone());
            continue;
        }
        let Some(dst) = RelPath::parse(&rel.to_string_lossy()).and_then(|r| r.under(&live).ok()) else {
            tracing::warn!(
                "rollback: {} is under a symlink in the snapshot; nested subvolume left in {}",
                rel.display(),
                aside.display()
            );
            continue;
        };
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
    journal.step(7)?;
    // Delete the aside only when it still equals the safety snapshot and holds nothing else:
    // nested subvolumes left behind are build dirs the user chose to drop, or it is kept.
    let leftover = match unchanged {
        Err(why) => keep_aside(ctx, &aside, &why.to_string())?,
        Ok(()) => match retire::prove(ctx, &aside, &Expect::Verified, &dropped) {
            Ok(proof) => match retire::delete(ctx, proof) {
                Ok(()) => None,
                Err(e) => {
                    tracing::warn!(
                        "rollback: could not delete {}: {e:#}; `bpm doctor --fix` will retry",
                        aside.display()
                    );
                    Some(aside)
                }
            },
            Err(why) => keep_aside(ctx, &aside, &why.to_string())?,
        },
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

fn keep_aside(ctx: &Ctx, aside: &Path, why: &str) -> Result<Option<PathBuf>> {
    let keep = retire::keep_name(ctx, aside, "rollback");
    rename_strict(aside, &keep)?;
    tracing::error!(
        "rollback: kept the previous tree as {} instead of deleting it: {why}. Compare with the rollback snapshot and merge by hand, then delete it",
        keep.display()
    );
    Ok(Some(keep))
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
