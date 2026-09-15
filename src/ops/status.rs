//! `bpm status`: all projects at a glance, or one in detail.

use crate::cli::StatusArgs;
use crate::ctx::Ctx;
use crate::output::{Table, emit};
use crate::project::{self, Discovery};
use crate::store::{Stage, newest};
use crate::util::bytes::fmt_bytes;
use crate::util::time::{age, fmt_age, fmt_local};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Serialize)]
struct ProjectRow {
    name: String,
    path: PathBuf,
    stage: String,
    idle: Option<String>,
    snapshots: usize,
    newest: Option<u64>,
    newest_age: Option<String>,
    frozen: Option<Vec<String>>,
    flags: Vec<String>,
    exclusive_bytes: Option<u64>,
}

#[derive(Serialize)]
struct RootReport {
    root: PathBuf,
    store: PathBuf,
    store_exists: bool,
    free_bytes: Option<u64>,
    projects: Vec<ProjectRow>,
    unadopted: Vec<String>,
    foreign_subvolumes: Vec<String>,
    ignored: Vec<String>,
    leftovers: Vec<PathBuf>,
    container_snapshots: usize,
}

pub fn run(ctx: &Ctx, a: StatusArgs) -> Result<()> {
    if let Some(p) = &a.project {
        return detail(ctx, p);
    }
    let now = ctx.now();
    let mut reports = Vec::new();
    for root in ctx.roots() {
        let store = ctx.store(&root);
        let disc = match project::discover(ctx, &root, &store) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("{}: {e:#}", root.path.display());
                Discovery::default()
            }
        };
        let mut rows = Vec::new();
        let mut add = |name: String, path: PathBuf, unit: &crate::store::Unit, missing: bool, renamed: bool| {
            let st = unit.read_state().unwrap_or_default();
            let snaps = unit.snapshots().unwrap_or_default();
            let n = newest(&snaps);
            let mut flags = Vec::new();
            if missing && st.stage != Stage::Archived {
                flags.push("MISSING".to_string());
            }
            if renamed {
                flags.push(format!("RENAMED(store:{})", unit.name));
            }
            if let Some(e) = &st.last_error {
                flags.push(format!("ERROR({})", e.message.chars().take(60).collect::<String>()));
            }
            for k in st.pending_convert.keys() {
                flags.push(format!("CONVERT({k})"));
            }
            if snaps.iter().any(|m| m.hold) {
                flags.push(format!("HELD({})", snaps.iter().filter(|m| m.hold).count()));
            }
            let exclusive = if a.du { n.and_then(|_| ctx.btrfs.du(&unit.dir).ok()).map(|d| d.exclusive) } else { None };
            rows.push(ProjectRow {
                name,
                path,
                stage: st.stage.to_string(),
                idle: st.last_change_at.map(|t| fmt_age(age(now, t))),
                snapshots: snaps.len(),
                newest: n.map(|m| m.id),
                newest_age: n.map(|m| fmt_age(age(now, m.created))),
                frozen: st.frozen.as_ref().map(|f| f.reasons.clone()),
                flags,
                exclusive_bytes: exclusive,
            });
        };
        for f in &disc.managed {
            add(f.name.clone(), f.path.clone(), &f.unit, false, f.name != f.unit.name);
        }
        for (unit, rec) in &disc.missing {
            add(unit.name.clone(), rec.path.clone(), unit, true, false);
        }
        rows.sort_by(|x, y| x.name.cmp(&y.name));
        reports.push(RootReport {
            root: root.path.clone(),
            store: store.dir.clone(),
            store_exists: store.exists(),
            free_bytes: ctx.btrfs.statfs(&root.path).ok().map(|u| u.free),
            projects: rows,
            unadopted: disc.unadopted.iter().map(|c| c.name.clone()).collect(),
            foreign_subvolumes: disc.foreign_subvols.iter().map(|c| c.name.clone()).collect(),
            ignored: disc.ignored.clone(),
            leftovers: disc.leftovers.clone(),
            container_snapshots: store.container().snapshots().map(|s| s.len()).unwrap_or(0),
        });
    }
    emit(ctx, &reports, || {
        let mut out = String::new();
        for r in &reports {
            out.push_str(&format!(
                "root {}  store {}{}  free {}  container snapshots {}\n",
                r.root.display(),
                r.store.display(),
                if r.store_exists { "" } else { " (missing: run `bpm setup`)" },
                r.free_bytes.map(fmt_bytes).unwrap_or_else(|| "?".into()),
                r.container_snapshots
            ));
            let mut t = Table::new(&["PROJECT", "STAGE", "IDLE", "SNAPS", "NEWEST", "FLAGS"]);
            for p in &r.projects {
                let mut flags = p.flags.clone();
                if p.frozen.is_some() {
                    flags.insert(0, "FROZEN".into());
                }
                if let Some(b) = p.exclusive_bytes {
                    flags.push(format!("store-exclusive {}", fmt_bytes(b)));
                }
                t.row(vec![
                    p.name.clone(),
                    p.stage.clone(),
                    p.idle.clone().unwrap_or_else(|| "-".into()),
                    p.snapshots.to_string(),
                    p.newest
                        .map(|id| format!("#{id} {} ago", p.newest_age.clone().unwrap_or_default()))
                        .unwrap_or_else(|| "-".into()),
                    flags.join(" "),
                ]);
            }
            if !t.is_empty() {
                out.push_str(&t.render());
            }
            for p in r.projects.iter().filter(|p| p.frozen.is_some()) {
                out.push_str(&format!(
                    "\n{} is FROZEN: {}\n  inspect: bpm list {0}; bpm diff {0} held\n  resolve: bpm rollback {0} held   or   bpm unfreeze {0}\n",
                    p.name,
                    p.frozen.as_ref().unwrap().join("; ")
                ));
            }
            if !r.unadopted.is_empty() {
                out.push_str(&format!("\nunadopted (plain directories, not protected): {}\n", r.unadopted.join(", ")));
            }
            if a.unadopted {
                if !r.foreign_subvolumes.is_empty() {
                    out.push_str(&format!("subvolumes not managed by bpm: {}\n", r.foreign_subvolumes.join(", ")));
                }
                if !r.ignored.is_empty() {
                    out.push_str(&format!("ignored: {}\n", r.ignored.join(", ")));
                }
            }
            if !r.leftovers.is_empty() {
                let l: Vec<String> = r.leftovers.iter().map(|p| p.display().to_string()).collect();
                out.push_str(&format!("leftovers from interrupted operations (bpm doctor --fix): {}\n", l.join(", ")));
            }
            out.push('\n');
        }
        out
    });
    Ok(())
}

