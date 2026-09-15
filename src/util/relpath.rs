//! Relative paths from configuration and the command line (banlist entries, restore paths).
//!
//! A `RelPath` is lexically safe (no `..`, not absolute), but a directory on disk may still be a
//! symlink that points out of the project. bpm runs as root, so every path that is about to be
//! created, replaced or deleted goes through `under`, which refuses a symlink or non-directory
//! in any existing intermediate component. The check and the use are not atomic; the threat
//! model is accidents (a directory replaced by a link to shared data), not a racing attacker.

use crate::error::refused;
use anyhow::{Context, Result};
use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RelPath(String);

impl RelPath {
    /// `target/`, `web/node_modules`; `None` for absolute paths, `..`, `.` or empty strings.
    pub fn parse(s: &str) -> Option<RelPath> {
        super::fs::safe_relative(s).map(|p| RelPath(p.to_string_lossy().into_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// For comparisons (`starts_with`, exclusion lists). Not for building paths to modify.
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub fn is_top_level(&self) -> bool {
        !self.0.contains('/')
    }

    /// `base/self` for reading only (walks, status): may resolve through symlinks.
    pub fn under_lexical(&self, base: &Path) -> PathBuf {
        base.join(&self.0)
    }

    /// `base/self` for creating, replacing or deleting. Refuses when an existing component
    /// before the last is a symlink or not a directory. The last component itself is not
    /// followed by the callers (they `lstat` it).
    pub fn under(&self, base: &Path) -> Result<PathBuf> {
        let parts: Vec<&str> = self.0.split('/').collect();
        let mut cur = base.to_path_buf();
        for part in &parts[..parts.len() - 1] {
            cur.push(part);
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(refused(format!("{} is a symlink; bpm does not follow it", cur.display())));
                }
                Ok(m) if !m.is_dir() => return Err(refused(format!("{} is not a directory", cur.display()))),
                Ok(_) => {}
                // nothing further down exists yet, so nothing can resolve elsewhere
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(e).with_context(|| format!("stat {}", cur.display())),
            }
        }
        Ok(base.join(&self.0))
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl PartialEq<str> for RelPath {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for RelPath {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for RelPath {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_resolve() {
        assert_eq!(RelPath::parse("web/node_modules/").unwrap(), "web/node_modules");
        assert!(RelPath::parse("../x").is_none() && RelPath::parse("/x").is_none());
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("proj");
        std::fs::create_dir_all(base.join("real")).unwrap();
        std::fs::create_dir_all(d.path().join("outside/x")).unwrap();
        std::os::unix::fs::symlink("../outside", base.join("link")).unwrap();
        std::fs::write(base.join("file"), "").unwrap();
        let ok = |s: &str| RelPath::parse(s).unwrap().under(&base);
        assert_eq!(ok("real/x").unwrap(), base.join("real/x"));
        assert_eq!(ok("missing/deeper/x").unwrap(), base.join("missing/deeper/x"));
        assert_eq!(ok("link").unwrap(), base.join("link"), "last component is not checked");
        assert!(ok("link/x").is_err(), "symlinked parent refused");
        assert!(ok("file/x").is_err(), "file as parent refused");
    }
}
