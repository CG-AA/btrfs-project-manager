//! In-memory model of btrfs subvolume semantics on an ordinary directory tree, for tests.
//!
//! Subvolumes are tracked by (dev, inode) of their root directory, so renames keep working.
//! Counters follow the kernel rules verified on real btrfs: content changes bump `ctransid`
//! and `generation`; snapshotting bumps only the source `generation`; snapshots inherit
//! `ctransid`; nested subvolumes appear as empty placeholder directories in snapshots.
//!
//! Like the kernel for buffered writes, counters move only when changes are flushed: on `sync`,
//! on `snapshot` (for the source) and on `defragment`. Reading counters without a flush sees
//! the state of the last flush, so code that forgets to sync fails tests.

use super::{Btrfs, DefragOpts, DuStats, FsUsage, SubvolInfo, Uuid};
use crate::util::walk::{self, EntryKind};
use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug)]
struct FakeSubvol {
    id: u64,
    name: String,
    uuid: Uuid,
    parent_uuid: Option<Uuid>,
    received_uuid: Option<Uuid>,
    generation: u64,
    ctransid: u64,
    otransid: u64,
    ro: bool,
    fingerprint: u64,
    ctime: Timestamp,
    otime: Timestamp,
    path: PathBuf,
}

struct FakeState {
    transid: u64,
    next_id: u64,
    subvols: HashMap<(u64, u64), FakeSubvol>,
    ops: Vec<String>,
    usage: FsUsage,
}

pub struct FakeBtrfs {
    st: Mutex<FakeState>,
    clock: Mutex<Option<std::sync::Arc<dyn crate::clock::Clock>>>,
}

impl Default for FakeBtrfs {
    fn default() -> Self {
        Self::new()
    }
}

fn key_of(path: &Path) -> Option<(u64, u64)> {
    let md = std::fs::symlink_metadata(path).ok()?;
    md.is_dir().then(|| (md.dev(), md.ino()))
}

impl FakeBtrfs {
    pub fn new() -> Self {
        FakeBtrfs {
            clock: Mutex::new(None),
            st: Mutex::new(FakeState {
                transid: 100,
                next_id: 256,
                subvols: HashMap::new(),
                ops: Vec::new(),
                usage: FsUsage { total: 400 << 30, free: 100 << 30 },
            }),
        }
    }

    /// Use this clock for subvolume ctime/otime instead of the wall clock.
    pub fn set_clock(&self, clock: std::sync::Arc<dyn crate::clock::Clock>) {
        *self.clock.lock().unwrap() = Some(clock);
    }

    fn now(&self) -> Timestamp {
        self.clock.lock().unwrap().as_ref().map(|c| c.now()).unwrap_or_else(Timestamp::now)
    }

