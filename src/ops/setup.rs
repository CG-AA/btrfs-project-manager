//! `bpm setup`: config, store subvolumes, hooks directory, systemd units.

use crate::cli::SetupArgs;
use crate::config;
use crate::ctx::Ctx;
use crate::output::emit;
use crate::store::StoreInfo;
use anyhow::{Context, Result, bail};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

pub const SERVICE: &str = include_str!("../../assets/systemd/bpm.service");
pub const TIMER: &str = include_str!("../../assets/systemd/bpm.timer");
pub const HEAVY_SERVICE: &str = include_str!("../../assets/systemd/bpm-heavy.service");
pub const HEAVY_TIMER: &str = include_str!("../../assets/systemd/bpm-heavy.timer");
pub const TIMERS: [&str; 2] = ["bpm.timer", "bpm-heavy.timer"];
pub const CLAUDE_HOOK: &str = include_str!("../../assets/claude-hook.json");

fn write_if_changed(ctx: &Ctx, path: &Path, content: &str, steps: &mut Vec<String>) -> Result<bool> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(false);
    }
    steps.push(format!("write {}", path.display()));
    if !ctx.opts.dry_run {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        crate::util::fs::write_atomic(path, content.as_bytes(), 0o644)?;
    }
    Ok(true)
}

fn systemctl(ctx: &Ctx, args: &[&str], steps: &mut Vec<String>) -> Result<()> {
    steps.push(format!("systemctl {}", args.join(" ")));
    if ctx.opts.dry_run {
        return Ok(());
    }
    let st = Command::new("systemctl").args(args).status().context("run systemctl")?;
    if !st.success() {
        bail!("systemctl {} failed", args.join(" "));
    }
    Ok(())
}

pub fn run(ctx: &Ctx, a: SetupArgs) -> Result<()> {
    if a.print_claude_hook {
        println!("{}", CLAUDE_HOOK.replace("@BIN@", &a.bin.display().to_string()).trim_end());
        eprintln!("# add this to ~/.claude/settings.json (all projects) or <project>/.claude/settings.json");
        return Ok(());
    }
    let mut steps = Vec::new();
    let cfg_path = config::config_path(ctx.opts.config.as_deref());
    if !cfg_path.exists() || a.force_config {
        write_if_changed(ctx, &cfg_path, config::DEFAULT_CONFIG_TOML, &mut steps)?;
    }
    let cfg = if cfg_path.exists() { config::load(Some(&cfg_path))?.config } else { ctx.cfg.clone() };
    if !cfg.global.hooks_dir.exists() {
        steps.push(format!("create {}", cfg.global.hooks_dir.display()));
        if !ctx.opts.dry_run {
            std::fs::create_dir_all(&cfg.global.hooks_dir)?;
            std::fs::set_permissions(&cfg.global.hooks_dir, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    let mut rw = Vec::new();
    for root in &cfg.roots {
        if !root.path.is_dir() {
            tracing::warn!("root {} does not exist; skipped", root.path.display());
            continue;
        }
        rw.push(root.path.display().to_string());
        if !ctx.btrfs.is_subvolume(&root.path)? {
            tracing::warn!(
                "{} is not a subvolume root: projects can still be managed, but container snapshots are disabled",
                root.path.display()
            );
        }
        let store = crate::store::Store::new(&root.path, &cfg.global.store_dir, ctx.opts.dry_run);
        if store.dir.exists() {
            if !ctx.btrfs.is_subvolume(&store.dir)? {
                bail!("{} exists but is not a subvolume; move it away and rerun setup", store.dir.display());
            }
        } else {
            steps.push(format!("create store subvolume {}", store.dir.display()));
            ctx.btrfs.create_subvolume(&store.dir)?;
            if !ctx.opts.dry_run {
                std::fs::set_permissions(&store.dir, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        if !store.exists() && !ctx.opts.dry_run {
            std::fs::create_dir_all(store.projects_dir())?;
            std::fs::create_dir_all(store.container().dir)?;
            let root_uuid = ctx.btrfs.subvol_info(&root.path)?.uuid;
            store.write_info(&StoreInfo {
                format: 1,
                root: root.path.clone(),
                root_uuid,
                created: ctx.now(),
                bpm_version: env!("CARGO_PKG_VERSION").into(),
            })?;
            steps.push(format!("initialize {}", store.info_path().display()));
        }
    }
    if cfg.global.archive_dir.parent().is_some_and(|p| p.exists()) {
        rw.push(format!("-{}", cfg.global.archive_dir.parent().unwrap().display()));
    }
    if !a.no_units {
        // the units run with exactly the config this setup used
        let cfg_abs = std::path::absolute(&cfg_path)?;
        let fill = |unit: &str| {
            unit.replace("@BIN@", &a.bin.display().to_string())
                .replace("@RW@", &rw.join(" "))
                .replace("@CFG@", &cfg_abs.display().to_string())
        };
        let mut changed = false;
        for (name, content) in [
            ("bpm.service", fill(SERVICE)),
            ("bpm.timer", fill(TIMER)),
            ("bpm-heavy.service", fill(HEAVY_SERVICE)),
            ("bpm-heavy.timer", fill(HEAVY_TIMER)),
        ] {
            changed |= write_if_changed(ctx, &Path::new("/etc/systemd/system").join(name), &content, &mut steps)?;
        }
        if changed {
            systemctl(ctx, &["daemon-reload"], &mut steps)?;
        }
        if !a.no_enable {
            let mut args = vec!["enable", "--now"];
            args.extend(TIMERS);
            systemctl(ctx, &args, &mut steps)?;
        }
        if !a.bin.exists() {
            tracing::warn!("{} does not exist yet; install the binary there (see scripts/install.sh)", a.bin.display());
        }
    }
    emit(ctx, &steps, || {
        let mut s = if steps.is_empty() { "already set up".to_string() } else { steps.join("\n") };
        s += "\n\nnext:\n  bpm status               # what is managed, what is not\n  bpm migrate-from-snapper # if snapper snapshots this root\n  bpm adopt --all          # or let the timer adopt projects one per tick\n  bpm setup --print-claude-hook";
        s
    });
    Ok(())
}
