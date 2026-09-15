//! Serde types for config files and the fully merged per-project policy.

use crate::util::bytes::ByteSize;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

fn one() -> u32 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub global: GlobalCfg,
    #[serde(default, rename = "root")]
    pub roots: Vec<RootCfg>,
    /// Policy layer plus `profile`/`banlist`; validated when merged.
    #[serde(default)]
    pub defaults: toml::Table,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileCfg>,
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectOverride>,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported config version {}", self.version);
        }
        self.global.destructive_regexes()?;
        for r in &self.roots {
            if !r.path.is_absolute() {
                bail!("root path must be absolute: {}", r.path.display());
            }
        }
        for key in self.defaults.keys() {
            if !matches!(key.as_str(), "profile" | "banlist" | "sentinels_add")
                && !merge::POLICY_KEYS.contains(&key.as_str())
            {
                bail!("unknown key in [defaults]: {key}");
            }
        }
        if !matches!(self.global.project_hooks.as_str(), "off" | "allowlist" | "on") {
            bail!("global.project_hooks must be off, allowlist or on");
        }
        Ok(())
    }

    pub fn root_for(&self, path: &std::path::Path) -> Option<&RootCfg> {
        self.roots.iter().find(|r| r.path == path)
    }
}

use super::merge;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct GlobalCfg {
    pub store_dir: String,
    #[serde(with = "crate::util::time::dur")]
    pub lock_timeout: Duration,
    pub sudo: bool,
    pub hooks_dir: PathBuf,
    #[serde(with = "crate::util::time::dur")]
    pub hook_timeout: Duration,
    pub project_hooks: String,
    pub project_hooks_allow: Vec<String>,
    pub project_config: bool,
    pub archive_dir: PathBuf,
    pub archive_zstd_level: u8,
    pub heavy_min_free: ByteSize,
    pub profile_order: Vec<String>,
    #[serde(with = "crate::util::time::dur")]
    pub hook_throttle: Duration,
    pub destructive_patterns: Vec<String>,
}

impl Default for GlobalCfg {
    fn default() -> Self {
        GlobalCfg {
            store_dir: ".bpm".into(),
            lock_timeout: Duration::from_secs(30),
            sudo: true,
            hooks_dir: "/etc/bpm/hooks.d".into(),
            hook_timeout: Duration::from_secs(60),
            project_hooks: "off".into(),
            project_hooks_allow: vec![],
            project_config: true,
            archive_dir: "/archives/bpm".into(),
            archive_zstd_level: 19,
            heavy_min_free: ByteSize(10 << 30),
            profile_order: ["generic", "python", "node", "cmake", "rust"].map(String::from).to_vec(),
            hook_throttle: Duration::from_secs(120),
            destructive_patterns: vec![
                r"\brm\s+(-[a-zA-Z]*[rRf]|--recursive|--force)".into(),
                r"\bgit\s+clean\b".into(),
                r"\bgit\s+reset\s+--hard".into(),
            ],
        }
    }
}