    /// Treat an existing directory as a subvolume (e.g. the test root standing in for /space).
    pub fn register_existing(&self, path: &Path) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        Self::register(self.now(), &mut st, path, None, None, false)?;
        Ok(())
    }

    pub fn set_free(&self, free: u64) {
        self.st.lock().unwrap().usage.free = free;
    }

    pub fn ops(&self) -> Vec<String> {
        self.st.lock().unwrap().ops.clone()
    }

    pub fn clear_ops(&self) {
        self.st.lock().unwrap().ops.clear();
    }

    fn register(
        now: Timestamp,
        st: &mut FakeState,
        path: &Path,
        parent: Option<&FakeSubvol>,
        received: Option<Uuid>,
        ro: bool,
    ) -> Result<(u64, u64)> {
        let key = key_of(path).ok_or_else(|| anyhow!("fake: {} is not a directory", path.display()))?;
        st.transid += 1;
        st.next_id += 1;
        let nested = Self::nested_keys(st);
        let fp = fingerprint(path, &nested);
        let sv = FakeSubvol {
            id: st.next_id,
            name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            uuid: Uuid::random(),
            parent_uuid: parent.map(|p| p.uuid),
            received_uuid: received,
            generation: st.transid,
            ctransid: parent.map(|p| p.ctransid).unwrap_or(st.transid),
            otransid: st.transid,
            ro,
            fingerprint: fp,
            ctime: parent.map(|p| p.ctime).unwrap_or(now),
            otime: now,
            path: path.to_path_buf(),
        };
        st.subvols.insert(key, sv);
        Ok(key)
    }

    fn nested_keys(st: &FakeState) -> HashSet<(u64, u64)> {
        st.subvols.keys().copied().collect()
    }

    /// Nearest enclosing registered subvolume root of `path`.
    fn root_of(st: &FakeState, path: &Path) -> Result<(PathBuf, (u64, u64))> {
        let abs = std::fs::canonicalize(path).with_context(|| format!("fake: resolve {}", path.display()))?;
        for anc in abs.ancestors() {
            if let Some(k) = key_of(anc) {
                if st.subvols.contains_key(&k) {
                    return Ok((anc.to_path_buf(), k));
                }
            }
        }
        bail!("fake: {} is not inside a subvolume", path.display())
    }

    /// Recompute the content fingerprint; bump counters when it changed.
    fn observe(now: Timestamp, st: &mut FakeState, root: &Path, key: (u64, u64)) {
        let nested = Self::nested_keys(st);
        let fp = fingerprint(root, &nested);
        let sv = st.subvols.get(&key).unwrap();
        if sv.ro || sv.fingerprint == fp {
            return;
        }
        st.transid += 1;
        let t = st.transid;
        let sv = st.subvols.get_mut(&key).unwrap();
        sv.fingerprint = fp;
        sv.generation = t;
        sv.ctransid = t;
        sv.ctime = now;
    }

    fn info_of(sv: &FakeSubvol) -> SubvolInfo {
        SubvolInfo {
            id: sv.id,
            name: sv.name.clone(),
            parent_id: 5,
            generation: sv.generation,
            flags: sv.ro as u64,
            uuid: sv.uuid,
            parent_uuid: sv.parent_uuid,
            received_uuid: sv.received_uuid,
            ctransid: sv.ctransid,
            otransid: sv.otransid,
            ctime: Some(sv.ctime),
            otime: Some(sv.otime),
        }
    }
}

fn fingerprint(root: &Path, subvols: &HashSet<(u64, u64)>) -> u64 {
    let probe = |p: &Path, _ino: u64| key_of(p).map(|k| subvols.contains(&k)).unwrap_or(false);
    let mut h = DefaultHasher::new();
    // the root's mtime is not in the index; a rename at the top level changes nothing else
    if let Ok(md) = std::fs::symlink_metadata(root) {
        walk::nanos(md.mtime(), md.mtime_nsec()).hash(&mut h);
    }
    if let Ok(idx) = walk::tree_index(root, &probe) {
        for (path, e) in idx {
            path.hash(&mut h);
            e.size.hash(&mut h);
            e.mtime_ns.hash(&mut h);
            e.mode.hash(&mut h);
            match e.kind {
                EntryKind::File => 1u8.hash(&mut h),
                EntryKind::Dir => 2u8.hash(&mut h),
                EntryKind::Symlink(t) => t.hash(&mut h),
                EntryKind::Other => 4u8.hash(&mut h),
            }
        }
    }
    h.finish()
}

/// Copy a tree, turning nested subvolume roots into empty placeholder directories.
fn copy_tree(src: &Path, dst: &Path, subvols: &HashSet<(u64, u64)>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dst)?;
    let probe = |p: &Path, _ino: u64| key_of(p).map(|k| subvols.contains(&k)).unwrap_or(false);
    let mut copied = Vec::new();
    for entry in walkdir::WalkDir::new(src).follow_links(false).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap();
        let target = dst.join(rel);
        if rel.components().count() > 1 {
            // skip anything below a placeholder
            let mut acc = src.to_path_buf();
            let mut inside_nested = false;
            for c in rel.parent().unwrap().components() {
                acc.push(c);
                if probe(&acc, 0) {
                    inside_nested = true;
                    break;
                }
            }
            if inside_nested {
                continue;
            }
        }
        let md = entry.metadata()?;
        let ft = entry.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&target)?;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(md.mode() & 0o7777))?;
            if probe(entry.path(), 0) {
                // a placeholder: the kernel gives it the time its inode is instantiated, not the
                // nested subvolume's times, so it differs between snapshots of the same state
                continue;
            }
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
        } else if ft.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
        copied.push((target, md));
    }
    // a snapshot keeps every timestamp: set them children first, since creating entries bumps
    // the parent directory's mtime
    for (target, md) in copied.iter().rev() {
        set_times(target, md)?;
    }
    set_times(dst, &std::fs::symlink_metadata(src)?)?;
    Ok(())
}

