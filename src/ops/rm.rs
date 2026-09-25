//! `bpm rm` and `bpm forget`.

use crate::cli::{ForgetArgs, RmArgs};
use crate::ctx::Ctx;
use crate::error::{BpmError, refused, usage};
use crate::mechanics::snapshot::{self, DeleteMode};
use crate::output::emit;
use crate::project;
use crate::store::{Stage, snapid};
use anyhow::Result;

pub fn run(ctx: &Ctx, a: RmArgs) -> Result<()> {
    let (unit, record, selectors) = if a.container {
        let selectors: Vec<String> = a.project.iter().chain(a.snapshots.iter()).cloned().collect();
        if let Some(bad) = selectors.iter().find(|s| snapid::parse(s, &ctx.tz).is_err()) {
            return Err(usage(format!("{bad:?} is not a snapshot selector; --container takes only snapshot ids")));
        }
        let root = ctx.roots().into_iter().next().ok_or_else(|| usage("no root configured"))?;
        let store = ctx.store(&root);
        let unit = store.container();
        let rec = unit.read_record()?.ok_or_else(|| usage("no container store"))?;
        (unit, rec, selectors)
    } else {
        let project = a.project.as_deref().ok_or_else(|| usage("name a project and snapshots"))?;
        let pref = project::resolve(ctx, project)?;
        (pref.unit, pref.record, a.snapshots.clone())
    };
    if selectors.is_empty() {
        return Err(usage("name the snapshots to delete"));
    }
    let unit = unit.lock(ctx.cfg.global.lock_timeout)?;
    let mut snaps = unit.snapshots()?;
    let mut targets = Vec::new();
    for sel in &selectors {
        targets.push(super::select(ctx, &unit, &snaps, sel)?.clone());
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

/// Files bpm writes into a unit directory besides snapshots and `lock`.
const OWN_FILES: [&str; 4] = ["project.toml", "state.toml", "FROZEN", "op.journal"];

/// Entries of a unit directory that are neither bpm's own files nor snapshot directories.
fn unknown_entries(dir: &std::path::Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let ours = OWN_FILES.contains(&name.as_str())
            || name == "lock"
            || crate::util::fs::is_atomic_tmp(&name)
            || name.trim_end_matches(".tmp").parse::<u64>().is_ok();
        if !ours {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

pub fn forget(ctx: &Ctx, a: ForgetArgs) -> Result<()> {
    let pref = project::resolve(ctx, &a.project)?;
    // lock first: a tick waiting to snapshot must not add a snapshot this command does not see
    let lu = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    let live_ok = ctx.btrfs.subvol_info(pref.path()).map(|i| i.uuid == pref.record.uuid).unwrap_or(false);
    let st = lu.read_state()?;
    if live_ok && st.stage != Stage::Archived {
        tracing::warn!(
            "{} still exists; after `forget` the next tick adopts it again (registering the existing subvolume) unless it is ignored or has managed = false",
            pref.path().display()
        );
    }
    // refuse before deleting any snapshot, not after: a forget that stops halfway leaves a unit
    // without its record that nothing lists any more
    let scan = lu.scan()?;
    let mut blockers = unknown_entries(&pref.unit.dir)?;
    blockers.extend(scan.tmp_dirs.iter().map(|p| format!("{} (in-progress snapshot)", p.display())));
    blockers.extend(scan.without_meta.iter().map(|(id, _)| format!("#{id} (snapshot without metadata)")));
    if !blockers.is_empty() {
        return Err(refused(format!(
            "{} holds more than bpm's own files and snapshots: {}; inspect with `bpm doctor`",
            pref.unit.dir.display(),
            blockers.join(", ")
        )));
    }
    let snaps = scan.snapshots;
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
    let mut remaining = snaps.clone();
    for m in &snaps {
        snapshot::delete(ctx, &lu, &pref.record, &remaining, m, DeleteMode::User { force_held: true }, "forget")?;
        remaining.retain(|s| s.id != m.id);
    }
    let scan = lu.scan()?;
    if !scan.without_meta.is_empty() || !scan.snapshots.is_empty() || !scan.tmp_dirs.is_empty() {
        return Err(refused(format!(
            "{} still contains snapshots or in-progress snapshots; inspect with `bpm doctor`",
            pref.unit.dir.display()
        )));
    }
    // metadata of snapshots that are already gone, as the tick's cleanup removes it
    for id in scan.without_snapshot {
        ctx.fs.remove_dir_all(&lu.snapshot_dir(id))?;
    }
    // only bpm's own files: never recurse into a snapshot subvolume
    for e in std::fs::read_dir(&pref.unit.dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if OWN_FILES.contains(&name.as_str()) || crate::util::fs::is_atomic_tmp(&name) {
            let _ = ctx.fs.remove_file(&e.path());
        } else if e.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = ctx.fs.remove_dir(&e.path());
        }
    }
    // last, and still under the lock: once it is gone another holder would lock a new inode, so
    // the directory it guards must go immediately after
    if pref.unit.dir.join("lock").exists() {
        let _ = ctx.fs.remove_file(&pref.unit.dir.join("lock"));
    }
    ctx.fs
        .remove_dir(&pref.unit.dir)
        .map_err(|e| refused(format!("{} is not empty after forget ({e:#}); inspect it", pref.unit.dir.display())))?;
    drop(lu);
    emit(ctx, &serde_json::json!({"forgot": pref.name(), "deleted_snapshots": snaps.len()}), || {
        format!("{}: removed from the store ({} snapshots deleted)", pref.name(), snaps.len())
    });
    Ok(())
}
