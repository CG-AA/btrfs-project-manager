//! Take and delete snapshots. Deletion always proves ownership first.

use super::Target;
use crate::ctx::Ctx;
use crate::error::refused;
use crate::hooks::{self, HookCtx, HookEvent};
use crate::store::{KindClass, ProjectRecord, SnapshotKind, SnapshotMeta, Unit, newest};
use crate::util::walk;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct SnapOpts {
    pub kind: SnapshotKind,
    pub reason: String,
    pub stats: bool,
    pub stats_budget: Duration,
    pub hold: bool,
    pub pair: Option<u64>,
}

impl SnapOpts {
    pub fn new(kind: SnapshotKind, reason: impl Into<String>) -> Self {
        SnapOpts {
            kind,
            reason: reason.into(),
            stats: true,
            stats_budget: Duration::from_secs(120),
            hold: false,
            pair: None,
        }
    }
}

fn hook_ctx<'a>(t: &'a Target<'a>, id: u64, path: PathBuf, o: &SnapOpts) -> HookCtx<'a> {
    HookCtx {
        project: Some(t.name),
        project_path: Some(t.live),
        owner: t.owner,
        root: Some(t.root),
        snapshot: Some((id, path, o.kind)),
        reason: o.reason.clone(),
        ..Default::default()
    }
}

fn reserve_id(unit: &Unit) -> Result<(u64, PathBuf)> {
    for _ in 0..100 {
        let id = unit.next_id()?;
        let tmp = unit.dir.join(format!("{id}.tmp"));
        match std::fs::create_dir(&tmp) {
            Ok(()) => {
                if unit.snapshot_dir(id).exists() {
                    let _ = std::fs::remove_dir(&tmp);
                    continue;
                }
                return Ok((id, tmp));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("create {}", tmp.display())),
        }
    }
    bail!("could not reserve a snapshot id in {}", unit.dir.display())
}

