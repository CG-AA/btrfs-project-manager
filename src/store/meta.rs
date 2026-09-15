//! Metadata schemas: store info, project identity, mutable state, per-snapshot meta.

use crate::btrfs::Uuid;
use crate::util::walk::TreeStats;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreInfo {
    pub format: u32,
    pub root: PathBuf,
    pub root_uuid: Uuid,
    pub created: Timestamp,
    pub bpm_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub format: u32,
    pub name: String,
    pub path: PathBuf,
    pub uuid: Uuid,
    #[serde(default)]
    pub uuid_history: Vec<Uuid>,
    pub owner_uid: u32,
    pub owner_gid: u32,
    pub adopted: Timestamp,
}

impl ProjectRecord {
    pub fn new(name: &str, path: &Path, uuid: Uuid, uid: u32, gid: u32, adopted: Timestamp) -> Self {
        ProjectRecord {
            format: 1,
            name: name.into(),
            path: path.into(),
            uuid,
            uuid_history: vec![],
            owner_uid: uid,
            owner_gid: gid,
            adopted,
        }
    }
    /// Snapshots whose parent uuid is the current or any previous live subvolume belong to us.
    pub fn owns(&self, parent: Option<Uuid>) -> bool {
        parent.is_some_and(|p| p == self.uuid || self.uuid_history.contains(&p))
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    #[default]
    Active,
    Dormant,
    Cold,
    Orphaned,
    Archived,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Stage::Active => "active",
            Stage::Dormant => "dormant",
            Stage::Cold => "cold",
            Stage::Orphaned => "orphaned",
            Stage::Archived => "archived",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Frozen {
    pub since: Timestamp,
    pub reasons: Vec<String>,
    pub trigger_snap: Option<u64>,
    pub ref_snap: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct RefStats {
    pub files: u64,
    pub files_snap: u64,
    pub bytes: u64,
    pub bytes_snap: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RecompressRecord {
    pub at: Timestamp,
    pub level: u8,
    pub ctransid: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    #[serde(default)]
    pub skipped: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ErrorRecord {
    pub at: Timestamp,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ArchiveRecord {
    pub file: PathBuf,
    pub sha256: String,
    pub at: Timestamp,
    pub snapshot: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ProjectState {
    pub stage: Stage,
    pub stage_since: Option<Timestamp>,
    pub last_change_at: Option<Timestamp>,
    pub snap_ctransid: u64,
    pub tool_ctransid: u64,
    pub last_snapshot_at: Option<Timestamp>,
    pub last_stats_at: Option<Timestamp>,
    pub ref_stats: Option<RefStats>,
    pub frozen: Option<Frozen>,
    pub recompress: Option<RecompressRecord>,
    pub missing_since: Option<Timestamp>,
    pub last_tick: Option<Timestamp>,
    pub last_error: Option<ErrorRecord>,
    pub pending_convert: BTreeMap<String, Timestamp>,
    /// Banlist paths that have existed at some point (recreated after `cargo clean` & co).
    pub banlist_seen: std::collections::BTreeSet<String>,
    pub warned: BTreeMap<String, Timestamp>,
    pub archives: Vec<ArchiveRecord>,
}

impl ProjectState {
    pub fn set_stage(&mut self, stage: Stage, now: Timestamp) -> Option<(Stage, Stage)> {
        if self.stage == stage {
            return None;
        }
        let from = self.stage;
        self.stage = stage;
        self.stage_since = Some(now);
        Some((from, stage))
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotKind {
    Auto,
    Hook,
    Manual,
    Pre,
    Post,
    Collapse,
    Rollback,
    PreRestore,
    Adopt,
    Import,
    Received,
    Recovered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KindClass {
    /// Thinned by retention windows and removed by collapse.
    Thinnable,
    /// Protected for `thin.safety_ttl`, then treated as thinnable.
    Safety,
    /// Never deleted automatically.
    Keep,
}

impl SnapshotKind {
    pub fn class(self) -> KindClass {
        use SnapshotKind::*;
        match self {
            Auto | Hook | Collapse | Adopt => KindClass::Thinnable,
            Pre | Post | Rollback | PreRestore => KindClass::Safety,
            Manual | Import | Received | Recovered => KindClass::Keep,
        }
    }
    pub fn as_str(self) -> &'static str {
        use SnapshotKind::*;
        match self {
            Auto => "auto",
            Hook => "hook",
            Manual => "manual",
            Pre => "pre",
            Post => "post",
            Collapse => "collapse",
            Rollback => "rollback",
            PreRestore => "pre-restore",
            Adopt => "adopt",
            Import => "import",
            Received => "received",
            Recovered => "recovered",
        }
    }
}

impl fmt::Display for SnapshotKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SnapshotKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        toml::Value::String(s.to_string()).try_into().map_err(|_| format!("unknown snapshot kind {s:?}"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct Origin {
    pub bpm: String,
    pub user: String,
    pub argv: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SnapshotMeta {
    pub format: u32,
    pub id: u64,
    pub project: String,
    pub created: Timestamp,
    pub kind: SnapshotKind,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub pair: Option<u64>,
    #[serde(default)]
    pub hold: bool,
    #[serde(default)]
    pub hold_note: String,
    pub source_uuid: Uuid,
    pub source_ctransid: u64,
    pub snapshot_uuid: Uuid,
    pub snapshot_otransid: u64,
    #[serde(default)]
    pub received_uuid: Option<Uuid>,
    #[serde(default)]
    pub stats: Option<TreeStats>,
    #[serde(default)]
    pub origin: Origin,
}

impl SnapshotMeta {
    pub fn complete_stats(&self) -> Option<&TreeStats> {
        self.stats.as_ref().filter(|s| s.complete)
    }

    #[doc(hidden)]
    pub fn sample(id: u64, project: &str) -> SnapshotMeta {
        SnapshotMeta {
            format: 1,
            id,
            project: project.into(),
            created: Timestamp::from_second(1_789_000_000 + id as i64 * 60).unwrap(),
            kind: SnapshotKind::Auto,
            reason: String::new(),
            pair: None,
            hold: false,
            hold_note: String::new(),
            source_uuid: Uuid([1; 16]),
            source_ctransid: 100 + id,
            snapshot_uuid: Uuid([id as u8; 16]),
            snapshot_otransid: 200 + id,
            received_uuid: None,
            stats: None,
            origin: Origin::default(),
        }
    }
}
