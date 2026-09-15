//! Keep banned directories (build output, caches) as nested subvolumes so snapshots skip them.

use crate::config::{EffectiveConfig, Precreate};
use crate::ctx::Ctx;
use crate::error::refused;
use crate::store::ProjectState;
use crate::util::fs::{copy_owner_mode, cp_a, dir_is_empty, rename_exchange, set_owner_mode, sibling};
use crate::util::proc;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::Path;

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "action", content = "path", rename_all = "kebab-case")]
pub enum BanAction {
    Created(String),
    Recreated(String),
    PendingConvert(String),
    NotADirectory(String),
}

impl BanAction {
    pub fn modified_live(&self) -> bool {
        matches!(self, BanAction::Created(_) | BanAction::Recreated(_))
    }
}

fn warn_once(st: &mut ProjectState, key: String, now: jiff::Timestamp, msg: impl FnOnce() -> String) {
    if let std::collections::btree_map::Entry::Vacant(e) = st.warned.entry(key) {
        tracing::warn!("{}", msg());
        e.insert(now);
    }
}

/// Cheap, every-tick enforcement. Non-empty plain directories are only marked for conversion.
pub fn enforce_cheap(
    ctx: &Ctx,
    live: &Path,
    eff: &EffectiveConfig,
    st: &mut ProjectState,
    owner: (u32, u32),
    allow_create: bool,
) -> Result<Vec<BanAction>> {
    let now = ctx.now();
    let mut out = Vec::new();
    st.pending_convert.retain(|k, _| eff.banlist.contains(k));
    for rel in &eff.banlist {
        let full = live.join(rel);
        match std::fs::symlink_metadata(&full) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                st.pending_convert.remove(rel);
                let wanted = match eff.policy.banlist_precreate {
                    Precreate::All => true,
                    Precreate::None => false,
                    Precreate::Primary => eff.primary_banlist.contains(rel) || st.banlist_seen.contains(rel),
                };
                let parent_ok = full.parent().is_some_and(|p| p.is_dir());
                if wanted && allow_create && parent_ok {
                    ctx.btrfs.create_subvolume(&full)?;
                    if !ctx.opts.dry_run {
                        let parent_md = std::fs::metadata(full.parent().unwrap())?;
                        use std::os::unix::fs::MetadataExt;
                        set_owner_mode(&full, owner.0, owner.1, parent_md.mode() & 0o777)?;
                    }
                    st.banlist_seen.insert(rel.clone());
                    out.push(BanAction::Created(rel.clone()));
                }
            }
            Err(e) => return Err(e).with_context(|| format!("stat {}", full.display())),
            Ok(md) if md.file_type().is_symlink() || !md.is_dir() => {
                warn_once(st, format!("banlist-not-dir:{rel}"), now, || {
                    format!("{}: banned path {rel} is not a directory; left alone", eff.name)
                });
                out.push(BanAction::NotADirectory(rel.clone()));
            }
            Ok(md) => {
                st.banlist_seen.insert(rel.clone());
                if ctx.btrfs.is_subvolume(&full)? {
                    st.pending_convert.remove(rel);
                    continue;
                }
                if dir_is_empty(&full)? {
                    if ctx.opts.dry_run {
                        out.push(BanAction::Recreated(rel.clone()));
                        continue;
                    }
                    match std::fs::remove_dir(&full) {
                        Ok(()) => {
                            ctx.btrfs.create_subvolume(&full)?;
                            copy_owner_mode(&md, &full)?;
                            st.pending_convert.remove(rel);
                            out.push(BanAction::Recreated(rel.clone()));
                        }
                        Err(_) => {
                            st.pending_convert.entry(rel.clone()).or_insert(now);
                            out.push(BanAction::PendingConvert(rel.clone()));
                        }
                    }
                } else {
                    st.pending_convert.entry(rel.clone()).or_insert(now);
                    out.push(BanAction::PendingConvert(rel.clone()));
                }
            }
        }
    }
    Ok(out)
}

/// Is a pending conversion safe to run now?
pub fn ready_to_convert(ctx: &Ctx, live: &Path, rel: &str, eff: &EffectiveConfig) -> bool {
    let full = live.join(rel);
    let probe = |p: &Path, ino: u64| ctx.is_subvol(p, ino);
    full.is_dir()
        && crate::util::walk::is_quiet(&full, &probe, eff.policy.banlist_settle, 5000)
        && proc::open_under(&full).is_empty()
}

/// Heavy: turn a non-empty plain banned directory into a nested subvolume.
pub fn convert(ctx: &Ctx, live: &Path, rel: &str, keep_contents: bool, force: bool) -> Result<()> {
    let full = live.join(rel);
    let tmp = sibling(&full, ".bpm-tmp");
    let old = sibling(&full, ".bpm-old");
    for leftover in [&tmp, &old] {
        if leftover.symlink_metadata().is_ok() {
            bail!("leftover {} from an interrupted conversion; run `bpm doctor --fix`", leftover.display());
        }
    }
    let md = std::fs::symlink_metadata(&full).with_context(|| format!("stat {}", full.display()))?;
    if !md.is_dir() || md.file_type().is_symlink() {
        bail!("{} is not a directory", full.display());
    }
    if ctx.btrfs.is_subvolume(&full)? {
        return Ok(());
    }
    let writers = proc::writers_under(&full);
    if !writers.is_empty() && !force {
        return Err(refused(format!("files open for writing under {}: {}", full.display(), proc::describe(&writers))));
    }
    if ctx.opts.dry_run {
        tracing::info!("[dry-run] convert {} into a nested subvolume", full.display());
        return Ok(());
    }
    ctx.btrfs.create_subvolume(&tmp)?;
    copy_owner_mode(&md, &tmp)?;
    if keep_contents {
        if let Err(e) = cp_a(&full, &tmp, ctx.reflink) {
            let _ = ctx.btrfs.delete_subvolume(&tmp, true);
            return Err(e);
        }
    }
    let _ = super::xattr::copy_times(&full, &tmp);
    rename_exchange(&full, &tmp)?;
    std::fs::rename(&tmp, &old)?;
    std::fs::remove_dir_all(&old).with_context(|| format!("remove {}", old.display()))?;
    tracing::info!("converted {} into a nested subvolume", full.display());
    Ok(())
}