#[derive(Serialize)]
struct Detail {
    name: String,
    path: PathBuf,
    uuid: String,
    uuid_history: Vec<String>,
    owner: (u32, u32),
    adopted: String,
    state: crate::store::ProjectState,
    profiles: Vec<String>,
    banlist: BTreeMap<String, String>,
    unprotected_nested: Vec<String>,
    snapshots_by_kind: BTreeMap<String, usize>,
    live_ctransid: Option<u64>,
    changed_since_newest: Option<bool>,
}

fn detail(ctx: &Ctx, spec: &str) -> Result<()> {
    let pref = project::resolve(ctx, spec)?;
    let eff = super::effective(ctx, &pref)?;
    let st = pref.unit.read_state()?;
    let snaps = pref.unit.snapshots()?;
    let live = ctx.btrfs.subvol_info(pref.path()).ok().filter(|i| i.uuid == pref.record.uuid);
    let mut banlist = BTreeMap::new();
    for b in &eff.banlist {
        let p = pref.path().join(b);
        let s = match std::fs::symlink_metadata(&p) {
            Err(_) => "absent".to_string(),
            Ok(m) if !m.is_dir() => "not a directory".into(),
            Ok(_) if ctx.btrfs.is_subvolume(&p).unwrap_or(false) => "nested subvolume (excluded)".into(),
            Ok(_) => "PLAIN DIRECTORY (included in snapshots until converted)".into(),
        };
        banlist.insert(b.clone(), s);
    }
    let mut unprotected = Vec::new();
    if live.is_some() {
        let mut it = walkdir::WalkDir::new(pref.path()).follow_links(false).min_depth(1).into_iter();
        while let Some(Ok(e)) = it.next() {
            if crate::util::walk::entry_is_subvol(&e, &|p, i| ctx.is_subvol(p, i)) {
                let rel = e.path().strip_prefix(pref.path()).unwrap().to_string_lossy().into_owned();
                if !eff.banlist.contains(&rel) {
                    unprotected.push(rel);
                }
                it.skip_current_dir();
            }
        }
    }
    let mut by_kind = BTreeMap::new();
    for m in &snaps {
        *by_kind.entry(m.kind.to_string()).or_insert(0) += 1;
    }
    let d = Detail {
        name: pref.name().into(),
        path: pref.path().into(),
        uuid: pref.record.uuid.to_string(),
        uuid_history: pref.record.uuid_history.iter().map(|u| u.to_string()).collect(),
        owner: (pref.record.owner_uid, pref.record.owner_gid),
        adopted: fmt_local(pref.record.adopted, &ctx.tz),
        state: st.clone(),
        profiles: eff.profiles.clone(),
        banlist,
        unprotected_nested: unprotected,
        snapshots_by_kind: by_kind,
        live_ctransid: live.as_ref().map(|l| l.ctransid),
        changed_since_newest: live.as_ref().map(|l| crate::policy::change::changed_since(l, newest(&snaps))),
    };
    emit(ctx, &d, || {
        let now = ctx.now();
        let mut s = format!("{}  {}\n", d.name, d.path.display());
        s += &format!(
            "  stage      {}{}\n",
            st.stage,
            st.stage_since.map(|t| format!(" since {}", fmt_local(t, &ctx.tz))).unwrap_or_default()
        );
        s += &format!(
            "  last change {}\n",
            st.last_change_at
                .map(|t| format!("{} ({} ago)", fmt_local(t, &ctx.tz), fmt_age(age(now, t))))
                .unwrap_or_else(|| "-".into())
        );
        s += &format!(
            "  live       {}\n",
            match (&live, d.changed_since_newest) {
                (None, _) => "MISSING".to_string(),
                (Some(_), Some(true)) => "changed since newest snapshot".into(),
                (Some(_), _) => "identical to newest snapshot".into(),
            }
        );
        s += &format!("  profiles   {}\n", d.profiles.join(", "));
        s += &format!(
            "  snapshots  {} ({})\n",
            snaps.len(),
            d.snapshots_by_kind.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")
        );
        if let Some(f) = &st.frozen {
            s += &format!("  FROZEN     since {}: {}\n", fmt_local(f.since, &ctx.tz), f.reasons.join("; "));
            if let Some(r) = f.ref_snap {
                s += &format!(
                    "             last good snapshot #{r} (held): bpm diff {} {r}   bpm rollback {} {r}   bpm unfreeze {}\n",
                    d.name, d.name, d.name
                );
            }
        }
        if let Some(r) = &st.ref_stats {
            s += &format!(
                "  guard ref  {} files (#{}), {} (#{})\n",
                r.files,
                r.files_snap,
                fmt_bytes(r.bytes),
                r.bytes_snap
            );
        }
        if let Some(r) = &st.recompress {
            s += &format!(
                "  recompress {} zstd:{}{}\n",
                fmt_local(r.at, &ctx.tz),
                r.level,
                r.skipped.as_ref().map(|x| format!(" skipped: {x}")).unwrap_or_default()
            );
        }
        s += "  banlist\n";
        for (k, v) in &d.banlist {
            s += &format!("    {k:<24} {v}\n");
        }
        if !d.unprotected_nested.is_empty() {
            s += &format!(
                "  WARNING: nested subvolumes not covered by snapshots: {}\n",
                d.unprotected_nested.join(", ")
            );
        }
        if let Some(e) = &st.last_error {
            s += &format!("  last error {}: {}\n", fmt_local(e.at, &ctx.tz), e.message);
        }
        s
    });
    Ok(())
}
