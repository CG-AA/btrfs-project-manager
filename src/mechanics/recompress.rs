//! Collapse a project to a single snapshot, and recompress its live tree at a higher zstd level.

use super::Target;
use super::observe::{self, Walk};
use super::snapshot::{self, DeleteMode, SnapOpts};
use crate::btrfs::DefragOpts;
use crate::config::EffectiveConfig;
use crate::ctx::Ctx;
use crate::hooks::{self, HookCtx, HookEvent};
use crate::policy::{change, thin};
use crate::project::ProjectRef;
use crate::store::journal::Journal;
use crate::store::{LockedUnit, ProjectState, RecompressRecord, SnapshotKind, SnapshotMeta, newest};
use crate::util::walk::{self, EntryInfo, EntryKind};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Serialize, Default)]
pub struct CollapseReport {
    pub new_snapshot: Option<u64>,
    pub deleted: Vec<u64>,
    pub froze: bool,
}

/// Ensure the newest snapshot equals live, then delete every policy-deletable older snapshot.
pub fn collapse(
    ctx: &Ctx,
    lu: &LockedUnit,
    pref: &ProjectRef,
    eff: &EffectiveConfig,
    st: &mut ProjectState,
    snaps: &mut Vec<SnapshotMeta>,
) -> Result<CollapseReport> {
    let mut report = CollapseReport::default();
    if st.frozen.is_some() {
        return Ok(report);
    }
    let target = Target::for_project(pref, eff);
    let now = ctx.now();
    ctx.btrfs.sync(pref.path())?;
    let live = ctx.btrfs.subvol_info(pref.path())?;
    observe::note_live(st, &live, now);
    match newest(snaps).cloned() {
        Some(n) if change::identical(&live, &n) => {
            // the newest snapshot already holds this state: count it if it was taken without stats
            if n.stats.is_none() {
                let mut n = n;
                snapshot::count_stats(ctx, lu, &target, &mut n, eff.policy.snapshot.stats_budget)?;
                if let Some(slot) = snaps.iter_mut().find(|m| m.id == n.id) {
                    *slot = n;
                }
            }
        }
        _ => {
            let mut o = SnapOpts::new(SnapshotKind::Collapse, "collapse");
            o.stats_budget = eff.policy.snapshot.stats_budget;
            let m = snapshot::take(ctx, &target, &o)?;
            observe::note_snapshot(st, &m);
            report.new_snapshot = Some(m.id);
            snaps.push(m);
        }
    }
    if observe::evaluate_guard(ctx, lu, &target, eff, st, snaps, now, Walk::Never) {
        report.froze = true;
        return Ok(report);
    }
    if newest(snaps).is_none_or(|n| n.complete_stats().is_none()) {
        tracing::warn!(
            "{}: counting the newest snapshot exceeded snapshot.stats_budget; not collapsing until the project changes",
            pref.name()
        );
        st.collapse_incomplete_ctransid = Some(live.ctransid);
        return Ok(report);
    }
    if !observe::thin_allowed(st, snaps) {
        return Ok(report);
    }
    let ids = thin::collapse_deletions(snaps, ctx.now(), eff.policy.thin.safety_ttl, st.frozen.as_ref());
    for id in ids {
        let Some(meta) = snaps.iter().find(|m| m.id == id).cloned() else {
            continue;
        };
        match snapshot::delete(ctx, lu, &pref.record, snaps, &meta, DeleteMode::Policy, "collapse") {
            Ok(()) => {
                report.deleted.push(id);
                snaps.retain(|m| m.id != id);
            }
            Err(e) => tracing::warn!("{}: collapse could not delete #{id}: {e:#}", pref.name()),
        }
    }
    Ok(report)
}

pub fn collapsed(ctx: &Ctx, snaps: &[SnapshotMeta], eff: &EffectiveConfig) -> bool {
    thin::collapse_deletions(snaps, ctx.now(), eff.policy.thin.safety_ttl, None).is_empty()
}

pub fn needs_recompress(eff: &EffectiveConfig, st: &ProjectState, live_ctransid: u64, snaps: &[SnapshotMeta]) -> bool {
    eff.policy.recompress.enabled
        && st.frozen.is_none()
        && st.recompress.as_ref().is_none_or(|r| live_ctransid > r.ctransid)
        && newest(snaps).and_then(|n| n.complete_stats()).is_some_and(|s| s.bytes <= eff.policy.recompress.max_bytes.0)
}

/// Compressed size of `data` with `zstd -<level>`.
fn zstd_len(data: &[u8], level: u8) -> Result<usize> {
    let mut child = Command::new("zstd")
        .arg(format!("-{level}"))
        .args(["-q", "-c", "-T1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn zstd")?;
    let mut stdin = child.stdin.take().unwrap();
    let payload = data.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&payload));
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out)?;
    let _ = writer.join();
    child.wait()?;
    Ok(out.len())
}

