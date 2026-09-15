//! The btrfs backend boundary. Every side effect on subvolumes goes through `Btrfs`.

pub mod fake;
pub mod ioctl;
pub mod real;

use anyhow::Result;
use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Uuid(pub [u8; 16]);

impl Uuid {
    pub const NIL: Uuid = Uuid([0; 16]);
    pub fn is_nil(&self) -> bool {
        self.0 == [0; 16]
    }
    pub fn short(&self) -> String {
        self.to_string()[..8].to_string()
    }
    pub fn random() -> Uuid {
        let mut b = [0u8; 16];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut b);
        }
        Uuid(b)
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let h: String = self.0.iter().map(|b| format!("{b:02x}")).collect();
        write!(f, "{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Uuid {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex: String = s.chars().filter(|c| *c != '-').collect();
        if hex.len() != 32 {
            return Err(format!("invalid uuid {s:?}"));
        }
        let mut b = [0u8; 16];
        for (i, byte) in b.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| format!("invalid uuid {s:?}"))?;
        }
        Ok(Uuid(b))
    }
}

impl Serialize for Uuid {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Uuid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SubvolInfo {
    pub id: u64,
    pub name: String,
    pub parent_id: u64,
    pub generation: u64,
    pub flags: u64,
    pub uuid: Uuid,
    pub parent_uuid: Option<Uuid>,
    pub received_uuid: Option<Uuid>,
    /// Transaction of the last content change (inherited by snapshots; not bumped by snapshotting).
    pub ctransid: u64,
    /// Transaction the subvolume was created in.
    pub otransid: u64,
    pub ctime: Option<Timestamp>,
    pub otime: Option<Timestamp>,
}

impl SubvolInfo {
    pub fn readonly(&self) -> bool {
        self.flags & 1 != 0
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct DuStats {
    pub total: u64,
    pub exclusive: u64,
    pub set_shared: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct FsUsage {
    pub total: u64,
    pub free: u64,
}

#[derive(Clone, Debug)]
pub struct DefragOpts {
    pub compress: String,
    pub level: Option<u8>,
    pub flush: bool,
}

pub trait Btrfs: Send + Sync {
    fn kind(&self) -> &'static str;

    // ---- unprivileged reads ----
    fn subvol_info(&self, path: &Path) -> Result<SubvolInfo>;
    /// Is `path` itself the root of a subvolume?
    fn is_subvolume(&self, path: &Path) -> Result<bool>;
    /// Cheap probe used during walks: `ino` comes from the directory entry.
    fn probe_subvolume(&self, path: &Path, ino: u64) -> bool {
        let _ = path;
        ino == 256
    }
    fn statfs(&self, path: &Path) -> Result<FsUsage>;
    /// Commit the filesystem transaction so counters reflect everything written so far.
    fn sync(&self, path: &Path) -> Result<()>;

    // ---- privileged mutations ----
    fn create_subvolume(&self, path: &Path) -> Result<()>;
    fn snapshot(&self, src: &Path, dst: &Path, readonly: bool) -> Result<()>;
    fn delete_subvolume(&self, path: &Path, recursive: bool) -> Result<()>;
    fn set_readonly(&self, path: &Path, ro: bool) -> Result<()>;
    fn defragment(&self, path: &Path, opts: &DefragOpts) -> Result<()>;
    fn du(&self, path: &Path) -> Result<DuStats>;
    fn send(&self, snapshot: &Path, compressed_data: bool, sink: &mut dyn Write) -> Result<u64>;
    fn receive(&self, source: &mut dyn Read, dest_dir: &Path) -> Result<()>;
    /// Locate a subvolume by uuid anywhere on the filesystem containing `fs_path`.
    fn find_by_uuid(&self, fs_path: &Path, uuid: Uuid) -> Result<Option<PathBuf>>;
}

/// Reads delegate; mutations are logged and skipped.
pub struct DryRunBtrfs(pub Arc<dyn Btrfs>);

macro_rules! dry {
    ($($arg:tt)*) => {{
        tracing::info!("[dry-run] {}", format!($($arg)*));
        Ok(())
    }};
}

impl Btrfs for DryRunBtrfs {
    fn kind(&self) -> &'static str {
        "dry-run"
    }
    fn subvol_info(&self, path: &Path) -> Result<SubvolInfo> {
        self.0.subvol_info(path)
    }
    fn is_subvolume(&self, path: &Path) -> Result<bool> {
        self.0.is_subvolume(path)
    }
    fn probe_subvolume(&self, path: &Path, ino: u64) -> bool {
        self.0.probe_subvolume(path, ino)
    }
    fn statfs(&self, path: &Path) -> Result<FsUsage> {
        self.0.statfs(path)
    }
    fn sync(&self, path: &Path) -> Result<()> {
        self.0.sync(path)
    }
    fn create_subvolume(&self, path: &Path) -> Result<()> {
        dry!("btrfs subvolume create {}", path.display())
    }
    fn snapshot(&self, src: &Path, dst: &Path, readonly: bool) -> Result<()> {
        dry!("btrfs subvolume snapshot {}{} {}", if readonly { "-r " } else { "" }, src.display(), dst.display())
    }
    fn delete_subvolume(&self, path: &Path, recursive: bool) -> Result<()> {
        dry!("btrfs subvolume delete {}{}", if recursive { "-R " } else { "" }, path.display())
    }
    fn set_readonly(&self, path: &Path, ro: bool) -> Result<()> {
        dry!("btrfs property set {} ro {ro}", path.display())
    }
    fn defragment(&self, path: &Path, opts: &DefragOpts) -> Result<()> {
        dry!("btrfs filesystem defragment -r -c{} {:?} {}", opts.compress, opts.level, path.display())
    }
    fn du(&self, path: &Path) -> Result<DuStats> {
        self.0.du(path)
    }
    fn send(&self, snapshot: &Path, _c: bool, _sink: &mut dyn Write) -> Result<u64> {
        tracing::info!("[dry-run] btrfs send {}", snapshot.display());
        Ok(0)
    }
    fn receive(&self, _source: &mut dyn Read, dest_dir: &Path) -> Result<()> {
        dry!("btrfs receive {}", dest_dir.display())
    }
    fn find_by_uuid(&self, fs_path: &Path, uuid: Uuid) -> Result<Option<PathBuf>> {
        self.0.find_by_uuid(fs_path, uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uuid_roundtrip() {
        let u: Uuid = "0bef9140-c16b-c14b-ad73-bf3dbed73949".parse().unwrap();
        assert_eq!(u.to_string(), "0bef9140-c16b-c14b-ad73-bf3dbed73949");
        assert_eq!(u.short(), "0bef9140");
        assert!(!u.is_nil());
        assert!(Uuid::NIL.is_nil());
    }
}
