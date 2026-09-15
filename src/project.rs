//! Project discovery under a root, name/path resolution, effective config lookup.

use crate::btrfs::SubvolInfo;
use crate::config::{EffectiveConfig, RootCfg, effective_for, merge::read_project_file};
use crate::ctx::Ctx;
use crate::error::{not_found, usage};
use crate::store::{ProjectRecord, Store, Unit};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const LEFTOVER_MARKERS: &[&str] = &[".bpm-tmp", ".bpm-old", ".bpm-rollback-", ".bpm-keep-"];

pub fn is_leftover_name(name: &str) -> bool {
    LEFTOVER_MARKERS.iter().any(|m| name.contains(m))
}

/// A directory name left behind by an interrupted or unproven operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Leftover {
    /// `<base>.bpm-tmp`: a copy being built, or the original right after the swap.
    Tmp { base: String },
    /// `<base>.bpm-old`: the original after the swap, before cleanup.
    Old { base: String },
    /// `<base>.bpm-rollback-<ts>`: the pre-rollback subvolume.
    Rollback { base: String },
    /// `<base>.bpm-keep-<op>-<ts>`: kept because deleting it could lose data. Never auto-fixed.
    Keep { base: String, op: String },
}

impl Leftover {
    pub fn parse(name: &str) -> Option<Leftover> {
        if let Some(i) = name.find(".bpm-keep-") {
            let op = name[i + ".bpm-keep-".len()..].split('-').next().unwrap_or("").to_string();
            return Some(Leftover::Keep { base: name[..i].into(), op });
        }
        if let Some(i) = name.find(".bpm-rollback-") {
            return Some(Leftover::Rollback { base: name[..i].into() });
        }
        if let Some(b) = name.strip_suffix(".bpm-tmp") {
            return Some(Leftover::Tmp { base: b.into() });
        }
        name.strip_suffix(".bpm-old").map(|b| Leftover::Old { base: b.into() })
    }

    pub fn base(&self) -> &str {
        match self {
            Leftover::Tmp { base }
            | Leftover::Old { base }
            | Leftover::Rollback { base }
            | Leftover::Keep { base, .. } => base,
        }
    }
}

#[derive(Debug)]
pub struct Found {
    pub name: String,
    pub path: PathBuf,
    pub unit: Unit,
    pub record: ProjectRecord,
    pub info: SubvolInfo,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub managed: Vec<Found>,
    pub unadopted: Vec<Candidate>,
    pub foreign_subvols: Vec<Candidate>,
    pub ignored: Vec<String>,
    pub leftovers: Vec<PathBuf>,
    pub missing: Vec<(Unit, ProjectRecord)>,
}

pub fn discover(ctx: &Ctx, root: &RootCfg, store: &Store) -> Result<Discovery> {
    let mut d = Discovery::default();
    let mut records = store.units()?;
    let mut matched = vec![false; records.len()];
    let mut entries: Vec<_> =
        std::fs::read_dir(&root.path).with_context(|| format!("read {}", root.path.display()))?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if name.starts_with('.') {
            continue;
        }
        if is_leftover_name(&name) {
            d.leftovers.push(path);
            continue;
        }
        if root.ignore.iter().any(|i| i == &name) {
            d.ignored.push(name);
            continue;
        }
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !md.is_dir() || crate::util::fs::is_mount_point(&path) {
            d.ignored.push(name);
            continue;
        }
        if ctx.btrfs.is_subvolume(&path)? {
            let info = ctx.btrfs.subvol_info(&path)?;
            if let Some(i) = records.iter().position(|(_, r)| r.uuid == info.uuid) {
                matched[i] = true;
                let (unit, record) = records[i].clone();
                d.managed.push(Found { name, path, unit, record, info });
            } else {
                d.foreign_subvols.push(Candidate { name, path });
            }
        } else {
            d.unadopted.push(Candidate { name, path });
        }
    }
    for (i, rec) in records.drain(..).enumerate() {
        if !matched[i] {
            d.missing.push(rec);
        }
    }
    Ok(d)
}

/// A managed project resolved from a name or a path.
#[derive(Debug, Clone)]
pub struct ProjectRef {
    pub root: RootCfg,
    pub store: Store,
    pub unit: Unit,
    pub record: ProjectRecord,
}

impl ProjectRef {
    pub fn path(&self) -> &Path {
        &self.record.path
    }
    pub fn name(&self) -> &str {
        &self.unit.name
    }
}

/// Root and top-level directory name containing `path`, if any.
pub fn locate_path(ctx: &Ctx, path: &Path) -> Option<(RootCfg, String)> {
    let abs = std::fs::canonicalize(path).ok().or_else(|| {
        // the directory may have been deleted: fall back to a lexical absolute path
        std::path::absolute(path).ok()
    })?;
    ctx.roots().into_iter().find_map(|root| {
        let rel = abs.strip_prefix(&root.path).ok()?;
        let first = rel.components().next()?;
        let name = first.as_os_str().to_string_lossy().into_owned();
        (!name.starts_with('.')).then_some((root, name))
    })
}

pub fn resolve(ctx: &Ctx, spec: &str) -> Result<ProjectRef> {
    if spec.contains('/') || spec == "." || spec == ".." {
        let (root, name) = locate_path(ctx, Path::new(spec))
            .ok_or_else(|| usage(format!("{spec} is not inside a configured root")))?;
        return resolve_in(ctx, &root, &name);
    }
    let mut hits = Vec::new();
    for root in ctx.roots() {
        let store = ctx.store(&root);
        if store.unit(spec).read_record().ok().flatten().is_some() {
            hits.push(root);
        }
    }
    match hits.len() {
        0 => Err(not_found(format!("project {spec:?} is not managed (see `bpm status --unadopted`)"))),
        1 => resolve_in(ctx, &hits[0], spec),
        _ => Err(usage(format!("project {spec:?} exists in several roots; pass --root"))),
    }
}

pub fn resolve_in(ctx: &Ctx, root: &RootCfg, name: &str) -> Result<ProjectRef> {
    let store = ctx.store(root);
    let unit = store.unit(name);
    let record = unit
        .read_record()?
        .ok_or_else(|| not_found(format!("project {name:?} is not managed under {}", root.path.display())))?;
    Ok(ProjectRef { root: root.clone(), store, unit, record })
}

pub fn effective(ctx: &Ctx, root: &RootCfg, name: &str, path: &Path) -> Result<EffectiveConfig> {
    let file = if path.is_dir() {
        read_project_file(path).unwrap_or_else(|e| {
            tracing::warn!("{e:#}");
            None
        })
    } else {
        None
    };
    effective_for(&ctx.cfg, root, name, path, file.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn leftover_names() {
        assert_eq!(Leftover::parse("demo.bpm-old"), Some(Leftover::Old { base: "demo".into() }));
        assert_eq!(Leftover::parse("demo.bpm-tmp"), Some(Leftover::Tmp { base: "demo".into() }));
        assert_eq!(
            Leftover::parse("demo.bpm-rollback-20260915T101010"),
            Some(Leftover::Rollback { base: "demo".into() })
        );
        assert_eq!(
            Leftover::parse("demo.bpm-keep-adopt-20260915T101010"),
            Some(Leftover::Keep { base: "demo".into(), op: "adopt".into() })
        );
        assert_eq!(Leftover::parse("demo"), None);
        assert!(is_leftover_name("x.bpm-keep-restore-1"));
    }
}
