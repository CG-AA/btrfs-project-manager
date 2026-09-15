//! Layered merge of policy tables with per-key origin tracking.

use super::schema::*;
use super::{BUILTIN_POLICY_TOML, builtin_profiles};
use crate::util::fs::safe_relative;
use crate::util::relpath::RelPath;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Policy tables `<project>/.bpm.toml` may not set: the file is writable by whatever works in the
/// repository (an AI agent included), and these decide whether deletions are noticed and how
/// long history is kept. Set them in the admin config (`[projects."name".policy]`).
pub const PROTECTED_FROM_PROJECT_FILE: &[&str] = &["shrink_guard", "thin"];

pub const POLICY_KEYS: &[&str] = &[
    "banlist_precreate",
    "banlist_settle",
    "keep_build_on_adopt",
    "snapshot",
    "thin",
    "lifecycle",
    "recompress",
    "shrink_guard",
];

#[derive(Clone, Debug, Serialize)]
pub struct EffectiveConfig {
    pub name: String,
    pub path: PathBuf,
    pub managed: bool,
    pub profiles: Vec<String>,
    pub banlist: Vec<RelPath>,
    /// Banlist entries created ahead of time under `banlist_precreate = "primary"`.
    pub primary_banlist: Vec<RelPath>,
    pub sentinels: Vec<String>,
    pub policy: Policy,
    pub origins: BTreeMap<String, String>,
    /// `.bpm.toml` policy tables that were ignored (see `PROTECTED_FROM_PROJECT_FILE`).
    pub ignored_project_keys: Vec<String>,
}

impl EffectiveConfig {
    pub fn banlist_paths(&self) -> Vec<PathBuf> {
        self.banlist.iter().map(|b| b.as_path().to_path_buf()).collect()
    }
    pub fn is_banned(&self, rel: &str) -> bool {
        self.banlist.iter().any(|b| b == rel)
    }
    /// Paths excluded from stats walks: banned dirs (even when still plain) and guard excludes.
    pub fn stats_exclude(&self) -> Vec<PathBuf> {
        let mut v = self.banlist_paths();
        v.extend(self.policy.shrink_guard.exclude.iter().filter_map(|s| safe_relative(s)));
        v
    }
}

fn record_leaves(prefix: &str, v: &toml::Value, origin: &str, origins: &mut BTreeMap<String, String>) {
    match v {
        toml::Value::Table(t) => {
            for (k, v) in t {
                record_leaves(&format!("{prefix}{k}."), v, origin, origins);
            }
        }
        _ => {
            origins.insert(prefix.trim_end_matches('.').to_string(), origin.to_string());
        }
    }
}

