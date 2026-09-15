//! `bpm archive` / `bpm unarchive` (manual only).

use crate::cli::{ArchiveArgs, UnarchiveArgs};
use crate::ctx::Ctx;
use crate::error::refused;
use crate::hooks::{self, HookCtx, HookEvent};
use crate::mechanics::snapshot::{self, DeleteMode, SnapOpts};
use crate::mechanics::{Target, archive, banlist, observe, rollback};
use crate::output::emit;
use crate::project::{self, ProjectRef};
use crate::store::{ProjectRecord, SnapshotKind, Stage, newest};
use anyhow::Result;

pub fn archive(ctx: &Ctx, a: ArchiveArgs) -> Result<()> {
    let pref = project::resolve(ctx, &a.project)?;
    let eff = super::effective(ctx, &pref)?;
    if (a.delete_live || a.delete_snapshots) && !a.yes {
        return Err(refused("--delete-live and --delete-snapshots need --yes"));
    }
    let _l = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    let mut st = pref.unit.read_state()?;
    if st.frozen.is_some() {
        return Err(refused(format!("{} is frozen; resolve that first (bpm status {})", pref.name(), pref.name())));
    }
    let mut snaps = pref.unit.snapshots()?;
    let live_exists = ctx.btrfs.subvol_info(pref.path()).map(|i| i.uuid == pref.record.uuid).unwrap_or(false);
    let meta = match &a.snapshot {
        Some(sel) => super::select(ctx, &pref.unit, &snaps, sel)?.clone(),
        None => {
            if !live_exists {
                return Err(refused(format!("{} does not exist; pass --snapshot", pref.path().display())));
            }
            let live = ctx.btrfs.subvol_info(pref.path())?;
            match newest(&snaps) {
                Some(n) if crate::policy::change::identical(&live, n) => n.clone(),
                _ => {
                    let target = Target::for_project(&pref, &eff);
                    let m = snapshot::take(ctx, &target, &SnapOpts::new(SnapshotKind::Manual, "archive"))?;
                    snaps.push(m.clone());
                    m
                }
            }
        }
    };
    let nested: Vec<String> = st.banlist_seen.iter().cloned().collect();
    let hctx = HookCtx {
        project: Some(pref.name()),
        project_path: Some(pref.path()),
        owner: Some((pref.record.owner_uid, pref.record.owner_gid)),
        root: Some(&pref.root.path),
        snapshot: Some((meta.id, pref.unit.snapshot_path(meta.id), meta.kind)),
        reason: "archive".into(),
        ..Default::default()
    };
    hooks::run(ctx, HookEvent::PreArchive, &hctx)?;
    let dest = a.dest.clone().unwrap_or_else(|| ctx.cfg.global.archive_dir.clone());
    let level = a.level.unwrap_or(ctx.cfg.global.archive_zstd_level);
    let report = archive::archive(ctx, &pref, &meta, nested, &dest, level, a.fast)?;
    if ctx.opts.dry_run {
        return Ok(());
    }
    st.archives.push(crate::store::ArchiveRecord {
        file: report.file.clone(),
        sha256: report.sha256.clone(),
        at: ctx.now(),
        snapshot: meta.id,
    });
    let mut deleted = Vec::new();
    if a.delete_snapshots {
        let mut remaining = snaps.clone();
        for m in snaps.iter().filter(|m| !m.hold && m.id != meta.id) {
            snapshot::delete(
                ctx,
                &pref.unit,
                &pref.record,
                &remaining,
                m,
                DeleteMode::User { force_held: false },
                "archived",
            )?;
            remaining.retain(|s| s.id != m.id);
            deleted.push(m.id);
        }
    }
    if a.delete_live && live_exists {
        let mut unprotected = Vec::new();
        let mut it = walkdir::WalkDir::new(pref.path()).follow_links(false).min_depth(1).into_iter();
        while let Some(Ok(e)) = it.next() {
            if crate::util::walk::entry_is_subvol(&e, &|p, i| ctx.is_subvol(p, i)) {
                let rel = e.path().strip_prefix(pref.path()).unwrap().to_string_lossy().into_owned();
                if !eff.banlist.contains(&rel) {
                    unprotected.push(rel);
                }
                it.skip_current_dir();
            }
        }
        if !unprotected.is_empty() {
            pref.unit.write_state(&st)?;
            return Err(refused(format!(
                "archive written, but {} contains nested subvolumes that are not in the archive ({}); live project kept",
                pref.path().display(),
                unprotected.join(", ")
            )));
        }
        let writers = crate::util::proc::writers_under(pref.path());
        if !writers.is_empty() {
            return Err(refused(format!(
                "archive written, but files are open for writing under {}: {}; live project kept",
                pref.path().display(),
                crate::util::proc::describe(&writers)
            )));
        }
        ctx.btrfs.delete_subvolume(pref.path(), true)?;
        st.set_stage(Stage::Archived, ctx.now());
    }
    pref.unit.write_state(&st)?;
    hooks::run(ctx, HookEvent::PostArchive, &hctx)?;
    emit(
        ctx,
        &serde_json::json!({"project": pref.name(), "snapshot": meta.id, "archive": report, "deleted_snapshots": deleted, "deleted_live": a.delete_live}),
        || {
            format!(
                "{}: snapshot #{} archived to {} ({}, sha256 {})\nNOTE: {} is on the same filesystem as {}; this saves space but is not a backup. Copy the .zst, .manifest.toml and .sha256 files off this machine.",
                pref.name(),
                meta.id,
                report.file.display(),
                crate::util::bytes::fmt_bytes(report.bytes),
                &report.sha256[..16.min(report.sha256.len())],
                dest.display(),
                pref.root.path.display()
            )
        },
    );
    Ok(())
}

