//! Step journal for multi-step operations, so `doctor` can tell where a crash happened.

use crate::util::fs::write_atomic;
use anyhow::Result;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JournalEntry {
    pub op: String,
    pub step: u32,
    pub started: Timestamp,
    pub updated: Timestamp,
    #[serde(default)]
    pub data: BTreeMap<String, String>,
}

pub struct Journal {
    path: PathBuf,
    entry: JournalEntry,
    dry_run: bool,
}

impl Journal {
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join("op.journal")
    }

    pub fn read(dir: &Path) -> Option<JournalEntry> {
        std::fs::read_to_string(Self::path_in(dir)).ok().and_then(|t| toml::from_str(&t).ok())
    }

    pub fn begin(dir: &Path, op: &str, data: &[(&str, String)], dry_run: bool) -> Result<Journal> {
        let now = Timestamp::now();
        let j = Journal {
            path: Self::path_in(dir),
            entry: JournalEntry {
                op: op.into(),
                step: 0,
                started: now,
                updated: now,
                data: data.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            },
            dry_run,
        };
        j.flush()?;
        Ok(j)
    }

    pub fn step(&mut self, n: u32) -> Result<()> {
        self.entry.step = n;
        self.entry.updated = Timestamp::now();
        self.flush()
    }

    pub fn set(&mut self, key: &str, value: String) -> Result<()> {
        self.entry.data.insert(key.into(), value);
        self.flush()
    }

    fn flush(&self) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_atomic(&self.path, toml::to_string(&self.entry)?.as_bytes(), 0o644)
    }

    pub fn finish(self) -> Result<()> {
        if !self.dry_run {
            let _ = std::fs::remove_file(&self.path);
        }
        Ok(())
    }
}
