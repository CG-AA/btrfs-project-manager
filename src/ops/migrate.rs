//! `bpm migrate-from-snapper`: stop snapper's timeline on a root and hand it to bpm.

use crate::cli::MigrateArgs;
use crate::ctx::Ctx;
use crate::error::{refused, usage};
use crate::mechanics::adopt;
use crate::output::emit;
use crate::project;
use crate::store::{Origin, SnapshotKind, SnapshotMeta};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

fn snapper(ctx: &Ctx, args: &[&str], steps: &mut Vec<String>) -> Result<()> {
    steps.push(format!("snapper {}", args.join(" ")));
    if ctx.opts.dry_run {
        return Ok(());
    }
    let out = Command::new("snapper").args(args).output().context("run snapper")?;
    if !out.status.success() {
        bail!("snapper {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn snapper_snapshots(root: &Path) -> Vec<(u64, PathBuf, String)> {
    let dir = root.join(".snapshots");
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let Ok(n) = e.file_name().to_string_lossy().parse::<u64>() else {
            continue;
        };
        let snap = e.path().join("snapshot");
        if snap.is_dir() {
            let desc = std::fs::read_to_string(e.path().join("info.xml"))
                .ok()
                .and_then(|x| {
                    x.split("<description>").nth(1).and_then(|s| s.split("</description>").next()).map(String::from)
                })
                .unwrap_or_default();
            out.push((n, snap, desc));
        }
    }
    out.sort_by_key(|x| x.0);
    out
}

pub fn run(ctx: &Ctx, a: MigrateArgs) -> Result<()> {
    let cfg_file = PathBuf::from("/etc/snapper/configs").join(&a.snapper_config);
    let text = std::fs::read_to_string(&cfg_file).with_context(|| format!("read {}", cfg_file.display()))?;
    let subvol = text
        .lines()
        .find_map(|l| l.strip_prefix("SUBVOLUME="))
        .map(|v| PathBuf::from(v.trim_matches('"')))
        .ok_or_else(|| usage("snapper config has no SUBVOLUME"))?;
    let root = ctx.roots().into_iter().find(|r| r.path == subvol).ok_or_else(|| {
        usage(format!("snapper config {} manages {}, which is not a bpm root", a.snapper_config, subvol.display()))
    })?;
    let store = ctx.store(&root);
    if !store.exists() {
        return Err(refused("run `bpm setup` first"));
    }
    if a.delete_snapper && !a.yes {
        return Err(refused("--delete-snapper deletes snapper's snapshots of this root; add --yes"));
    }
    let mut steps = Vec::new();
    snapper(
        ctx,
        &["-c", &a.snapper_config, "set-config", "TIMELINE_CREATE=no", "TIMELINE_CLEANUP=no", "NUMBER_CLEANUP=no"],
        &mut steps,
    )?;

    if a.import {
        let unit = store.container();
        unit.ensure_dir()?;
        let _l = unit.lock(ctx.cfg.global.lock_timeout)?;
        let rec = super::snap::container_record(ctx, &root, &unit)?;
        for (n, snap, desc) in snapper_snapshots(&root.path) {
            let info = ctx.btrfs.subvol_info(&snap)?;
            if !rec.owns(info.parent_uuid) {
                tracing::warn!("snapper snapshot {n} is not a snapshot of {}; left alone", root.path.display());
                continue;
            }
            let id = unit.next_id()?;
            steps.push(format!("import snapper #{n} as container snapshot #{id}"));
            if ctx.opts.dry_run {
                continue;
            }
            let tmp = unit.dir.join(format!("{id}.tmp"));
            std::fs::create_dir(&tmp)?;
            crate::util::fs::rename_strict(&snap, &tmp.join("snapshot"))?;
            let meta = SnapshotMeta {
                format: 1,
                id,
                project: crate::store::CONTAINER.into(),
                created: info.otime.unwrap_or(ctx.now()),
                kind: SnapshotKind::Import,
                reason: format!("snapper {} #{n} {desc}", a.snapper_config),
                pair: None,
                hold: true,
                hold_note: "imported from snapper".into(),
                source_uuid: info.parent_uuid.unwrap_or_default(),
                source_ctransid: info.ctransid,
                snapshot_uuid: info.uuid,
                snapshot_otransid: info.otransid,
                received_uuid: None,
                stats: None,
                origin: Origin {
                    bpm: env!("CARGO_PKG_VERSION").into(),
                    user: ctx.invoker.user.clone(),
                    argv: ctx.invoker.argv.clone(),
                },
            };
            unit.write_meta(&tmp, &meta)?;
            std::fs::rename(&tmp, unit.snapshot_dir(id))?;
            let _ = std::fs::remove_dir_all(snap.parent().unwrap());
        }
    }

    let mut failed = Vec::new();
    if a.adopt_all {
        let disc = project::discover(ctx, &root, &store)?;
        for c in disc.unadopted.iter().chain(disc.foreign_subvols.iter()) {
            steps.push(format!("adopt {}", c.name));
            if let Err(e) = adopt::adopt(
                ctx,
                &root,
                &c.name,
                &adopt::AdoptOpts { keep_build: None, force: false, verify_paths: false, require_unused: false },
            ) {
                tracing::error!("{}: {e:#}", c.name);
                failed.push(c.name.clone());
            }
        }
    }

    if a.delete_snapper {
        let disc = project::discover(ctx, &root, &store)?;
        if !disc.unadopted.is_empty() && store.container().snapshots()?.is_empty() {
            return Err(refused(format!(
                "not deleting snapper snapshots: {} are still unadopted and no container snapshot protects them yet (run `bpm snap --container` or adopt them first)",
                disc.unadopted.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
        let remaining: Vec<String> = snapper_snapshots(&root.path).iter().map(|x| x.0.to_string()).collect();
        if !remaining.is_empty() {
            let mut args = vec!["-c", a.snapper_config.as_str(), "delete"];
            args.extend(remaining.iter().map(String::as_str));
            snapper(ctx, &args, &mut steps)?;
        }
        snapper(ctx, &["-c", &a.snapper_config, "delete-config"], &mut steps)?;
        steps
            .push("space held by snapper snapshots is freed asynchronously (watch with `btrfs subvolume sync`)".into());
    }
    emit(ctx, &serde_json::json!({"steps": steps, "adopt_failed": failed}), || {
        let mut s = steps.join("\n");
        if !failed.is_empty() {
            s += &format!("\nadoption failed for: {} (retry when idle: bpm adopt <name>)", failed.join(", "));
        }
        if !a.adopt_all {
            s += "\nprojects will be adopted by the timer one per tick, or now with `bpm adopt --all`";
        }
        if !a.delete_snapper {
            s += &format!(
                "\nwhen everything is adopted: bpm migrate-from-snapper --snapper-config {} --delete-snapper --yes",
                a.snapper_config
            );
        }
        s
    });
    if !failed.is_empty() {
        return Err(crate::error::BpmError::Partial { failed: failed.len() }.into());
    }
    Ok(())
}
