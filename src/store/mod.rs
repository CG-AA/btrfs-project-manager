//! The snapshot store: `<root>/.bpm/{projects/<name>, container}` with TOML metadata.

pub mod journal;
pub mod lock;
pub mod meta;
pub mod snapid;

pub use meta::*;

use crate::util::fs::write_atomic;
use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const CONTAINER: &str = "@container";

#[derive(Clone, Debug)]
pub struct Store {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct Unit {
    pub name: String,
    pub dir: PathBuf,
    pub dry_run: bool,
}

/// What a scan of a unit directory found.
#[derive(Debug, Default)]
pub struct Scan {
    pub snapshots: Vec<SnapshotMeta>,
    /// `<id>/snapshot` exists but `meta.toml` is missing or unreadable.
    pub without_meta: Vec<(u64, PathBuf)>,
    /// `<id>/meta.toml` exists but the snapshot subvolume is gone.
    pub without_snapshot: Vec<u64>,
    /// Leftover `<id>.tmp` directories.
    pub tmp_dirs: Vec<PathBuf>,
}

fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(toml::from_str(&t).with_context(|| format!("parse {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn write_toml<T: Serialize>(path: &Path, value: &T, dry_run: bool) -> Result<()> {
    if dry_run {
        tracing::debug!("[dry-run] write {}", path.display());
        return Ok(());
    }
    let text = toml::to_string_pretty(value)?;
    write_atomic(path, text.as_bytes(), 0o644)
}

impl Store {
    pub fn new(root: &Path, store_dir: &str, dry_run: bool) -> Store {
        Store { root: root.to_path_buf(), dir: root.join(store_dir), dry_run }
    }

    pub fn info_path(&self) -> PathBuf {
        self.dir.join("store.toml")
    }

    pub fn exists(&self) -> bool {
        self.info_path().exists()
    }

    pub fn read_info(&self) -> Result<Option<StoreInfo>> {
        read_toml(&self.info_path())
    }

    pub fn write_info(&self, info: &StoreInfo) -> Result<()> {
        write_toml(&self.info_path(), info, self.dry_run)
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.dir.join("projects")
    }

    pub fn unit(&self, name: &str) -> Unit {
        if name == CONTAINER {
            return self.container();
        }
        Unit { name: name.to_string(), dir: self.projects_dir().join(name), dry_run: self.dry_run }
    }

    pub fn container(&self) -> Unit {
        Unit { name: CONTAINER.to_string(), dir: self.dir.join("container"), dry_run: self.dry_run }
    }

    pub fn lock(&self, timeout: Duration) -> Result<lock::Lock> {
        lock::Lock::acquire(&self.dir.join("lock"), timeout, "store")
    }

    /// Serialises adopt, convert, collapse and recompress on this root; never waits.
    pub fn heavy_lock(&self) -> Result<lock::Lock> {
        lock::Lock::acquire(&self.dir.join("heavy.lock"), Duration::ZERO, "heavy operation")
    }

    /// Project units that have a `project.toml`, sorted by name.
    pub fn units(&self) -> Result<Vec<(Unit, ProjectRecord)>> {
        let mut out = Vec::new();
        let dir = self.projects_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let unit = self.unit(&name);
            if let Some(rec) = unit.read_record()? {
                out.push((unit, rec));
            }
        }
        out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        Ok(out)
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.dir.join("tmp")
    }
}

impl Unit {
    pub fn record_path(&self) -> PathBuf {
        self.dir.join("project.toml")
    }
    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.toml")
    }
    pub fn frozen_marker(&self) -> PathBuf {
        self.dir.join("FROZEN")
    }
    pub fn snapshot_dir(&self, id: u64) -> PathBuf {
        self.dir.join(id.to_string())
    }
    pub fn snapshot_path(&self, id: u64) -> PathBuf {
        self.snapshot_dir(id).join("snapshot")
    }
    pub fn meta_path(&self, id: u64) -> PathBuf {
        self.snapshot_dir(id).join("meta.toml")
    }
    pub fn is_container(&self) -> bool {
        self.name == CONTAINER
    }

    pub fn ensure_dir(&self) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        fs::create_dir_all(&self.dir).with_context(|| format!("create {}", self.dir.display()))
    }

    pub fn read_record(&self) -> Result<Option<ProjectRecord>> {
        read_toml(&self.record_path())
    }

    pub fn read_state(&self) -> Result<ProjectState> {
        Ok(read_toml(&self.state_path())?.unwrap_or_default())
    }

    /// Metadata of a snapshot being created in its own reserved `<id>.tmp` directory. The only
    /// write allowed without the unit lock: nobody else can see that directory yet.
    pub fn write_new_meta(&self, tmp: &Path, meta: &SnapshotMeta) -> Result<()> {
        write_toml(&tmp.join("meta.toml"), meta, self.dry_run)
    }