impl GlobalCfg {
    pub fn destructive_regexes(&self) -> Result<Vec<regex::Regex>> {
        self.destructive_patterns
            .iter()
            .map(|p| regex::Regex::new(p).map_err(|e| anyhow::anyhow!("destructive_patterns {p:?}: {e}")))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AdoptMode {
    Auto,
    Manual,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootCfg {
    pub path: PathBuf,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default = "default_adopt")]
    pub adopt: AdoptMode,
    #[serde(default = "default_adopt_min_age", with = "crate::util::time::dur")]
    pub adopt_min_age: Duration,
    #[serde(default)]
    pub banlist_add: Vec<String>,
    #[serde(default)]
    pub container: ContainerCfg,
    #[serde(default)]
    pub policy: toml::Table,
}

fn default_adopt() -> AdoptMode {
    AdoptMode::Manual
}
fn default_adopt_min_age() -> Duration {
    Duration::from_secs(15 * 60)
}

impl RootCfg {
    pub fn adhoc(path: PathBuf) -> RootCfg {
        RootCfg {
            path,
            ignore: vec![],
            adopt: AdoptMode::Manual,
            adopt_min_age: default_adopt_min_age(),
            banlist_add: vec![],
            container: ContainerCfg::default(),
            policy: toml::Table::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ContainerCfg {
    pub enabled: bool,
    #[serde(with = "crate::util::time::dur")]
    pub interval: Duration,
    #[serde(with = "crate::util::time::dur")]
    pub keep_all: Duration,
    #[serde(with = "crate::util::time::dur")]
    pub daily: Duration,
    #[serde(with = "crate::util::time::dur")]
    pub weekly: Duration,
}

impl Default for ContainerCfg {
    fn default() -> Self {
        ContainerCfg {
            enabled: true,
            interval: Duration::from_secs(6 * 3600),
            keep_all: Duration::from_secs(24 * 3600),
            daily: Duration::from_secs(7 * 86400),
            weekly: Duration::from_secs(28 * 86400),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProfileCfg {
    #[serde(default)]
    pub markers: Vec<String>,
    #[serde(default)]
    pub banlist: Vec<String>,
    #[serde(default)]
    pub policy: toml::Table,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ProfileSel {
    One(String),
    Many(Vec<String>),
}

impl ProfileSel {
    /// None means auto-detect.
    pub fn names(&self) -> Option<Vec<String>> {
        match self {
            ProfileSel::One(s) if s == "auto" => None,
            ProfileSel::One(s) => Some(vec![s.clone()]),
            ProfileSel::Many(v) => Some(v.clone()),
        }
    }
}

/// `[projects."name"]` in the global file and the whole of `<project>/.bpm.toml`.
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProjectOverride {
    pub managed: Option<bool>,
    pub profile: Option<ProfileSel>,
    #[serde(default)]
    pub banlist_add: Vec<String>,
    #[serde(default)]
    pub banlist_remove: Vec<String>,
    #[serde(default)]
    pub sentinels_add: Vec<String>,
    #[serde(default)]
    pub policy: toml::Table,
}

// ---------------- merged policy ----------------

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub banlist_precreate: Precreate,
    #[serde(with = "crate::util::time::dur")]
    pub banlist_settle: Duration,
    pub keep_build_on_adopt: bool,
    pub snapshot: SnapshotPolicy,
    pub thin: ThinPolicy,
    pub lifecycle: LifecyclePolicy,
    pub recompress: RecompressPolicy,
    pub shrink_guard: ShrinkPolicy,
}

/// Which missing banlist directories get created as empty nested subvolumes ahead of time.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Precreate {
    /// The main build dir of each detected profile, explicit project additions, and any dir seen before.
    Primary,
    All,
    None,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotPolicy {
    #[serde(with = "crate::util::time::dur")]
    pub min_interval: Duration,
    pub stats: bool,
    #[serde(with = "crate::util::time::dur")]
    pub stats_budget: Duration,
    #[serde(with = "crate::util::time::dur")]
    pub stats_min_interval: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ThinPolicy {
    #[serde(with = "crate::util::time::dur")]
    pub keep_all: Duration,
    #[serde(with = "crate::util::time::dur")]
    pub hourly: Duration,
    #[serde(with = "crate::util::time::dur")]
    pub daily: Duration,
    #[serde(with = "crate::util::time::dur")]
    pub weekly: Duration,
    #[serde(with = "crate::util::time::dur")]
    pub safety_ttl: Duration,
    pub min_free: ByteSize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LifecyclePolicy {
    #[serde(with = "crate::util::time::dur")]
    pub dormant_after: Duration,
    #[serde(with = "crate::util::time::dur")]
    pub cold_after: Duration,
    #[serde(with = "crate::util::time::dur")]
    pub adopt_grace: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RecompressPolicy {
    pub enabled: bool,
    pub level: u8,
    pub max_bytes: ByteSize,
    pub min_free: ByteSize,
    pub min_expected_gain: f64,
    pub nested: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ShrinkPolicy {
    pub enabled: bool,
    pub ratio: f64,
    pub min_files: u64,
    pub min_bytes: ByteSize,
    pub catastrophic: f64,
    pub exclude: Vec<String>,
    pub sentinels: Vec<String>,
    pub thin_after_freeze: bool,
}
