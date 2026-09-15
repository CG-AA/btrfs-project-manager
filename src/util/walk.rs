//! Tree walks that stop at nested subvolume boundaries: stats, indexes, quiet checks.

use anyhow::Result;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use walkdir::{DirEntry, WalkDir};

/// Decides whether a directory entry below the walk root is a nested subvolume root.
pub type SubvolProbe<'a> = &'a dyn Fn(&Path, u64) -> bool;

pub fn real_probe(_path: &Path, ino: u64) -> bool {
    ino == 256
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TreeStats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub others: u64,
    pub bytes: u64,
    /// Bytes counted once per path (hardlinks counted every time): comparable between a tree
    /// and a copy that does not preserve hardlinks.
    #[serde(default)]
    pub path_bytes: u64,
    pub newest_mtime: Option<Timestamp>,
    /// Latest mtime or ctime of any entry, plus the root's mtime, at nanosecond precision.
    /// `ctime` cannot be set by tools (`mv`, `cp -p`, `tar` and `rsync -a` all bump it).
    #[serde(default)]
    pub newest_change: Option<Timestamp>,
    #[serde(default)]
    pub sentinels: BTreeMap<String, bool>,
    pub complete: bool,
    pub walk_ms: u64,
}

fn rel<'a>(root: &Path, p: &'a Path) -> &'a Path {
    p.strip_prefix(root).unwrap_or(p)
}

/// Is this entry (below the walk root) the root of a nested subvolume?
///
/// Uses `lstat`'s inode, not the directory entry's `d_ino`: btrfs reports a subvolume's tree id
/// as `d_ino` in `readdir`, while `stat` reports 256 for every subvolume root.
pub fn entry_is_subvol(e: &DirEntry, probe: SubvolProbe) -> bool {
    e.depth() > 0 && e.file_type().is_dir() && e.metadata().map(|m| probe(e.path(), m.ino())).unwrap_or(false)
}

fn nested(e: &DirEntry, probe: SubvolProbe) -> bool {
    entry_is_subvol(e, probe)
}

/// Count entries under `root`, skipping nested subvolumes and `exclude` (relative paths).
/// Sentinels are recorded as present/absent even when they are excluded from counts.
pub fn tree_stats(
    root: &Path,
    probe: SubvolProbe,
    exclude: &[PathBuf],
    sentinels: &[String],
    budget: Duration,
) -> Result<TreeStats> {
    let start = Instant::now();
    let mut st = TreeStats { complete: true, ..Default::default() };
    let mut seen: HashSet<u64> = HashSet::new();
    let mut newest: i64 = i64::MIN;
    let mut newest_change: i128 = i128::MIN;
    if let Ok(md) = std::fs::symlink_metadata(root) {
        newest_change = nanos(md.mtime(), md.mtime_nsec());
    }
    let walker = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }
        let r = rel(root, e.path());
        !(exclude.iter().any(|x| x == r) || nested(e, probe))
    });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                if err.io_error().map(|e| e.kind() == std::io::ErrorKind::NotFound).unwrap_or(false) {
                    continue;
                }
                return Err(err.into());
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let ft = entry.file_type();
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        newest = newest.max(md.mtime());
        newest_change = newest_change.max(nanos(md.mtime(), md.mtime_nsec())).max(nanos(md.ctime(), md.ctime_nsec()));
        if ft.is_dir() {
            st.dirs += 1;
        } else if ft.is_symlink() {
            st.symlinks += 1;
        } else if ft.is_file() {
            st.files += 1;
            st.path_bytes += md.len();
            if md.nlink() <= 1 || seen.insert(md.ino()) {
                st.bytes += md.len();
            }
        } else {
            st.others += 1;
        }
        if (st.files + st.dirs) % 4096 == 0 && start.elapsed() > budget {
            st.complete = false;
            break;
        }
    }
    for s in sentinels {
        let exists = root.join(s).symlink_metadata().is_ok();
        st.sentinels.insert(s.clone(), exists);
    }
    if newest != i64::MIN {
        st.newest_mtime = Timestamp::from_second(newest).ok();
    }
    if newest_change != i128::MIN {
        st.newest_change = Timestamp::from_nanosecond(newest_change).ok();
    }
    st.walk_ms = start.elapsed().as_millis() as u64;
    Ok(st)
}

pub fn nanos(sec: i64, nsec: i64) -> i128 {
    sec as i128 * 1_000_000_000 + nsec as i128
}

