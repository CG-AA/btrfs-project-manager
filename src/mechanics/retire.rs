//! Removing a user tree that bpm has replaced (the original directory after adopt or convert, the
//! pre-rollback subvolume, a file replaced by restore, the live project after archive).
//!
//! This is the only place such trees are deleted, and only with a `Proof` that nothing in them
//! is newer than what was kept. Without proof the tree is renamed to a `<name>.bpm-keep-<op>-<ts>`
//! leftover that `bpm doctor` reports and never deletes.

use crate::btrfs::SubvolInfo;
use crate::ctx::Ctx;
use crate::policy::change;
use crate::store::SnapshotMeta;
use crate::util::{proc, walk};
use anyhow::Result;
use jiff::Timestamp;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const KEEP_MARKER: &str = ".bpm-keep-";

/// What the old tree must match to be deleted.
#[derive(Clone, Debug)]
pub enum Expect<'a> {
    /// A directory (or file): nothing in it changed after `not_after`, and, when given, its
    /// counts equal those of the copy that was kept. `exclude` are relative paths skipped by both.
    Tree { stats: Option<walk::TreeStats>, not_after: Timestamp, exclude: Vec<PathBuf> },
    /// A non-directory bpm has just moved aside. Its ctime cannot be read back after the move
    /// (the rename bumps it), so `pre` are the facts read just before, and the file on disk must
    /// still be the one they describe.
    Replaced { pre: FileFacts, not_after: Timestamp },
    /// A subvolume: identical (by transaction counters) to the snapshot `kept` of it.
    Snapshot { kept: &'a SnapshotMeta },
    /// Content already verified: only check that nothing uses it and nothing unsaved is inside.
    Verified,
}

/// Identity and timestamps of a non-directory, read before bpm moves it aside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileFacts {
    ino: u64,
    size: u64,
    mtime_ns: i128,
    change_ns: i128,
}

impl FileFacts {
    pub fn of(md: &std::fs::Metadata) -> Self {
        let ns = |s: i64, n: i64| s as i128 * 1_000_000_000 + n as i128;
        FileFacts {
            ino: md.ino(),
            size: md.size(),
            mtime_ns: ns(md.mtime(), md.mtime_nsec()),
            change_ns: ns(md.mtime(), md.mtime_nsec()).max(ns(md.ctime(), md.ctime_nsec())),
        }
    }
}

#[derive(Debug)]
pub struct Proof {
    path: PathBuf,
    subvolume: bool,
    recursive: bool,
}

#[derive(Debug)]
pub struct Unproven(pub String);

impl std::fmt::Display for Unproven {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum Outcome {
    Deleted,
    Kept(PathBuf),
}

pub type Counts = (u64, u64, u64, u64, u64);

pub fn counts(s: &walk::TreeStats) -> Counts {
    (s.files, s.dirs, s.symlinks, s.others, s.path_bytes)
}

fn mounts_under(path: &Path) -> Vec<String> {
    let Ok(canon) = std::fs::canonicalize(path) else {
        return vec![];
    };
    let prefix = canon.to_string_lossy().into_owned();
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().nth(4).map(|m| m.replace("\\040", " ")))
        .filter(|m| m == &prefix || m.starts_with(&format!("{prefix}/")))
        .collect()
}