/// Estimate the gain of recompressing at `level` over zstd:1 from a sample of files.
pub fn sample_gain(ctx: &Ctx, live: &Path, exclude: &[PathBuf], level: u8) -> Result<f64> {
    let probe = |p: &Path, ino: u64| ctx.is_subvol(p, ino);
    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(live).follow_links(false).into_iter().filter_entry(|e| {
        let rel = e.path().strip_prefix(live).unwrap_or(e.path());
        !(e.depth() > 0 && (exclude.iter().any(|x| x == rel) || walk::entry_is_subvol(e, &probe)))
    });
    for e in walker.flatten().filter(|e| e.file_type().is_file()).take(20_000) {
        if e.metadata().map(|m| m.len() >= 4096).unwrap_or(false) {
            files.push(e.into_path());
        }
    }
    if files.is_empty() {
        return Ok(0.0);
    }
    let step = (files.len() / 32).max(1);
    let mut sample = Vec::new();
    for f in files.iter().step_by(step).take(32) {
        if let Ok(mut fh) = std::fs::File::open(f) {
            let mut buf = Vec::new();
            let _ = (&mut fh).take(1 << 20).read_to_end(&mut buf);
            sample.extend_from_slice(&buf);
        }
    }
    let base = zstd_len(&sample, 1)?.min(sample.len());
    let high = zstd_len(&sample, level)?;
    Ok(if base == 0 { 0.0 } else { 1.0 - high as f64 / base as f64 })
}

/// What a defragment must not change about an entry: kind, size, mtime, mode.
type EntryKey = (EntryKind, u64, Option<i128>, u32);

