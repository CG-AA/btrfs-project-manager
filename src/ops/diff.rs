//! `bpm diff`: what changed between two snapshots, or between a snapshot and live.

use crate::cli::DiffArgs;
use crate::ctx::Ctx;
use crate::output::emit;
use crate::util::walk::{EntryInfo, EntryKind, tree_index};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Debug, PartialEq)]
pub struct Change {
    pub op: char,
    pub path: PathBuf,
    pub detail: String,
}

pub fn compare(a: &BTreeMap<PathBuf, EntryInfo>, b: &BTreeMap<PathBuf, EntryInfo>) -> Vec<Change> {
    let mut out = Vec::new();
    for (p, ea) in a {
        match b.get(p) {
            None => out.push(Change { op: 'D', path: p.clone(), detail: String::new() }),
            Some(eb) => {
                let mut d = Vec::new();
                if ea.kind != eb.kind {
                    d.push("type".to_string());
                } else {
                    if ea.kind == EntryKind::File && (ea.size != eb.size || ea.mtime_ns != eb.mtime_ns) {
                        d.push(format!("content {} -> {} bytes", ea.size, eb.size));
                    }
                    if ea.mode != eb.mode {
                        d.push(format!("mode {:o} -> {:o}", ea.mode, eb.mode));
                    }
                }
                if !d.is_empty() {
                    out.push(Change { op: 'M', path: p.clone(), detail: d.join(", ") });
                }
            }
        }
    }
    for p in b.keys() {
        if !a.contains_key(p) {
            out.push(Change { op: 'A', path: p.clone(), detail: String::new() });
        }
    }
    out.sort_by(|x, y| x.path.cmp(&y.path));
    out
}

pub fn run(ctx: &Ctx, a: DiffArgs) -> Result<()> {
    let pref = crate::project::resolve(ctx, &a.project)?;
    let eff = super::effective(ctx, &pref)?;
    let snaps = pref.unit.snapshots()?;
    let from = super::select(ctx, &snaps, &a.from)?;
    let to_label = a.to.clone().unwrap_or_else(|| "live".into());
    let to_path = if to_label == "live" {
        pref.path().to_path_buf()
    } else {
        pref.unit.snapshot_path(super::select(ctx, &snaps, &to_label)?.id)
    };
    let probe = |p: &Path, ino: u64| ctx.is_subvol(p, ino);
    let banned: Vec<PathBuf> = eff.banlist_paths();
    let filter = |m: BTreeMap<PathBuf, EntryInfo>| -> BTreeMap<PathBuf, EntryInfo> {
        m.into_iter()
            .filter(|(k, _)| !banned.iter().any(|b| k.starts_with(b)))
            .filter(|(k, _)| a.path.as_ref().is_none_or(|sub| k.starts_with(sub)))
            .collect()
    };
    let ia = filter(tree_index(&pref.unit.snapshot_path(from.id), &probe)?);
    let ib = filter(tree_index(&to_path, &probe)?);
    let changes = compare(&ia, &ib);
    emit(ctx, &changes, || {
        if a.stat {
            let count = |c: char| changes.iter().filter(|x| x.op == c).count();
            return format!(
                "#{} -> {to_label}: {} added, {} deleted, {} modified",
                from.id,
                count('A'),
                count('D'),
                count('M')
            );
        }
        let mut s = String::new();
        for c in &changes {
            if a.name_only {
                s.push_str(&format!("{}\n", c.path.display()));
            } else if c.detail.is_empty() {
                s.push_str(&format!("{} {}\n", c.op, c.path.display()));
            } else {
                s.push_str(&format!("{} {}  ({})\n", c.op, c.path.display(), c.detail));
            }
        }
        if s.is_empty() { format!("no differences between #{} and {to_label}", from.id) } else { s }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compare_detects_changes() {
        let f = |size, m| EntryInfo { kind: EntryKind::File, size, mtime_ns: m, mode: 0o644 };
        let mut a = BTreeMap::new();
        a.insert(PathBuf::from("same"), f(1, 1));
        a.insert(PathBuf::from("gone"), f(1, 1));
        a.insert(PathBuf::from("edited"), f(1, 1));
        let mut b = a.clone();
        b.remove(Path::new("gone"));
        b.insert(PathBuf::from("edited"), f(2, 2));
        b.insert(PathBuf::from("new"), f(3, 3));
        let c = compare(&a, &b);
        let ops: Vec<(char, &str)> = c.iter().map(|x| (x.op, x.path.to_str().unwrap())).collect();
        assert_eq!(ops, vec![('M', "edited"), ('D', "gone"), ('A', "new")]);
    }
}
