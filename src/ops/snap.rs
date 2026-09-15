//! `bpm snap`: manual snapshots, and the Claude Code PreToolUse hook mode.

use crate::cli::SnapArgs;
use crate::ctx::Ctx;
use crate::error::usage;
use crate::mechanics::Target;
use crate::mechanics::observe;
use crate::mechanics::snapshot::{self, SnapOpts};
use crate::output::emit;
use crate::policy::change;
use crate::project::{self, ProjectRef};
use crate::store::{CONTAINER, ProjectRecord, SnapshotKind, newest};
use crate::util::time::age;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

#[derive(Serialize, Debug)]
pub struct SnapResult {
    pub project: String,
    pub id: Option<u64>,
    pub kind: String,
    pub skipped: Option<String>,
}

pub fn run(ctx: &Ctx, a: SnapArgs) -> Result<()> {
    if a.claude_hook {
        if let Err(e) = claude_hook(ctx, &a) {
            eprintln!("bpm: claude hook: {e:#}");
        }
        return Ok(());
    }
    let never_fail = a.never_fail;
    match run_inner(ctx, &a) {
        Err(e) if never_fail => {
            eprintln!("bpm: snapshot skipped: {e:#}");
            Ok(())
        }
        other => other,
    }
}

fn run_inner(ctx: &Ctx, a: &SnapArgs) -> Result<()> {
    let kind: SnapshotKind = a.kind.parse().map_err(|e: String| usage(e))?;
    if !matches!(
        kind,
        SnapshotKind::Manual | SnapshotKind::Pre | SnapshotKind::Post | SnapshotKind::Hook | SnapshotKind::Auto
    ) {
        return Err(usage(format!("--kind {kind} is reserved for bpm itself")));
    }
    let mut results = Vec::new();
    if a.container {
        for root in ctx.roots() {
            results.push(snap_container(ctx, &root, a, kind)?);
        }
    }
    let targets: Vec<ProjectRef> = if a.all {
        let mut v = Vec::new();
        for root in ctx.roots() {
            let store = ctx.store(&root);
            for f in project::discover(ctx, &root, &store)?.managed {
                v.push(ProjectRef { root: root.clone(), store: store.clone(), unit: f.unit, record: f.record });
            }
        }
        v
    } else if a.container && a.project.is_none() {
        vec![]
    } else {
        vec![super::resolve_or_cwd(ctx, a.project.as_deref())?]
    };
    let mut errors = 0;
    for pref in targets {
        match snap_project(ctx, &pref, a, kind) {
            Ok(r) => results.push(r),
            Err(e) => {
                errors += 1;
                tracing::error!("{}: {e:#}", pref.name());
            }
        }
    }
    emit(ctx, &results, || {
        results
            .iter()
            .map(|r| match (r.id, &r.skipped) {
                (Some(id), _) => format!("{}: snapshot #{id} ({})", r.project, r.kind),
                (None, Some(why)) => format!("{}: skipped ({why})", r.project),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    if errors > 0 {
        bail!(crate::error::BpmError::Partial { failed: errors });
    }
    Ok(())
}

fn default_reason(a: &SnapArgs, kind: SnapshotKind) -> String {
    if a.reason.is_empty() {
        format!("{kind} by {}", std::env::var("SUDO_USER").or_else(|_| std::env::var("USER")).unwrap_or_default())
    } else {
        a.reason.clone()
    }
}

pub fn snap_project(ctx: &Ctx, pref: &ProjectRef, a: &SnapArgs, kind: SnapshotKind) -> Result<SnapResult> {
    let eff = super::effective(ctx, pref)?;
    let lock = match pref.unit.lock(super::lock_timeout(ctx, a.lock_timeout)) {
        Ok(l) => Some(l),
        // A tick may hold the project lock for a long stats walk. Snapshot ids are reserved
        // atomically, so a hook snapshot can go ahead; the next tick reconciles the state.
        Err(e) if kind == SnapshotKind::Hook && crate::error::exit_code_for(&e) == 4 => {
            tracing::warn!("{}: {e}; taking the hook snapshot without the project lock", pref.name());
            None
        }
        Err(e) => return Err(e),
    };
    let mut st = pref.unit.read_state()?;
    let mut snaps = pref.unit.snapshots()?;
    let live = ctx.btrfs.subvol_info(pref.path()).with_context(|| format!("{} is missing", pref.path().display()))?;
    if live.uuid != pref.record.uuid {
        bail!("{} is not the managed subvolume (uuid {} != {})", pref.path().display(), live.uuid, pref.record.uuid);
    }
    let skip = |why: &str| {
        Ok(SnapResult { project: pref.name().into(), id: None, kind: kind.to_string(), skipped: Some(why.into()) })
    };
    if let (Some(t), Some(n)) = (a.throttle, newest(&snaps)) {
        if age(ctx.now(), n.created) < t {
            return skip("throttled");
        }
    }
    if a.if_changed && {
        // buffered writes only reach the subvolume counters at writeback
        ctx.btrfs.sync(pref.path())?;
        !change::changed_since(&ctx.btrfs.subvol_info(pref.path())?, newest(&snaps))
    } {
        return skip("unchanged");
    }
    let target = Target::for_project(pref, &eff);
    let mut o = SnapOpts::new(kind, default_reason(a, kind));
    o.stats = !a.quick && eff.policy.snapshot.stats;
    o.stats_budget = eff.policy.snapshot.stats_budget;
    o.hold = a.hold;
    o.pair = a.pair;
    let meta = snapshot::take(ctx, &target, &o)?;
    // Without the lock the tick owns the state; it counts and checks this snapshot next time.
    if let Some(lu) = &lock {
        observe::note_snapshot(&mut st, &meta);
        snaps.push(meta.clone());
        observe::after_command(ctx, lu, &target, &eff, pref.record.adopted, &mut st, &mut snaps)?;
        lu.write_state(&st)?;
    }
    Ok(SnapResult { project: pref.name().into(), id: Some(meta.id), kind: kind.to_string(), skipped: None })
}

pub fn container_record(
    ctx: &Ctx,
    root: &crate::config::RootCfg,
    unit: &crate::store::LockedUnit,
) -> Result<ProjectRecord> {
    if !ctx.btrfs.is_subvolume(&root.path)? {
        bail!("{} is not a subvolume root; container snapshots are not possible", root.path.display());
    }
    let info = ctx.btrfs.subvol_info(&root.path)?;
    match unit.read_record()? {
        Some(r) if r.uuid == info.uuid => Ok(r),
        Some(r) => {
            bail!("container store belongs to subvolume {} but {} is {}", r.uuid, root.path.display(), info.uuid)
        }
        None => {
            let r = ProjectRecord::new(CONTAINER, &root.path, info.uuid, 0, 0, ctx.now());
            unit.write_record(&r)?;
            Ok(r)
        }
    }
}

fn snap_container(ctx: &Ctx, root: &crate::config::RootCfg, a: &SnapArgs, kind: SnapshotKind) -> Result<SnapResult> {
    let store = ctx.store(root);
    let unit = store.container();
    unit.ensure_dir()?;
    let lu = unit.lock(super::lock_timeout(ctx, a.lock_timeout))?;
    let rec = container_record(ctx, root, &lu)?;
    let snaps = unit.snapshots()?;
    if a.if_changed && {
        ctx.btrfs.sync(&root.path)?;
        !change::changed_since(&ctx.btrfs.subvol_info(&root.path)?, newest(&snaps))
    } {
        return Ok(SnapResult {
            project: CONTAINER.into(),
            id: None,
            kind: kind.to_string(),
            skipped: Some("unchanged".into()),
        });
    }
    let t = Target {
        unit: &unit,
        name: CONTAINER,
        live: &root.path,
        root: &root.path,
        expected_uuid: Some(rec.uuid),
        owner: None,
        stats_exclude: vec![],
        sentinels: vec![],
    };
    let mut o = SnapOpts::new(kind, default_reason(a, kind));
    o.stats = false;
    o.hold = a.hold;
    let m = snapshot::take(ctx, &t, &o)?;
    Ok(SnapResult { project: CONTAINER.into(), id: Some(m.id), kind: kind.to_string(), skipped: None })
}

// ---------------- Claude Code hook ----------------

fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Projects touched by a shell command: the working directory plus paths mentioned in it.
pub fn projects_in_command(ctx: &Ctx, cwd: &Path, command: &str) -> Vec<(crate::config::RootCfg, String)> {
    let mut found: Vec<(crate::config::RootCfg, String)> = Vec::new();
    let mut push = |p: &Path| {
        if let Some((root, name)) = project::locate_path(ctx, p) {
            if !found.iter().any(|(r, n)| r.path == root.path && *n == name) && found.len() < 8 {
                found.push((root, name));
            }
        }
    };
    push(cwd);
    for raw in command
        .split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | ';' | '&' | '|' | '(' | ')' | '=' | '<' | '>'))
    {
        if !raw.contains('/') {
            continue;
        }
        let p = if raw.starts_with('/') { PathBuf::from(raw) } else { cwd.join(raw) };
        push(&lexical_normalize(&p));
    }
    found
}

fn truncate(s: &str, n: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= n {
        one_line
    } else {
        format!("{}…", one_line.chars().take(n).collect::<String>())
    }
}

fn claude_hook(ctx: &Ctx, _a: &SnapArgs) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let v: serde_json::Value = serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);
    let cwd = v["cwd"].as_str().map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let command = v["tool_input"]["command"].as_str().unwrap_or("");
    let destructive = ctx.cfg.global.destructive_regexes()?.iter().any(|r| r.is_match(command));
    let reason = format!("claude{}: {}", if destructive { " (destructive)" } else { "" }, truncate(command, 200));
    for (root, name) in projects_in_command(ctx, &cwd, command) {
        let Ok(pref) = project::resolve_in(ctx, &root, &name) else {
            continue;
        };
        // The child flushes and re-checks for changes; here only cheap checks happen.
        if ctx.btrfs.subvol_info(pref.path()).map(|l| l.uuid != pref.record.uuid).unwrap_or(true) {
            continue;
        }
        let snaps = pref.unit.snapshots().unwrap_or_default();
        if !destructive {
            if let Some(n) = newest(&snaps) {
                if age(ctx.now(), n.created) < ctx.cfg.global.hook_throttle {
                    continue;
                }
            }
        }
        let mut args: Vec<String> = vec![
            "snap".into(),
            name.clone(),
            "--root".into(),
            root.path.display().to_string(),
            "--kind".into(),
            "hook".into(),
            "--reason".into(),
            reason.clone(),
            "--quick".into(),
            "--if-changed".into(),
            "--never-fail".into(),
            "--quiet".into(),
            "--lock-timeout".into(),
            "3s".into(),
        ];
        if !destructive {
            args.push("--throttle".into());
            args.push(humantime::format_duration(ctx.cfg.global.hook_throttle).to_string());
        }
        let config = ctx.opts.config.clone().or_else(|| std::env::var_os("BPM_CONFIG").map(Into::into));
        let args: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
        let via_sudo = !(crate::privilege::is_root() || ctx.opts.no_sudo);
        let mut cmd = crate::privilege::self_command(&args, config.as_deref(), via_sudo)?;
        let status = cmd.stdin(Stdio::null()).stdout(Stdio::null()).status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("bpm: hook snapshot of {name} exited with {s}"),
            Err(e) => eprintln!("bpm: hook snapshot of {name}: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalize_and_truncate() {
        assert_eq!(lexical_normalize(Path::new("/space/a/../b/./c")), PathBuf::from("/space/b/c"));
        assert_eq!(truncate("rm  -rf\n build", 100), "rm -rf build");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