fn set_times(path: &Path, md: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let ts = [
        libc::timespec { tv_sec: md.atime(), tv_nsec: md.atime_nsec() },
        libc::timespec { tv_sec: md.mtime(), tv_nsec: md.mtime_nsec() },
    ];
    if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("fake: set times of {}", path.display()));
    }
    Ok(())
}

impl Btrfs for FakeBtrfs {
    fn kind(&self) -> &'static str {
        "fake"
    }

    fn subvol_info(&self, path: &Path) -> Result<SubvolInfo> {
        let st = self.st.lock().unwrap();
        let (_, key) = Self::root_of(&st, path)?;
        Ok(Self::info_of(&st.subvols[&key]))
    }

    fn is_subvolume(&self, path: &Path) -> Result<bool> {
        std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let st = self.st.lock().unwrap();
        Ok(key_of(path).map(|k| st.subvols.contains_key(&k)).unwrap_or(false))
    }

    fn probe_subvolume(&self, path: &Path, _ino: u64) -> bool {
        let st = self.st.lock().unwrap();
        key_of(path).map(|k| st.subvols.contains_key(&k)).unwrap_or(false)
    }

    fn statfs(&self, _path: &Path) -> Result<FsUsage> {
        Ok(self.st.lock().unwrap().usage)
    }

    fn sync(&self, path: &Path) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        let Ok(abs) = std::fs::canonicalize(path) else {
            return Ok(());
        };
        // syncfs flushes the whole filesystem: start from the outermost registered subvolume
        let Some(top) = abs.ancestors().filter(|a| key_of(a).is_some_and(|k| st.subvols.contains_key(&k))).last()
        else {
            return Ok(());
        };
        let top = top.to_path_buf();
        let now = self.now();
        let mut it = walkdir::WalkDir::new(&top).follow_links(false).into_iter();
        while let Some(Ok(e)) = it.next() {
            if !e.file_type().is_dir() {
                continue;
            }
            let Some(k) = key_of(e.path()) else { continue };
            let Some(sv) = st.subvols.get_mut(&k) else { continue };
            sv.path = e.path().to_path_buf();
            if sv.ro {
                it.skip_current_dir();
                continue;
            }
            Self::observe(now, &mut st, e.path(), k);
        }
        Ok(())
    }

    fn create_subvolume(&self, path: &Path) -> Result<()> {
        if path.exists() {
            bail!("fake: create {}: File exists", path.display());
        }
        std::fs::create_dir(path).with_context(|| format!("fake: create {}", path.display()))?;
        let mut st = self.st.lock().unwrap();
        st.ops.push(format!("create {}", path.display()));
        Self::register(self.now(), &mut st, path, None, None, false)?;
        Ok(())
    }

    fn snapshot(&self, src: &Path, dst: &Path, readonly: bool) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        if !st.subvols.contains_key(&key_of(src).unwrap_or((0, 0))) {
            bail!("fake: snapshot source {} is not a subvolume", src.display());
        }
        if dst.exists() {
            bail!("fake: snapshot target {} exists", dst.display());
        }
        let (root, skey) = Self::root_of(&st, src)?;
        Self::observe(self.now(), &mut st, &root, skey);
        let subvols = Self::nested_keys(&st);
        copy_tree(src, dst, &subvols)?;
        let parent = st.subvols[&skey].clone();
        let dkey = Self::register(self.now(), &mut st, dst, Some(&parent), None, readonly)?;
        let ot = st.subvols[&dkey].otransid;
        st.subvols.get_mut(&skey).unwrap().generation = ot;
        st.ops.push(format!("snapshot{} {} {}", if readonly { " -r" } else { "" }, src.display(), dst.display()));
        Ok(())
    }

    fn delete_subvolume(&self, path: &Path, recursive: bool) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        let key = key_of(path).ok_or_else(|| anyhow!("fake: delete {}: not found", path.display()))?;
        if !st.subvols.contains_key(&key) {
            bail!("fake: delete {}: not a subvolume", path.display());
        }
        let mut nested = Vec::new();
        for e in walkdir::WalkDir::new(path).follow_links(false).min_depth(1).into_iter().flatten() {
            if let Some(k) = key_of(e.path()) {
                if st.subvols.contains_key(&k) {
                    nested.push(k);
                }
            }
        }
        if !nested.is_empty() && !recursive {
            bail!("fake: delete {}: Directory not empty (nested subvolumes)", path.display());
        }
        std::fs::remove_dir_all(path)?;
        st.subvols.remove(&key);
        for k in nested {
            st.subvols.remove(&k);
        }
        st.ops.push(format!("delete{} {}", if recursive { " -R" } else { "" }, path.display()));
        Ok(())
    }

    fn set_readonly(&self, path: &Path, ro: bool) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        let key = key_of(path).context("fake: set ro: not found")?;
        let sv = st.subvols.get_mut(&key).context("fake: set ro: not a subvolume")?;
        sv.ro = ro;
        st.ops.push(format!("ro={ro} {}", path.display()));
        Ok(())
    }

    fn defragment(&self, path: &Path, opts: &DefragOpts) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        let (root, key) = Self::root_of(&st, path)?;
        Self::observe(self.now(), &mut st, &root, key);
        st.transid += 1;
        let t = st.transid;
        let sv = st.subvols.get_mut(&key).unwrap();
        sv.ctransid = t;
        sv.generation = t;
        st.ops.push(format!("defrag -c{} {:?} {}", opts.compress, opts.level, path.display()));
        Ok(())
    }

    fn du(&self, path: &Path) -> Result<DuStats> {
        let st = self.st.lock().unwrap();
        let subvols = Self::nested_keys(&st);
        drop(st);
        let probe = |p: &Path, _ino: u64| key_of(p).map(|k| subvols.contains(&k)).unwrap_or(false);
        let idx = walk::tree_index(path, &probe)?;
        let total = idx.values().map(|e| e.size).sum();
        Ok(DuStats { total, exclusive: 0, set_shared: total })
    }

    fn send(&self, snapshot: &Path, _compressed: bool, sink: &mut dyn Write) -> Result<u64> {
        let uuid = self.subvol_info(snapshot)?.uuid;
        let name = snapshot.file_name().unwrap().to_string_lossy().into_owned();
        let header = format!("FAKEBTRFS {name} {uuid}\n");
        sink.write_all(header.as_bytes())?;
        let out = std::process::Command::new("tar").arg("-C").arg(snapshot).args(["-cf", "-", "."]).output()?;
        if !out.status.success() {
            bail!("fake send: tar failed");
        }
        sink.write_all(&out.stdout)?;
        Ok((header.len() + out.stdout.len()) as u64)
    }

    fn receive(&self, source: &mut dyn Read, dest_dir: &Path) -> Result<()> {
        let mut r = BufReader::new(source);
        let mut header = String::new();
        r.read_line(&mut header)?;
        let mut parts = header.split_whitespace();
        if parts.next() != Some("FAKEBTRFS") {
            bail!("fake receive: bad stream");
        }
        let name = parts.next().context("name")?.to_string();
        let uuid: Uuid = parts.next().context("uuid")?.parse().map_err(|e: String| anyhow!(e))?;
        let target = dest_dir.join(&name);
        std::fs::create_dir(&target)?;
        let mut child = std::process::Command::new("tar")
            .arg("-C")
            .arg(&target)
            .args(["-xf", "-"])
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        std::io::copy(&mut r, child.stdin.as_mut().unwrap())?;
        drop(child.stdin.take());
        if !child.wait()?.success() {
            bail!("fake receive: tar failed");
        }
        let mut st = self.st.lock().unwrap();
        Self::register(self.now(), &mut st, &target, None, Some(uuid), true)?;
        st.ops.push(format!("receive {}", target.display()));
        Ok(())
    }

    fn find_by_uuid(&self, _fs_path: &Path, uuid: Uuid) -> Result<Option<PathBuf>> {
        let st = self.st.lock().unwrap();
        Ok(st
            .subvols
            .iter()
            .find(|(k, sv)| sv.uuid == uuid && key_of(&sv.path) == Some(**k))
            .map(|(_, sv)| sv.path.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn counters_follow_kernel_rules() {
        let d = tempfile::tempdir().unwrap();
        let fb = FakeBtrfs::new();
        fb.register_existing(d.path()).unwrap();
        let proj = d.path().join("proj");
        fb.create_subvolume(&proj).unwrap();
        fs::write(proj.join("a"), "1").unwrap();
        fb.create_subvolume(&proj.join("target")).unwrap();
        fs::write(proj.join("target/big"), "xxxx").unwrap();
        fb.sync(&proj).unwrap();
        let before = fb.subvol_info(&proj).unwrap();

        let snap = d.path().join("s1");
        fb.snapshot(&proj, &snap, true).unwrap();
        let s = fb.subvol_info(&snap).unwrap();
        let after = fb.subvol_info(&proj).unwrap();
        assert_eq!(s.ctransid, before.ctransid, "snapshot inherits ctransid");
        assert_eq!(after.ctransid, before.ctransid, "snapshotting does not bump ctransid");
        assert_eq!(after.generation, s.otransid, "source generation bumped to otransid");
        assert_eq!(s.parent_uuid, Some(before.uuid));
        assert!(s.readonly());
        assert!(snap.join("target").is_dir());
        assert!(!snap.join("target/big").exists(), "nested subvolume is a placeholder");

        fs::write(proj.join("target/big"), "yyyyyy").unwrap();
        fb.sync(&proj).unwrap();
        assert_eq!(fb.subvol_info(&proj).unwrap().ctransid, before.ctransid, "nested write invisible");
        fs::remove_file(proj.join("a")).unwrap();
        assert_eq!(fb.subvol_info(&proj).unwrap().ctransid, before.ctransid, "not flushed yet");
        fb.sync(d.path()).unwrap();
        assert!(fb.subvol_info(&proj).unwrap().ctransid > before.ctransid, "delete bumps ctransid once flushed");

        // rename keeps identity
        let moved = d.path().join("renamed");
        fs::rename(&proj, &moved).unwrap();
        assert!(fb.is_subvolume(&moved).unwrap());
        assert!(fb.delete_subvolume(&moved, false).is_err(), "nested subvolume blocks delete");
        fb.delete_subvolume(&moved, true).unwrap();
        assert!(!moved.exists());
    }

    #[test]
    fn send_receive_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let fb = FakeBtrfs::new();
        fb.register_existing(d.path()).unwrap();
        let p = d.path().join("p");
        fb.create_subvolume(&p).unwrap();
        fs::write(p.join("f"), "data").unwrap();
        fb.snapshot(&p, &d.path().join("snapshot"), true).unwrap();
        let mut buf = Vec::new();
        fb.send(&d.path().join("snapshot"), false, &mut buf).unwrap();
        let recv = d.path().join("recv");
        fs::create_dir(&recv).unwrap();
        fb.receive(&mut buf.as_slice(), &recv).unwrap();
        assert_eq!(fs::read_to_string(recv.join("snapshot/f")).unwrap(), "data");
        let sent = fb.subvol_info(&d.path().join("snapshot")).unwrap();
        assert_eq!(fb.subvol_info(&recv.join("snapshot")).unwrap().received_uuid, Some(sent.uuid));
    }
}