/// Check `path` against `expect`. `allowed_nested` are relative paths of nested subvolumes that
/// may be deleted with it (build directories the user chose to drop).
pub fn prove(ctx: &Ctx, path: &Path, expect: &Expect, allowed_nested: &[PathBuf]) -> Result<Proof, Unproven> {
    let md = std::fs::symlink_metadata(path).map_err(|e| Unproven(format!("stat: {e}")))?;
    let users = proc::open_under(path);
    if !users.is_empty() {
        return Err(Unproven(format!("in use by {}", proc::describe(&users))));
    }
    if let Some(m) = mounts_under(path).first() {
        return Err(Unproven(format!("{m} is mounted inside")));
    }
    let probe = |p: &Path, ino: u64| ctx.is_subvol(p, ino);
    let mut recursive = false;
    if md.is_dir() {
        let nested = walk::nested_subvolumes(path, &probe).map_err(|e| Unproven(format!("{e:#}")))?;
        let unexpected: Vec<String> = nested
            .iter()
            .filter_map(|n| n.strip_prefix(path).ok())
            .filter(|rel| !allowed_nested.iter().any(|a| a == rel))
            .map(|rel| rel.display().to_string())
            .collect();
        if !unexpected.is_empty() {
            return Err(Unproven(format!("contains nested subvolumes not saved anywhere: {}", unexpected.join(", "))));
        }
        recursive = !nested.is_empty();
    }
    let subvolume = md.is_dir() && ctx.btrfs.is_subvolume(path).unwrap_or(false);
    match expect {
        Expect::Verified => {}
        Expect::Replaced { pre, not_after } => {
            if md.is_dir() {
                return Err(Unproven("is a directory, but a file was moved aside".into()));
            }
            if Timestamp::from_nanosecond(pre.change_ns).is_ok_and(|c| c > *not_after) {
                return Err(Unproven(format!("modified after {not_after}")));
            }
            // the move must not have raced a writer: same inode, same size, same mtime
            let now = FileFacts::of(&md);
            if (now.ino, now.size, now.mtime_ns) != (pre.ino, pre.size, pre.mtime_ns) {
                return Err(Unproven("changed while it was being replaced".into()));
            }
        }
        Expect::Snapshot { kept } => {
            ctx.btrfs.sync(path).map_err(|e| Unproven(format!("sync: {e:#}")))?;
            let live: SubvolInfo = ctx.btrfs.subvol_info(path).map_err(|e| Unproven(format!("{e:#}")))?;
            if live.uuid != kept.source_uuid {
                return Err(Unproven(format!("is subvolume {}, not the source of snapshot #{}", live.uuid, kept.id)));
            }
            if !change::identical(&live, kept) {
                return Err(Unproven(format!("changed after snapshot #{} was taken", kept.id)));
            }
        }
        Expect::Tree { stats, not_after, exclude } => {
            if !md.is_dir() {
                let changed = Timestamp::from_nanosecond(
                    (md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128)
                        .max(md.ctime() as i128 * 1_000_000_000 + md.ctime_nsec() as i128),
                )
                .ok();
                if changed.is_some_and(|c| c > *not_after) {
                    return Err(Unproven(format!("modified after {not_after}")));
                }
            } else {
                let now = walk::tree_stats(path, &probe, exclude, &[], Duration::from_secs(24 * 3600))
                    .map_err(|e| Unproven(format!("{e:#}")))?;
                if now.newest_change.is_some_and(|c| c > *not_after) {
                    return Err(Unproven(format!(
                        "modified after the copy was made ({} > {not_after})",
                        now.newest_change.unwrap()
                    )));
                }
                if let Some(s) = stats {
                    if counts(&now) != counts(s) {
                        return Err(Unproven(format!(
                            "differs from the kept copy: {:?} vs {:?} (files, dirs, symlinks, others, bytes)",
                            counts(&now),
                            counts(s)
                        )));
                    }
                }
            }
        }
    }
    Ok(Proof { path: path.to_path_buf(), subvolume, recursive })
}

/// Delete a proven tree.
pub fn delete(ctx: &Ctx, proof: Proof) -> Result<()> {
    if proof.subvolume {
        ctx.btrfs.delete_subvolume(&proof.path, proof.recursive)
    } else if std::fs::symlink_metadata(&proof.path)?.is_dir() {
        if proof.recursive {
            for n in walk::nested_subvolumes(&proof.path, &|p, i| ctx.is_subvol(p, i))? {
                ctx.btrfs.delete_subvolume(&n, true)?;
            }
        }
        ctx.fs.remove_dir_all(&proof.path)
    } else {
        ctx.fs.remove_file(&proof.path)
    }
}

/// `<base>.bpm-keep-<op>-<timestamp>` next to `path`, where `base` drops bpm's own suffixes.
pub fn keep_name(ctx: &Ctx, path: &Path, op: &str) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let base = crate::project::Leftover::parse(&name).map(|l| l.base().to_string()).unwrap_or(name);
    let ts = ctx.now().strftime("%Y%m%dT%H%M%S").to_string();
    let mut candidate = path.with_file_name(format!("{base}{KEEP_MARKER}{op}-{ts}"));
    let mut n = 1;
    while candidate.symlink_metadata().is_ok() {
        candidate = path.with_file_name(format!("{base}{KEEP_MARKER}{op}-{ts}-{n}"));
        n += 1;
    }
    candidate
}

/// Delete `path` if it matches `expect`, otherwise keep it under a leftover name.
pub fn retire(ctx: &Ctx, path: &Path, expect: &Expect, allowed_nested: &[PathBuf], op: &str) -> Result<Outcome> {
    match prove(ctx, path, expect, allowed_nested) {
        Ok(proof) => {
            delete(ctx, proof)?;
            Ok(Outcome::Deleted)
        }
        Err(why) => {
            let keep = keep_name(ctx, path, op);
            ctx.fs.rename(path, &keep)?;
            tracing::error!(
                "{op}: kept {} instead of deleting it: {why}. It may hold changes that exist nowhere else; compare and merge by hand, then delete it",
                keep.display()
            );
            Ok(Outcome::Kept(keep))
        }
    }
}