/// Nested subvolume roots below `dir` (not descending into them). A walk error other than an
/// entry vanishing is an error: callers use the result to decide what is safe to delete.
pub fn nested_subvolumes(dir: &Path, probe: SubvolProbe) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut it = WalkDir::new(dir).follow_links(false).min_depth(1).into_iter();
    while let Some(entry) = it.next() {
        let e = match entry {
            Ok(e) => e,
            Err(err) if err.io_error().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => continue,
            Err(err) => return Err(anyhow::Error::from(err).context(format!("scan {}", dir.display()))),
        };
        if entry_is_subvol(&e, probe) {
            out.push(e.path().to_path_buf());
            it.skip_current_dir();
        }
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink(PathBuf),
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryInfo {
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ns: i128,
    pub mode: u32,
}

/// Map of relative path -> entry info, not crossing nested subvolumes.
pub fn tree_index(root: &Path, probe: SubvolProbe) -> Result<BTreeMap<PathBuf, EntryInfo>> {
    let mut map = BTreeMap::new();
    let mut it = WalkDir::new(root).follow_links(false).sort_by_file_name().min_depth(1).into_iter();
    while let Some(entry) = it.next() {
        let entry = entry?;
        let md = entry.metadata()?;
        let ft = entry.file_type();
        // one lstat per entry: the subvolume test reuses it instead of a `filter_entry` probe
        if ft.is_dir() && probe(entry.path(), md.ino()) {
            it.skip_current_dir();
            continue;
        }
        let kind = if ft.is_dir() {
            EntryKind::Dir
        } else if ft.is_symlink() {
            EntryKind::Symlink(std::fs::read_link(entry.path()).unwrap_or_default())
        } else if ft.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        let size = if ft.is_file() { md.len() } else { 0 };
        map.insert(
            rel(root, entry.path()).to_path_buf(),
            EntryInfo {
                kind,
                size,
                mtime_ns: md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128,
                mode: md.mode() & 0o7777,
            },
        );
    }
    Ok(map)
}

/// True when nothing under `dir` (first `max_entries`) was modified within `settle`.
pub fn is_quiet(dir: &Path, probe: SubvolProbe, settle: Duration, max_entries: usize) -> bool {
    let cutoff = std::time::SystemTime::now().checked_sub(settle).unwrap_or(std::time::UNIX_EPOCH);
    let walker = WalkDir::new(dir).follow_links(false).into_iter().filter_entry(|e| !nested(e, probe));
    for (i, entry) in walker.enumerate() {
        if i >= max_entries {
            break;
        }
        let Ok(entry) = entry else { continue };
        if let Ok(md) = entry.metadata() {
            if md.modified().map(|m| m > cutoff).unwrap_or(false) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn stats_exclude_and_sentinels() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("src")).unwrap();
        fs::write(r.join("src/a"), "hello").unwrap();
        fs::hard_link(r.join("src/a"), r.join("src/b")).unwrap();
        fs::create_dir_all(r.join(".git/objects")).unwrap();
        fs::write(r.join(".git/objects/x"), "123456").unwrap();
        fs::create_dir_all(r.join("target/debug")).unwrap();
        fs::write(r.join("target/debug/bin"), "0123456789").unwrap();
        let st = tree_stats(
            r,
            &real_probe,
            &[PathBuf::from(".git"), PathBuf::from("target")],
            &[".git".into(), "docs".into()],
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(st.files, 2);
        assert_eq!(st.bytes, 5, "hardlinks counted once");
        assert_eq!(st.path_bytes, 10, "path bytes count every link");
        assert!(st.newest_change.is_some());
        assert_eq!(st.dirs, 1);
        assert_eq!(st.sentinels.get(".git"), Some(&true));
        assert_eq!(st.sentinels.get("docs"), Some(&false));
        assert!(st.complete);
    }

    #[test]
    fn index_and_quiet() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("f"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", d.path().join("l")).unwrap();
        let idx = tree_index(d.path(), &real_probe).unwrap();
        assert_eq!(idx.len(), 2);
        assert!(matches!(idx[&PathBuf::from("l")].kind, EntryKind::Symlink(_)));
        assert!(!is_quiet(d.path(), &real_probe, Duration::from_secs(3600), 100));
        assert!(is_quiet(d.path(), &real_probe, Duration::ZERO, 100));
    }
}
