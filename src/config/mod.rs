//! Configuration: global file, built-in profiles, per-project overrides, and merging.

pub mod merge;
pub mod schema;

pub use merge::{EffectiveConfig, effective_for};
pub use schema::*;

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/bpm/config.toml";
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../../assets/config.default.toml");
pub const BUILTIN_POLICY_TOML: &str = include_str!("../../assets/builtin-policy.toml");
pub const BUILTIN_PROFILES_TOML: &str = include_str!("../../assets/profiles.toml");

pub struct Loaded {
    pub config: Config,
    /// None when the embedded default was used because no file exists.
    pub path: Option<PathBuf>,
}

pub fn config_path(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("BPM_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

pub fn parse(text: &str, origin: &str) -> Result<Config> {
    let cfg: Config = toml::from_str(text).with_context(|| format!("parse {origin}"))?;
    cfg.validate().with_context(|| format!("validate {origin}"))?;
    Ok(cfg)
}

/// Load the config. A missing file at the default location falls back to the embedded default;
/// a missing explicitly-requested file is an error.
pub fn load(explicit: Option<&Path>) -> Result<Loaded> {
    let path = config_path(explicit);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Loaded { config: parse(&text, &path.display().to_string())?, path: Some(path) }),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && explicit.is_none()
                && std::env::var_os("BPM_CONFIG").is_none() =>
        {
            Ok(Loaded { config: parse(DEFAULT_CONFIG_TOML, "embedded default config")?, path: None })
        }
        Err(e) => bail!("read {}: {e}", path.display()),
    }
}

pub fn builtin_profiles() -> std::collections::BTreeMap<String, ProfileCfg> {
    toml::from_str(BUILTIN_PROFILES_TOML).expect("embedded profiles parse")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_files_parse_and_validate() {
        let cfg = parse(DEFAULT_CONFIG_TOML, "default").unwrap();
        assert_eq!(cfg.roots.len(), 1);
        assert_eq!(cfg.roots[0].path, PathBuf::from("/space"));
        assert!(cfg.global.destructive_regexes().unwrap().len() > 5);
        let p = builtin_profiles();
        assert_eq!(p["rust"].banlist, vec!["target"]);
        let _: Policy = toml::from_str(BUILTIN_POLICY_TOML).unwrap();
    }
    #[test]
    fn unknown_keys_rejected() {
        let e = parse("version = 1\n[global]\nstoer_dir = 'x'\n", "t").unwrap_err();
        assert!(format!("{e:#}").contains("stoer_dir"));
    }
}