    pub fn scan(&self) -> Result<Scan> {
        let mut scan = Scan::default();
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(scan),
            Err(e) => return Err(e).with_context(|| format!("read {}", self.dir.display())),
        };
        for e in entries.flatten() {
            let fname = e.file_name().to_string_lossy().into_owned();
            if fname.ends_with(".tmp") && fname.trim_end_matches(".tmp").parse::<u64>().is_ok() {
                scan.tmp_dirs.push(e.path());
                continue;
            }
            let Ok(id) = fname.parse::<u64>() else {
                continue;
            };
            let snap = self.snapshot_path(id);
            let has_snap = snap.symlink_metadata().is_ok();
            match read_toml::<SnapshotMeta>(&self.meta_path(id)) {
                Ok(Some(m)) if has_snap => scan.snapshots.push(m),
                Ok(Some(_)) => scan.without_snapshot.push(id),
                _ if has_snap => scan.without_meta.push((id, snap)),
                _ => {}
            }
        }
        scan.snapshots.sort_by_key(|m| m.id);
        scan.without_meta.sort();
        Ok(scan)
    }

    pub fn snapshots(&self) -> Result<Vec<SnapshotMeta>> {
        Ok(self.scan()?.snapshots)
    }

    /// Next id: one more than any id present (snapshots, metas, tmp dirs).
    pub fn next_id(&self) -> Result<u64> {
        let mut max = 0;
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().trim_end_matches(".tmp").to_string();
                if let Ok(id) = n.parse::<u64>() {
                    max = max.max(id);
                }
            }
        }
        Ok(max + 1)
    }

    /// Every change to the unit's record, state or existing snapshot metadata goes through the
    /// returned guard.
    pub fn lock(&self, timeout: Duration) -> Result<LockedUnit> {
        self.ensure_dir()?;
        let lock = lock::Lock::acquire(&self.dir.join("lock"), timeout, &self.name)?;
        Ok(LockedUnit { unit: self.clone(), _lock: lock })
    }
}

/// A unit whose lock this process holds.
pub struct LockedUnit {
    unit: Unit,
    _lock: lock::Lock,
}

impl std::ops::Deref for LockedUnit {
    type Target = Unit;
    fn deref(&self) -> &Unit {
        &self.unit
    }
}

impl LockedUnit {
    pub fn write_record(&self, rec: &ProjectRecord) -> Result<()> {
        self.ensure_dir()?;
        write_toml(&self.record_path(), rec, self.dry_run)
    }

    pub fn write_state(&self, st: &ProjectState) -> Result<()> {
        self.ensure_dir()?;
        write_toml(&self.state_path(), st, self.dry_run)?;
        if self.dry_run {
            return Ok(());
        }
        match &st.frozen {
            Some(f) => write_atomic(&self.frozen_marker(), f.reasons.join("\n").as_bytes(), 0o644)?,
            None => {
                let _ = fs::remove_file(self.frozen_marker());
            }
        }
        Ok(())
    }

    pub fn update_meta(&self, meta: &SnapshotMeta) -> Result<()> {
        write_toml(&self.meta_path(meta.id), meta, self.dry_run)
    }

    /// Metadata for an existing snapshot directory that has none (recovery).
    pub fn write_meta_in(&self, dir: &Path, meta: &SnapshotMeta) -> Result<()> {
        write_toml(&dir.join("meta.toml"), meta, self.dry_run)
    }

    /// Rename the unit directory. The lock file moves with it, so the lock stays held.
    pub fn rename_to(self, target: Unit) -> Result<LockedUnit> {
        if !self.dry_run {
            fs::rename(&self.unit.dir, &target.dir)
                .with_context(|| format!("rename {} -> {}", self.unit.dir.display(), target.dir.display()))?;
        }
        Ok(LockedUnit { unit: target, _lock: self._lock })
    }
}

pub fn newest(snaps: &[SnapshotMeta]) -> Option<&SnapshotMeta> {
    snaps.iter().max_by_key(|m| (m.snapshot_otransid, m.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs::Uuid;

    #[test]
    fn scan_and_ids() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path(), ".bpm", false);
        let unit = store.unit("demo");
        unit.ensure_dir().unwrap();
        for id in [1u64, 2, 5] {
            fs::create_dir_all(unit.snapshot_path(id)).unwrap();
            let m = SnapshotMeta::sample(id, "demo");
            unit.write_new_meta(&unit.snapshot_dir(id), &m).unwrap();
        }
        fs::create_dir_all(unit.snapshot_path(7)).unwrap();
        fs::create_dir_all(unit.snapshot_dir(8)).unwrap();
        unit.write_new_meta(&unit.snapshot_dir(8), &SnapshotMeta::sample(8, "demo")).unwrap();
        fs::create_dir_all(unit.dir.join("9.tmp")).unwrap();
        let scan = unit.scan().unwrap();
        assert_eq!(scan.snapshots.iter().map(|m| m.id).collect::<Vec<_>>(), vec![1, 2, 5]);
        assert_eq!(scan.without_meta.iter().map(|x| x.0).collect::<Vec<_>>(), vec![7]);
        assert_eq!(scan.without_snapshot, vec![8]);
        assert_eq!(scan.tmp_dirs.len(), 1);
        assert_eq!(unit.next_id().unwrap(), 10);

        let rec = ProjectRecord::new(
            "demo",
            Path::new("/space/demo"),
            Uuid([1; 16]),
            1000,
            1000,
            "2026-01-01T00:00:00Z".parse().unwrap(),
        );
        let unit = unit.lock(Duration::ZERO).unwrap();
        unit.write_record(&rec).unwrap();
        let mut st = ProjectState {
            frozen: Some(Frozen {
                since: "2026-01-02T00:00:00Z".parse().unwrap(),
                reasons: vec!["files -50%".into()],
                trigger_snap: Some(5),
                ref_snap: Some(2),
            }),
            ..Default::default()
        };
        unit.write_state(&st).unwrap();
        assert!(unit.frozen_marker().exists());
        assert_eq!(store.units().unwrap().len(), 1);
        assert_eq!(unit.read_state().unwrap().frozen.unwrap().ref_snap, Some(2));
        st.frozen = None;
        unit.write_state(&st).unwrap();
        assert!(!unit.frozen_marker().exists());
    }
}