pub fn deep_merge(
    base: &mut toml::Table,
    overlay: &toml::Table,
    origin: &str,
    prefix: &str,
    origins: &mut BTreeMap<String, String>,
) {
    for (k, v) in overlay {
        let key = format!("{prefix}{k}");
        match (base.get_mut(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => {
                deep_merge(b, o, origin, &format!("{key}."), origins);
            }
            _ => {
                base.insert(k.clone(), v.clone());
                record_leaves(&format!("{key}."), v, origin, origins);
            }
        }
    }
}

fn policy_part(t: &toml::Table) -> toml::Table {
    t.iter().filter(|(k, _)| POLICY_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn str_list(t: &toml::Table, key: &str) -> Vec<String> {
    t.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

pub fn read_project_file(project: &Path) -> Result<Option<ProjectOverride>> {
    let p = project.join(".bpm.toml");
    match std::fs::read_to_string(&p) {
        Ok(t) => Ok(Some(toml::from_str(&t).with_context(|| format!("parse {}", p.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

/// All profiles (built-in, replaced or extended by config) in detection order.
pub fn all_profiles(cfg: &Config) -> Vec<(String, ProfileCfg)> {
    let mut map = builtin_profiles();
    for (k, v) in &cfg.profiles {
        map.insert(k.clone(), v.clone());
    }
    let mut ordered: Vec<(String, ProfileCfg)> = Vec::new();
    for name in &cfg.global.profile_order {
        if let Some(p) = map.remove(name) {
            ordered.push((name.clone(), p));
        }
    }
    ordered.extend(map);
    ordered
}

pub fn detect_profiles(cfg: &Config, path: &Path) -> Vec<String> {
    let found: Vec<String> = all_profiles(cfg)
        .into_iter()
        .filter(|(_, p)| p.markers.iter().any(|m| path.join(m).exists()))
        .map(|(n, _)| n)
        .collect();
    if found.is_empty() { vec!["generic".to_string()] } else { found }
}

pub fn effective_for(
    cfg: &Config,
    root: &RootCfg,
    name: &str,
    path: &Path,
    file: Option<&ProjectOverride>,
) -> Result<EffectiveConfig> {
    let mut origins = BTreeMap::new();
    let mut table: toml::Table = toml::from_str(BUILTIN_POLICY_TOML).expect("builtin policy parses");
    for (k, v) in &table {
        record_leaves(&format!("{k}."), v, "builtin", &mut origins);
    }
    deep_merge(&mut table, &policy_part(&cfg.defaults), "defaults", "", &mut origins);

    let global_over = cfg.projects.get(name);
    let file = if cfg.global.project_config { file } else { None };

    let selection = file
        .and_then(|f| f.profile.clone())
        .or_else(|| global_over.and_then(|o| o.profile.clone()))
        .or_else(|| cfg.defaults.get("profile").and_then(|v| v.clone().try_into::<ProfileSel>().ok()));
    let profiles = selection.and_then(|s| s.names()).unwrap_or_else(|| detect_profiles(cfg, path));
    let profile_map: BTreeMap<String, ProfileCfg> = all_profiles(cfg).into_iter().collect();

    let mut banlist: Vec<String> = str_list(&cfg.defaults, "banlist");
    let mut primary: Vec<String> = Vec::new();
    for pname in &profiles {
        let p = profile_map.get(pname).with_context(|| format!("project {name}: unknown profile {pname:?}"))?;
        banlist.extend(p.banlist.iter().cloned());
        primary.extend(p.banlist.first().cloned());
        deep_merge(&mut table, &p.policy, &format!("profiles.{pname}"), "", &mut origins);
    }
    banlist.extend(root.banlist_add.iter().cloned());
    deep_merge(&mut table, &root.policy, &format!("root({})", root.path.display()), "", &mut origins);

    let mut remove: Vec<String> = Vec::new();
    let mut sentinels_add = str_list(&cfg.defaults, "sentinels_add");
    let mut ignored_project_keys = Vec::new();
    for (layer, origin) in [(global_over, format!("projects.{name}")), (file, ".bpm.toml".to_string())] {
        if let Some(o) = layer {
            banlist.extend(o.banlist_add.iter().cloned());
            primary.extend(o.banlist_add.iter().cloned());
            remove.extend(o.banlist_remove.iter().cloned());
            sentinels_add.extend(o.sentinels_add.iter().cloned());
            let mut policy = o.policy.clone();
            if origin == ".bpm.toml" {
                for key in PROTECTED_FROM_PROJECT_FILE {
                    if policy.remove(*key).is_some() {
                        ignored_project_keys.push(format!("policy.{key}"));
                    }
                }
            }
            deep_merge(&mut table, &policy, &origin, "", &mut origins);
        }
    }

    let policy: Policy =
        toml::Value::Table(table).try_into().with_context(|| format!("project {name}: invalid policy"))?;

    let norm = |s: &String| RelPath::parse(s);
    let removed: Vec<RelPath> = remove.iter().filter_map(norm).collect();
    let mut seen = std::collections::BTreeSet::new();
    let banlist: Vec<RelPath> =
        banlist.iter().filter_map(norm).filter(|b| !removed.contains(b)).filter(|b| seen.insert(b.clone())).collect();
    let mut sentinels: Vec<String> = policy.shrink_guard.sentinels.clone();
    for s in sentinels_add.iter().filter_map(norm) {
        if !sentinels.iter().any(|x| s == *x) {
            sentinels.push(s.to_string());
        }
    }
    let primary_banlist: Vec<RelPath> = primary.iter().filter_map(norm).filter(|p| banlist.contains(p)).collect();
    let managed = file.and_then(|f| f.managed).or_else(|| global_over.and_then(|o| o.managed)).unwrap_or(true);
    Ok(EffectiveConfig {
        name: name.into(),
        path: path.into(),
        managed,
        profiles,
        banlist,
        primary_banlist,
        sentinels,
        policy,
        origins,
        ignored_project_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;
    use std::time::Duration;

    const CFG: &str = r#"
version = 1
[[root]]
path = "/space"
banlist_add = ["tmp"]
[root.policy.thin]
keep_all = "12h"
[defaults]
banlist = [".cache"]
[defaults.snapshot]
min_interval = "10m"
[profiles.rust]
markers = ["Cargo.toml"]
banlist = ["target"]
[profiles.rust.policy.lifecycle]
cold_after = "90d"
[projects.demo]
banlist_add = ["build"]
banlist_remove = [".cache"]
[projects.demo.policy.recompress]
level = 15
"#;

    #[test]
    fn precedence_banlist_and_origins() {
        let cfg = parse(CFG, "t").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let file: ProjectOverride = toml::from_str(
            "banlist_add = ['out/']\nsentinels_add = ['docs']\n[policy.snapshot]\nmin_interval = '1m'\n",
        )
        .unwrap();
        let e = effective_for(&cfg, &cfg.roots[0], "demo", dir.path(), Some(&file)).unwrap();
        assert_eq!(e.profiles, vec!["rust"]);
        assert_eq!(e.banlist, vec!["target", "tmp", "build", "out"]);
        assert_eq!(e.primary_banlist, vec!["target", "build", "out"]);
        assert_eq!(e.sentinels, vec![".git", "docs"]);
        assert_eq!(e.policy.snapshot.min_interval, Duration::from_secs(60));
        assert_eq!(e.origins["snapshot.min_interval"], ".bpm.toml");
        assert_eq!(e.policy.thin.keep_all, Duration::from_secs(12 * 3600));
        assert_eq!(e.origins["thin.keep_all"], "root(/space)");
        assert_eq!(e.policy.lifecycle.cold_after, Duration::from_secs(90 * 86400));
        assert_eq!(e.origins["lifecycle.cold_after"], "profiles.rust");
        assert_eq!(e.policy.recompress.level, 15);
        assert_eq!(e.origins["recompress.level"], "projects.demo");
        assert_eq!(e.origins["thin.hourly"], "builtin");
        assert!(e.managed);
        assert!(e.ignored_project_keys.is_empty());
    }

    #[test]
    fn project_file_cannot_weaken_the_guard_or_retention() {
        let cfg = parse(CFG, "t").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file: ProjectOverride = toml::from_str(
            "[policy.shrink_guard]\nenabled = false\n[policy.thin]\nkeep_all = '0s'\n[policy.lifecycle]\ndormant_after = '30d'\n",
        )
        .unwrap();
        let e = effective_for(&cfg, &cfg.roots[0], "demo", dir.path(), Some(&file)).unwrap();
        assert!(e.policy.shrink_guard.enabled);
        assert_eq!(e.policy.thin.keep_all, Duration::from_secs(12 * 3600));
        assert_eq!(e.policy.lifecycle.dormant_after, Duration::from_secs(30 * 86400), "other tables still apply");
        assert_eq!(e.ignored_project_keys, vec!["policy.shrink_guard", "policy.thin"]);
    }

    #[test]
    fn detection_defaults_to_generic_and_project_config_can_be_disabled() {
        let mut cfg = parse(CFG, "t").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file: ProjectOverride = toml::from_str("managed = false").unwrap();
        let e = effective_for(&cfg, &cfg.roots[0], "other", dir.path(), Some(&file)).unwrap();
        assert_eq!(e.profiles, vec!["generic"]);
        assert!(!e.managed);
        cfg.global.project_config = false;
        let e = effective_for(&cfg, &cfg.roots[0], "other", dir.path(), Some(&file)).unwrap();
        assert!(e.managed);
    }

    #[test]
    fn typo_in_policy_is_an_error() {
        let cfg = parse("version=1\n[[root]]\npath='/x'\n[projects.p.policy.thin]\nkeepall='1h'\n", "t").unwrap();
        let err = effective_for(&cfg, &cfg.roots[0], "p", Path::new("/nonexistent"), None).unwrap_err();
        assert!(format!("{err:#}").contains("keepall"), "{err:#}");
    }
}
