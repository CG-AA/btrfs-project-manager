//! Adopt: convert a plain top-level directory into a subvolume (or register an existing one).

use super::Target;
use super::retire::{self, Expect, Outcome, counts};
use super::snapshot::{self, SnapOpts};
use crate::config::RootCfg;
use crate::ctx::Ctx;
use crate::error::refused;
use crate::hooks::{self, HookCtx, HookEvent};
use crate::project::{self, ProjectRef};
use crate::store::journal::Journal;
use crate::store::{ProjectRecord, ProjectState, SnapshotKind, Store};
use crate::util::fs::{copy_owner_mode, cp_a, rename_exchange, sibling};
use crate::util::relpath::RelPath;
use crate::util::{proc, walk};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct AdoptOpts {
    pub keep_build: Option<bool>,
    pub force: bool,
    pub verify_paths: bool,
    /// Refuse when any process has a file open or its working directory inside (automatic adoption).
    pub require_unused: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdoptReport {
    pub name: String,
    pub path: PathBuf,
    pub converted: bool,
    pub files: u64,
    pub bytes: u64,
    pub nested: Vec<String>,
    pub snapshot: u64,
    pub stage: String,
    /// The original directory, kept because it changed while it was being adopted.
    pub leftover: Option<PathBuf>,
}

const MIN_FREE: u64 = 2 << 30;

/// Move an orphaned store unit out of the way of a new project with the same name.
fn clear_name(store: &Store, name: &str, live_uuid: Option<crate::btrfs::Uuid>, lock_timeout: Duration) -> Result<()> {
    let unit = store.unit(name);
    let Some(rec) = unit.read_record()? else {
        return Ok(());
    };
    if Some(rec.uuid) == live_uuid {
        return Err(refused(format!("{name} is already managed")));
    }
    let new_name = format!("{name}@{}", rec.uuid.short());
    let target = store.unit(&new_name);
    if target.dir.exists() {
        bail!("cannot move old store entry {} aside: {} exists", unit.dir.display(), target.dir.display());
    }
    tracing::warn!("store entry for a previous {name} (uuid {}) moved to {new_name}", rec.uuid);
    if !store.dry_run {
        let moved = unit.lock(lock_timeout)?.rename_to(target)?;
        let mut rec = rec;
        rec.name = new_name;
        moved.write_record(&rec)?;
    }
    Ok(())
}

pub fn adopt(ctx: &Ctx, root: &RootCfg, name: &str, o: &AdoptOpts) -> Result<AdoptReport> {
    let store = ctx.store(root);
    if !store.exists() {
        bail!("store {} does not exist; run `bpm setup` first", store.dir.display());
    }
    let path = root.path.join(name);
    if name.starts_with('.') || name.contains('/') || project::is_leftover_name(name) {
        return Err(refused(format!("{name:?} is not a valid project name")));
    }
    if root.ignore.iter().any(|i| i == name) {
        return Err(refused(format!("{name} is in the ignore list of {}", root.path.display())));
    }
    let md = std::fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
    if !md.is_dir() || md.file_type().is_symlink() {
        return Err(refused(format!("{} is not a directory", path.display())));
    }
    if crate::util::fs::is_mount_point(&path) {
        return Err(refused(format!("{} is a mount point", path.display())));
    }
    let is_sub = ctx.btrfs.is_subvolume(&path)?;
    let live_uuid = if is_sub { Some(ctx.btrfs.subvol_info(&path)?.uuid) } else { None };
    let eff = project::effective(ctx, root, name, &path)?;
    if !eff.managed {
        return Err(refused(format!("{name} has managed = false")));
    }
    let tmp = sibling(&path, ".bpm-tmp");
    let old = sibling(&path, ".bpm-old");
    if !is_sub {
        for leftover in [&tmp, &old] {
            if leftover.symlink_metadata().is_ok() {
                return Err(refused(format!("leftover {} exists; run `bpm doctor --fix`", leftover.display())));
            }
        }
        let nested = walk::nested_subvolumes(&path, &|p, i| ctx.is_subvol(p, i))?;
        if !nested.is_empty() {
            let list: Vec<String> = nested.iter().map(|p| p.display().to_string()).collect();
            return Err(refused(format!(
                "{name} contains nested subvolumes ({}); copying would flatten them",
                list.join(", ")
            )));
        }
        let free = ctx.free_bytes(&root.path)?;
        if free < MIN_FREE {
            return Err(refused(format!(
                "only {} free; adopt needs at least 2G for metadata",
                crate::util::bytes::fmt_bytes(free)
            )));
        }
        let writers = proc::writers_under(&path);
        if !writers.is_empty() && !o.force {
            return Err(refused(format!(
                "files open for writing under {}: {} (retry when idle, or --force)",
                path.display(),
                proc::describe(&writers)
            )));
        }
        let users = proc::open_under(&path);
        if o.require_unused && !users.is_empty() {
            return Err(refused(format!(
                "{} is in use: {}; will retry on a later tick",
                path.display(),
                proc::describe(&users)
            )));
        }
        let cwd_users: Vec<_> = users.into_iter().filter(|u| u.cwd).collect();
        if !cwd_users.is_empty() {
            tracing::warn!(
                "processes with a working directory inside {name} will keep the old directory until they `cd` again: {}",
                proc::describe(&cwd_users)
            );
        }
    }
    clear_name(&store, name, live_uuid, ctx.cfg.global.lock_timeout)?;
    let owner = (md.uid(), md.gid());
    hooks::run(
        ctx,
        HookEvent::PreAdopt,
        &HookCtx {
            project: Some(name),
            project_path: Some(&path),
            owner: Some(owner),
            root: Some(&root.path),
            reason: "adopt".into(),
            ..Default::default()
        },
    )?;
    if ctx.opts.dry_run {
        tracing::info!(
            "[dry-run] adopt {} (banlist: {})",
            path.display(),
            eff.banlist.iter().map(|b| b.as_str()).collect::<Vec<_>>().join(", ")
        );
        return Ok(AdoptReport {
            name: name.into(),
            path,
            converted: !is_sub,
            files: 0,
            bytes: 0,
            nested: vec![],
            snapshot: 0,
            stage: "active".into(),
            leftover: None,
        });
    }

    let unit = store.unit(name).lock(ctx.cfg.global.lock_timeout)?;
    let mut journal = Journal::begin(&unit.dir, "adopt", &[("path", path.display().to_string())], false)?;
    let keep_build = o.keep_build.unwrap_or(eff.policy.keep_build_on_adopt);
    // compared with file timestamps, so the real clock
    let started = jiff::Timestamp::now();
    let banned_paths = eff.banlist_paths();
    let mut copied: Option<walk::TreeStats> = None;
    let mut nested_made = Vec::new();

    if !is_sub {
        journal.step(1)?;
        ctx.btrfs.create_subvolume(&tmp)?;
        let copy = (|| -> Result<()> {
            copy_owner_mode(&md, &tmp)?;
            super::xattr::copy_xattrs(&path, &tmp)?;
            journal.step(2)?;
            let banned_top: Vec<&str> = eff.banlist.iter().filter(|b| b.is_top_level()).map(|b| b.as_str()).collect();
            let mut entries: Vec<_> = std::fs::read_dir(&path)?.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let fname = e.file_name();
                // banned directories become nested subvolumes in step 4; a banned name that is a
                // symlink or a file is copied like anything else
                let real_dir = std::fs::symlink_metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false);
                if real_dir && banned_top.contains(&fname.to_string_lossy().as_ref()) {
                    continue;
                }
                cp_a(&e.path(), &tmp.join(&fname), ctx.reflink)?;
            }
            for rel in eff.banlist.iter().filter(|b| !b.is_top_level()) {
                // a symlinked parent was copied as a symlink: never delete through it
                let Ok(p) = rel.under(&tmp) else {
                    tracing::warn!(
                        "{name}: banned path {rel} has a symlinked or non-directory parent; left in snapshots"
                    );
                    continue;
                };
                if p.symlink_metadata().is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
                    std::fs::remove_dir_all(&p)?;
                }
            }
            journal.step(3)?;
            let probe = |p: &Path, ino: u64| ctx.is_subvol(p, ino);
            let exclude = banned_paths.clone();
            let a = walk::tree_stats(&path, &probe, &exclude, &[], Duration::from_secs(24 * 3600))?;
            let b = walk::tree_stats(&tmp, &probe, &exclude, &[], Duration::from_secs(24 * 3600))?;
            // path bytes: a hardlink between two top-level entries becomes two files in the copy
            if counts(&a) != counts(&b) {
                bail!(
                    "verification failed: source {:?} vs copy {:?} (files, dirs, symlinks, others, bytes)",
                    counts(&a),
                    counts(&b)
                );
            }
            if a.bytes != a.path_bytes {
                tracing::warn!("{name}: hardlinks between top-level entries are copied as separate files");
            }
            if o.verify_paths {
                let ia = walk::tree_index(&path, &probe)?;
                let ib = walk::tree_index(&tmp, &probe)?;
                let filt = |m: std::collections::BTreeMap<PathBuf, walk::EntryInfo>| {
                    m.into_iter()
                        .filter(|(k, _)| !exclude.iter().any(|x| k.starts_with(x)))
                        .map(|(k, v)| (k, v.kind, v.size, v.mode))
                        .collect::<Vec<_>>()
                };
                if filt(ia) != filt(ib) {
                    bail!("verification failed: path listing differs between {} and its copy", path.display());
                }
            }
            if a.newest_change.is_some_and(|c| c > started) {
                bail!("{} was modified during adoption; retry when idle", path.display());
            }
            copied = Some(b);
            journal.step(4)?;
            let mut banned: Vec<&RelPath> = eff.banlist.iter().collect();
            banned.sort_by_key(|b| b.as_str().matches('/').count());
            for rel in banned {
                let (Ok(src), Ok(dst)) = (rel.under(&path), rel.under(&tmp)) else {
                    continue;
                };
                let Ok(smd) = std::fs::symlink_metadata(&src) else {
                    continue;
                };
                if !smd.is_dir() || smd.file_type().is_symlink() {
                    continue;
                }
                if !dst.parent().is_some_and(|p| p.is_dir()) {
                    continue;
                }
                ctx.btrfs.create_subvolume(&dst)?;
                copy_owner_mode(&smd, &dst)?;
                if keep_build {
                    cp_a(&src, &dst, ctx.reflink)?;
                }
                let _ = super::xattr::copy_times(&src, &dst);
                nested_made.push(rel.to_string());
            }
            super::xattr::copy_times(&path, &tmp)?;
            // last check before the swap: step 4 can take minutes for large build directories
            let writers = proc::writers_under(&path);
            if !writers.is_empty() && !o.force {
                bail!(
                    "files opened for writing under {} during adoption: {}",
                    path.display(),
                    proc::describe(&writers)
                );
            }
            if o.require_unused {
                let users = proc::open_under(&path);
                if !users.is_empty() {
                    bail!("{} came into use during adoption: {}", path.display(), proc::describe(&users));
                }
            }
            let again = walk::tree_stats(&path, &probe, &banned_paths, &[], Duration::from_secs(24 * 3600))?;
            if again.newest_change.is_some_and(|c| c > started) {
                bail!("{} was modified during adoption; retry when idle", path.display());
            }
            Ok(())
        })();
        if let Err(e) = copy {
            let _ = ctx.btrfs.delete_subvolume(&tmp, true);
            let _ = journal.finish();
            return Err(e.context(format!("adopt {name}: copy into new subvolume failed; original untouched")));
        }
        journal.step(5)?;
        rename_exchange(&path, &tmp).with_context(|| format!("swap {} into place", path.display()))?;
        std::fs::rename(&tmp, &old)?;
    }

    journal.step(6)?;
    let info = ctx.btrfs.subvol_info(&path)?;
    let record = ProjectRecord::new(name, &path, info.uuid, owner.0, owner.1, ctx.now());
    unit.write_record(&record)?;
    let pref = ProjectRef { root: root.clone(), store: store.clone(), unit: (*unit).clone(), record: record.clone() };
    let mut st = ProjectState::default();
    st.banlist_seen.extend(nested_made.iter().cloned());
    let ban = super::banlist::enforce_cheap(ctx, &path, &eff, &mut st, owner, true)?;
    for b in &ban {
        if let super::banlist::BanAction::Created(r) | super::banlist::BanAction::Recreated(r) = b {
            nested_made.push(r.clone());
        }
    }
    let target = Target::for_project(&pref, &eff);
    let mut opts = SnapOpts::new(SnapshotKind::Adopt, if is_sub { "registered existing subvolume" } else { "adopted" });
    opts.stats_budget = eff.policy.snapshot.stats_budget;
    let meta = snapshot::take(ctx, &target, &opts)?;
    let last_change = meta.stats.as_ref().and_then(|s| s.newest_mtime).unwrap_or(ctx.now());
    super::observe::init_new_live(ctx, &eff, &mut st, &meta, &path, last_change)?;
    unit.write_state(&st)?;

    journal.step(7)?;
    let mut leftover = None;
    if !is_sub {
        // writes that reached the original after the checks above (a shell whose working
        // directory is inside) exist only here
        let expect = Expect::Tree { stats: copied, not_after: started, exclude: banned_paths.clone() };
        if let Outcome::Kept(p) = retire::retire(ctx, &old, &expect, &[], "adopt")? {
            leftover = Some(p);
        }
    }
    journal.finish()?;
    let (files, bytes) = meta.stats.as_ref().map(|s| (s.files, s.bytes)).unwrap_or((0, 0));
    hooks::run(
        ctx,
        HookEvent::PostAdopt,
        &HookCtx {
            project: Some(name),
            project_path: Some(&path),
            owner: Some(owner),
            root: Some(&root.path),
            snapshot: Some((meta.id, unit.snapshot_path(meta.id), meta.kind)),
            reason: "adopt".into(),
            ..Default::default()
        },
    )?;
    Ok(AdoptReport {
        name: name.into(),
        path,
        converted: !is_sub,
        files,
        bytes,
        nested: nested_made,
        snapshot: meta.id,
        stage: st.stage.to_string(),
        leftover,
    })
}