pub fn take(ctx: &Ctx, t: &Target, o: &SnapOpts) -> Result<SnapshotMeta> {
    t.unit.ensure_dir()?;
    let live_before = ctx.btrfs.subvol_info(t.live).with_context(|| format!("read {}", t.live.display()))?;
    if let Some(u) = t.expected_uuid {
        if live_before.uuid != u {
            bail!("{} is subvolume {} but the store expects {}", t.live.display(), live_before.uuid, u);
        }
    }
    if ctx.opts.dry_run {
        let id = t.unit.next_id()?;
        let final_dir = t.unit.snapshot_dir(id);
        tracing::info!("[dry-run] snapshot {} -> {} ({})", t.live.display(), final_dir.display(), o.kind);
        return Ok(SnapshotMeta {
            format: 1,
            id,
            project: t.name.into(),
            created: ctx.now(),
            kind: o.kind,
            reason: o.reason.clone(),
            pair: o.pair,
            hold: o.hold,
            hold_note: String::new(),
            source_uuid: live_before.uuid,
            source_ctransid: live_before.ctransid,
            snapshot_uuid: crate::btrfs::Uuid::NIL,
            snapshot_otransid: live_before.generation,
            received_uuid: None,
            stats: None,
            origin: ctx.origin(),
        });
    }

    // Reserve an id atomically (mkdir), so concurrent snapshots of one project never collide.
    let (id, tmp) = reserve_id(t.unit)?;
    let final_dir = t.unit.snapshot_dir(id);
    if let Err(e) = hooks::run(ctx, HookEvent::PreSnapshot, &hook_ctx(t, id, final_dir.join("snapshot"), o)) {
        let _ = std::fs::remove_dir(&tmp);
        return Err(e);
    }
    let snap_path = tmp.join("snapshot");
    if let Err(e) = ctx.btrfs.snapshot(t.live, &snap_path, true) {
        let _ = std::fs::remove_dir(&tmp);
        return Err(e);
    }
    let result = (|| -> Result<SnapshotMeta> {
        ctx.btrfs.sync(&t.unit.dir)?;
        let s = ctx.btrfs.subvol_info(&snap_path)?;
        if s.parent_uuid != Some(live_before.uuid) {
            bail!("snapshot parent uuid {:?} does not match live {}", s.parent_uuid, live_before.uuid);
        }
        let stats = if o.stats {
            let probe = |p: &std::path::Path, ino: u64| ctx.is_subvol(p, ino);
            match walk::tree_stats(&snap_path, &probe, &t.stats_exclude, &t.sentinels, o.stats_budget) {
                Ok(st) => Some(st),
                Err(e) => {
                    tracing::warn!("{}: stats walk failed: {e:#}", t.name);
                    None
                }
            }
        } else {
            None
        };
        let meta = SnapshotMeta {
            format: 1,
            id,
            project: t.name.into(),
            created: ctx.now(),
            kind: o.kind,
            reason: o.reason.clone(),
            pair: o.pair,
            hold: o.hold,
            hold_note: if o.hold { "held at creation".into() } else { String::new() },
            source_uuid: live_before.uuid,
            source_ctransid: s.ctransid,
            snapshot_uuid: s.uuid,
            snapshot_otransid: s.otransid,
            received_uuid: None,
            stats,
            origin: ctx.origin(),
        };
        t.unit.write_meta(&tmp, &meta)?;
        std::fs::rename(&tmp, &final_dir)
            .with_context(|| format!("rename {} -> {}", tmp.display(), final_dir.display()))?;
        if let Ok(d) = std::fs::File::open(&t.unit.dir) {
            let _ = d.sync_all();
        }
        Ok(meta)
    })();
    let meta = match result {
        Ok(m) => m,
        Err(e) => {
            if snap_path.exists() {
                let _ = ctx.btrfs.delete_subvolume(&snap_path, false);
            }
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    tracing::debug!("{}: snapshot #{} ({}) {}", t.name, meta.id, meta.kind, meta.reason);
    hooks::run(ctx, HookEvent::PostSnapshot, &hook_ctx(t, id, t.unit.snapshot_path(id), o))?;
    Ok(meta)
}

/// Count files and bytes of an existing (read-only) snapshot and store them in its metadata.
/// Used for snapshots taken without stats, so the shrink guard can still evaluate them.
pub fn count_stats(ctx: &Ctx, t: &Target, meta: &mut SnapshotMeta, budget: Duration) -> Result<()> {
    let path = t.unit.snapshot_path(meta.id);
    let probe = |p: &std::path::Path, ino: u64| ctx.is_subvol(p, ino);
    let stats = walk::tree_stats(&path, &probe, &t.stats_exclude, &t.sentinels, budget)
        .with_context(|| format!("count snapshot #{}", meta.id))?;
    meta.stats = Some(stats);
    t.unit.update_meta(meta)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteMode {
    /// Retention or collapse: never held, never newest, never keep-class.
    Policy,
    /// Explicit `bpm rm`: held requires `force_held`; newest allowed.
    User { force_held: bool },
}

/// Prove the snapshot belongs to this unit's project, then delete it.
pub fn delete(
    ctx: &Ctx,
    unit: &Unit,
    record: &ProjectRecord,
    snaps: &[SnapshotMeta],
    meta: &SnapshotMeta,
    mode: DeleteMode,
    why: &str,
) -> Result<()> {
    let path = unit.snapshot_path(meta.id);
    let canon = std::fs::canonicalize(&path).with_context(|| format!("resolve {}", path.display()))?;
    let unit_canon = std::fs::canonicalize(&unit.dir)?;
    if canon.parent().and_then(|p| p.parent()) != Some(unit_canon.as_path()) {
        return Err(refused(format!("{} is not inside {}", canon.display(), unit_canon.display())));
    }
    if !ctx.btrfs.is_subvolume(&canon)? {
        return Err(refused(format!("{} is not a subvolume", canon.display())));
    }
    let info = ctx.btrfs.subvol_info(&canon)?;
    if !info.readonly() {
        return Err(refused(format!("{} is not read-only", canon.display())));
    }
    if info.uuid != meta.snapshot_uuid {
        return Err(refused(format!(
            "snapshot #{} uuid {} does not match metadata {}",
            meta.id, info.uuid, meta.snapshot_uuid
        )));
    }
    let owned = record.owns(info.parent_uuid)
        || (meta.kind == SnapshotKind::Received
            && info.received_uuid.is_some()
            && info.received_uuid == meta.received_uuid);
    if !owned {
        return Err(refused(format!(
            "snapshot #{} parent {:?} is not a subvolume of project {}",
            meta.id, info.parent_uuid, record.name
        )));
    }
    match mode {
        DeleteMode::Policy => {
            if meta.hold || meta.kind.class() == KindClass::Keep {
                return Err(refused(format!("snapshot #{} is held or a keep-kind ({})", meta.id, meta.kind)));
            }
            if newest(snaps).map(|n| n.id) == Some(meta.id) {
                return Err(refused(format!("snapshot #{} is the newest", meta.id)));
            }
        }
        DeleteMode::User { force_held } => {
            if meta.hold && !force_held {
                return Err(refused(format!(
                    "snapshot #{} is held ({}); unhold it or pass --force-held",
                    meta.id, meta.hold_note
                )));
            }
        }
    }
    let hctx = HookCtx {
        project: Some(&record.name),
        project_path: Some(&record.path),
        owner: Some((record.owner_uid, record.owner_gid)),
        snapshot: Some((meta.id, canon.clone(), meta.kind)),
        reason: why.to_string(),
        ..Default::default()
    };
    hooks::run(ctx, HookEvent::PreDelete, &hctx)?;
    ctx.btrfs.delete_subvolume(&canon, false)?;
    if !ctx.opts.dry_run {
        std::fs::remove_dir_all(unit.snapshot_dir(meta.id))
            .with_context(|| format!("remove {}", unit.snapshot_dir(meta.id).display()))?;
    }
    tracing::debug!("{}: deleted snapshot #{} ({why})", record.name, meta.id);
    hooks::run(ctx, HookEvent::PostDelete, &hctx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reserved_ids_are_unique() {
        let d = tempfile::tempdir().unwrap();
        let store = crate::store::Store::new(d.path(), ".bpm", false);
        let unit = store.unit("p");
        unit.ensure_dir().unwrap();
        std::fs::create_dir_all(unit.snapshot_dir(3)).unwrap();
        let (a, _) = reserve_id(&unit).unwrap();
        let (b, _) = reserve_id(&unit).unwrap();
        assert_eq!((a, b), (4, 5));
    }
}
