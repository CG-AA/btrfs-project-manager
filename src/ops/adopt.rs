//! `bpm adopt` and `bpm convert`.

use crate::cli::{AdoptArgs, ConvertArgs};
use crate::ctx::Ctx;
use crate::error::{BpmError, usage};
use crate::mechanics::{adopt, banlist};
use crate::output::emit;
use crate::project;
use anyhow::Result;

pub fn run(ctx: &Ctx, a: AdoptArgs) -> Result<()> {
    let roots = ctx.roots();
    let mut todo: Vec<(crate::config::RootCfg, String)> = Vec::new();
    if a.all {
        for root in &roots {
            let store = ctx.store(root);
            let d = project::discover(ctx, root, &store)?;
            for c in d.unadopted.iter().chain(d.foreign_subvols.iter()) {
                todo.push((root.clone(), c.name.clone()));
            }
        }
    }
    for p in &a.projects {
        let (root, name) = if p.contains('/') {
            project::locate_path(ctx, std::path::Path::new(p))
                .ok_or_else(|| usage(format!("{p} is not inside a configured root")))?
        } else {
            let root = match roots.as_slice() {
                [one] => one.clone(),
                _ => roots
                    .iter()
                    .find(|r| r.path.join(p).is_dir())
                    .cloned()
                    .ok_or_else(|| usage(format!("{p} not found in any root; pass --root")))?,
            };
            (root, p.clone())
        };
        todo.push((root, name));
    }
    if todo.is_empty() {
        return Err(usage("nothing to adopt (name projects or pass --all)"));
    }
    let opts = adopt::AdoptOpts {
        keep_build: a.no_keep_build.then_some(false),
        force: a.force,
        verify_paths: a.verify_paths,
        require_unused: false,
    };
    let mut reports = Vec::new();
    let mut failed = 0;
    for (root, name) in todo {
        if !ctx.opts.quiet && !ctx.opts.json {
            eprintln!("bpm: adopting {}/{name} …", root.path.display());
        }
        match adopt::adopt(ctx, &root, &name, &opts) {
            Ok(r) => reports.push(r),
            Err(e) => {
                failed += 1;
                tracing::error!("{name}: {e:#}");
            }
        }
    }
    emit(ctx, &reports, || {
        reports
            .iter()
            .map(|r| {
                format!(
                    "{}: {} ({} files, {}), nested: [{}], snapshot #{}, stage {}",
                    r.name,
                    if r.converted { "converted to subvolume" } else { "registered" },
                    r.files,
                    crate::util::bytes::fmt_bytes(r.bytes),
                    r.nested.join(", "),
                    r.snapshot,
                    r.stage
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    if failed > 0 {
        return Err(BpmError::Partial { failed }.into());
    }
    Ok(())
}

pub fn convert(ctx: &Ctx, a: ConvertArgs) -> Result<()> {
    let pref = project::resolve(ctx, &a.project)?;
    let eff = super::effective(ctx, &pref)?;
    let rel = crate::util::fs::safe_relative(&a.path).ok_or_else(|| usage("path must be relative to the project"))?;
    let rel = rel.to_string_lossy().into_owned();
    if !eff.banlist.contains(&rel) {
        tracing::warn!("{rel} is not in the banlist of {}; it will be excluded from snapshots anyway", pref.name());
    }
    let _l = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
    ctx.btrfs.sync(pref.path())?;
    let before = ctx.btrfs.subvol_info(pref.path())?;
    banlist::convert(ctx, pref.path(), &rel, !a.discard_contents, a.force)?;
    let mut st = pref.unit.read_state()?;
    st.pending_convert.remove(&rel);
    st.banlist_seen.insert(rel.clone());
    ctx.btrfs.sync(pref.path())?;
    let after = ctx.btrfs.subvol_info(pref.path())?;
    crate::mechanics::observe::note_tool_change(&mut st, &before, &after, ctx.now());
    pref.unit.write_state(&st)?;
    emit(ctx, &serde_json::json!({"project": pref.name(), "converted": rel}), || {
        format!("{}: {rel} is now a nested subvolume", pref.name())
    });
    Ok(())
}