/// Listing keys of a snapshot tree, in path order.
///
/// The mtime of an empty directory is left out: a nested subvolume's placeholder is an empty
/// directory whose mtime is the moment the kernel instantiated its inode, so it differs between
/// walks. Non-empty directories keep theirs: replacing a file by one with the same size, mode and
/// mtime (`cp -p`, `rsync -a`, `tar -x`) shows only in the parent directory's mtime.
fn listing_keys(idx: BTreeMap<PathBuf, EntryInfo>) -> impl Iterator<Item = (PathBuf, EntryKey)> {
    let mut it = idx.into_iter().peekable();
    std::iter::from_fn(move || {
        let (path, e) = it.next()?;
        let has_children = it.peek().is_some_and(|(next, _)| next.starts_with(&path));
        let mtime = (e.kind != EntryKind::Dir || has_children).then_some(e.mtime_ns);
        Some((path, (e.kind, e.size, mtime, e.mode)))
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct RecompressReport {
    pub done: bool,
    pub skipped: Option<String>,
    pub level: u8,
    pub new_snapshot: Option<u64>,
    pub deleted: Vec<u64>,
    pub free_before: u64,
    pub free_after: u64,
}

#[allow(clippy::too_many_arguments)]
pub fn recompress(
    ctx: &Ctx,
    lu: &LockedUnit,
    pref: &ProjectRef,
    eff: &EffectiveConfig,
    st: &mut ProjectState,
    snaps: &mut Vec<SnapshotMeta>,
    level: u8,
    force: bool,
) -> Result<RecompressReport> {
    let live = pref.path().to_path_buf();
    let mut report = RecompressReport {
        done: false,
        skipped: None,
        level,
        new_snapshot: None,
        deleted: vec![],
        free_before: 0,
        free_after: 0,
    };
    let skip = |r: &mut RecompressReport, why: String| {
        tracing::info!("{}: recompress skipped: {why}", pref.name());
        r.skipped = Some(why);
    };
    if st.frozen.is_some() {
        skip(&mut report, "project is frozen".into());
        return Ok(report);
    }
    let c = collapse(ctx, lu, pref, eff, st, snaps)?;
    report.deleted.extend(c.deleted);
    if c.froze {
        skip(&mut report, "shrink guard froze the project".into());
        return Ok(report);
    }
    let Some(old) = newest(snaps).cloned() else {
        skip(&mut report, "no snapshot".into());
        return Ok(report);
    };
    let bytes = old.complete_stats().map(|s| s.bytes).unwrap_or(0);
    let needed = (bytes as f64 * 1.1) as u64 + (2 << 30);
    let free = ctx.free_bytes(&live)?;
    report.free_before = free;
    let min_free = needed.max(eff.policy.recompress.min_free.0);
    if free < min_free {
        let why = format!(
            "needs {} free, have {}",
            crate::util::bytes::fmt_bytes(min_free),
            crate::util::bytes::fmt_bytes(free)
        );
        skip(&mut report, why);
        return Ok(report);
    }
    ctx.btrfs.sync(&live)?;
    let live_info = ctx.btrfs.subvol_info(&live)?;
    if !force {
        let gain = sample_gain(ctx, &live, &eff.stats_exclude(), level)?;
        if gain < eff.policy.recompress.min_expected_gain {
            let why = format!(
                "sample gain {:.1}% below {:.1}%",
                gain * 100.0,
                eff.policy.recompress.min_expected_gain * 100.0
            );
            st.recompress = Some(RecompressRecord {
                at: ctx.now(),
                level,
                ctransid: live_info.ctransid,
                bytes_before: free,
                bytes_after: free,
                skipped: Some(why.clone()),
            });
            skip(&mut report, why);
            return Ok(report);
        }
    }
    let hctx = HookCtx {
        project: Some(pref.name()),
        project_path: Some(&live),
        owner: Some((pref.record.owner_uid, pref.record.owner_gid)),
        root: Some(&pref.root.path),
        reason: format!("zstd:{level}"),
        ..Default::default()
    };
    hooks::run(ctx, HookEvent::PreRecompress, &hctx)?;
    let mut journal = Journal::begin(
        &pref.unit.dir,
        "recompress",
        &[("level", level.to_string()), ("old_snapshot", old.id.to_string())],
        ctx.opts.dry_run,
    )?;
    journal.step(1)?;
    ctx.btrfs.defragment(&live, &DefragOpts { compress: "zstd".into(), level: Some(level), flush: true })?;
    ctx.btrfs.sync(&live)?;
    journal.step(2)?;
    let target = Target::for_project(pref, eff);
    let mut o = SnapOpts::new(SnapshotKind::Collapse, format!("recompress zstd:{level}"));
    o.stats_budget = eff.policy.snapshot.stats_budget;
    let m = snapshot::take(ctx, &target, &o)?;
    observe::note_snapshot(st, &m);
    report.new_snapshot = Some(m.id);
    snaps.push(m.clone());
    if observe::evaluate_guard(ctx, lu, &target, eff, st, snaps, ctx.now(), Walk::Never) {
        journal.finish()?;
        skip(&mut report, "shrink guard froze the project after defragment".into());
        return Ok(report);
    }
    // Defragment rewrites extents, not names, sizes or times. Anything else that differs between
    // the snapshots before and after was changed by someone else during the defragment: keep the
    // old snapshot (the only copy of the previous state) and count the change as activity.
    let probe = |p: &Path, ino: u64| ctx.is_subvol(p, ino);
    // the root's own mtime is not in the index: it is what a replacement at the top level changes
    let index = |id: u64| -> Result<(i128, BTreeMap<PathBuf, EntryInfo>)> {
        use std::os::unix::fs::MetadataExt;
        let path = pref.unit.snapshot_path(id);
        let md = std::fs::symlink_metadata(&path)?;
        Ok((walk::nanos(md.mtime(), md.mtime_nsec()), walk::tree_index(&path, &probe)?))
    };
    let (root_before, before) = index(old.id)?;
    let (root_after, after_idx) = index(m.id)?;
    let untouched = root_before == root_after && listing_keys(before).eq(listing_keys(after_idx));
    ctx.btrfs.sync(&live)?;
    let after = ctx.btrfs.subvol_info(&live)?;
    if !untouched {
        tracing::warn!(
            "{}: the project changed during recompression; snapshot #{} is kept and the change counts as activity",
            pref.name(),
            old.id
        );
        observe::note_live(st, &after, ctx.now());
        report.free_after = ctx.free_bytes(&live)?;
        st.recompress = Some(RecompressRecord {
            at: ctx.now(),
            level,
            ctransid: after.ctransid,
            bytes_before: report.free_before,
            bytes_after: report.free_after,
            skipped: None,
        });
        journal.finish()?;
        report.done = true;
        hooks::run(ctx, HookEvent::PostRecompress, &hctx)?;
        return Ok(report);
    }
    if !old.hold && old.kind.class() != crate::store::KindClass::Keep {
        match snapshot::delete(
            ctx,
            lu,
            &pref.record,
            snaps,
            &old,
            DeleteMode::Policy,
            "replaced by recompressed snapshot",
        ) {
            Ok(()) => {
                report.deleted.push(old.id);
                snaps.retain(|s| s.id != old.id);
            }
            Err(e) => tracing::warn!("{}: could not delete pre-recompress snapshot #{}: {e:#}", pref.name(), old.id),
        }
    } else {
        tracing::warn!(
            "{}: snapshot #{} is held; recompressed extents stay duplicated until it is released",
            pref.name(),
            old.id
        );
    }
    observe::note_tool_change(st, &live_info, &after, ctx.now());
    report.free_after = ctx.free_bytes(&live)?;
    st.recompress = Some(RecompressRecord {
        at: ctx.now(),
        level,
        ctransid: after.ctransid,
        bytes_before: report.free_before,
        bytes_after: report.free_after,
        skipped: None,
    });
    journal.finish()?;
    report.done = true;
    hooks::run(ctx, HookEvent::PostRecompress, &hctx)?;
    Ok(report)
}