pub fn unarchive(ctx: &Ctx, a: UnarchiveArgs) -> Result<()> {
    let dest = ctx.cfg.global.archive_dir.clone();
    let file = match &a.file {
        Some(f) => f.clone(),
        None => archive::newest_archive(&dest, &a.project)?,
    };
    let manifest = archive::read_manifest(&file)?;
    let mut pref = match project::resolve(ctx, &a.project) {
        Ok(p) => p,
        Err(_) => {
            let (root, name) = project::locate_path(ctx, &manifest.original_path)
                .or_else(|| ctx.roots().first().map(|r| (r.clone(), manifest.project.clone())))
                .ok_or_else(|| crate::error::usage("no root for this archive; pass --root"))?;
            let store = ctx.store(&root);
            let unit = store.unit(&name);
            let mut record = ProjectRecord::new(
                &name,
                &root.path.join(&name),
                manifest.project_uuid,
                manifest.owner_uid,
                manifest.owner_gid,
                ctx.now(),
            );
            record.uuid_history = manifest.uuid_history.clone();
            unit.write_record(&record)?;
            ProjectRef { root, store, unit, record }
        }
    };
    let _l = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    let meta = archive::receive(ctx, &pref, &file, &manifest)?;
    let mut st = pref.unit.read_state()?;
    st.banlist_seen.extend(manifest.nested.iter().cloned());
    let mut recreated = false;
    if !a.no_live && !ctx.opts.dry_run {
        if pref.path().symlink_metadata().is_ok() {
            tracing::warn!(
                "{} exists; received snapshot #{} stays in the store (use `bpm rollback {} {}`)",
                pref.path().display(),
                meta.id,
                pref.name(),
                meta.id
            );
        } else {
            rollback::recreate(ctx, &mut pref, &meta)?;
            let eff = super::effective(ctx, &pref)?;
            banlist::enforce_cheap(
                ctx,
                pref.path(),
                &eff,
                &mut st,
                (pref.record.owner_uid, pref.record.owner_gid),
                true,
            )?;
            let target = Target::for_project(&pref, &eff);
            let post = snapshot::take(
                ctx,
                &target,
                &SnapOpts::new(SnapshotKind::Post, format!("unarchived from #{}", meta.id)),
            )?;
            observe::init_new_live(ctx, &eff, &mut st, &post, pref.path(), ctx.now())?;
            recreated = true;
        }
    }
    pref.unit.write_state(&st)?;
    emit(
        ctx,
        &serde_json::json!({"project": pref.name(), "received_snapshot": meta.id, "recreated": recreated}),
        || {
            format!(
                "{}: received {} as held snapshot #{}{}",
                pref.name(),
                file.display(),
                meta.id,
                if recreated { format!("; recreated {}", pref.path().display()) } else { String::new() }
            )
        },
    );
    Ok(())
}
