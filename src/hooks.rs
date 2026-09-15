//! User hooks: executables in `<hooks_dir>/<event>/` (root) and `<project>/.bpm/hooks/<event>/`.

use crate::ctx::Ctx;
use crate::error::BpmError;
use crate::privilege;
use crate::store::{SnapshotKind, Stage};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    PreSnapshot,
    PostSnapshot,
    PreDelete,
    PostDelete,
    PreAdopt,
    PostAdopt,
    StageChange,
    PreRecompress,
    PostRecompress,
    PreArchive,
    PostArchive,
    PreRollback,
    PostRollback,
    OnFreeze,
    OnShrinkDetected,
    OnOrphaned,
}

pub const ALL_EVENTS: &[HookEvent] = &[
    HookEvent::PreSnapshot,
    HookEvent::PostSnapshot,
    HookEvent::PreDelete,
    HookEvent::PostDelete,
    HookEvent::PreAdopt,
    HookEvent::PostAdopt,
    HookEvent::StageChange,
    HookEvent::PreRecompress,
    HookEvent::PostRecompress,
    HookEvent::PreArchive,
    HookEvent::PostArchive,
    HookEvent::PreRollback,
    HookEvent::PostRollback,
    HookEvent::OnFreeze,
    HookEvent::OnShrinkDetected,
    HookEvent::OnOrphaned,
];

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        use HookEvent::*;
        match self {
            PreSnapshot => "pre-snapshot",
            PostSnapshot => "post-snapshot",
            PreDelete => "pre-delete",
            PostDelete => "post-delete",
            PreAdopt => "pre-adopt",
            PostAdopt => "post-adopt",
            StageChange => "stage-change",
            PreRecompress => "pre-recompress",
            PostRecompress => "post-recompress",
            PreArchive => "pre-archive",
            PostArchive => "post-archive",
            PreRollback => "pre-rollback",
            PostRollback => "post-rollback",
            OnFreeze => "on-freeze",
            OnShrinkDetected => "on-shrink-detected",
            OnOrphaned => "on-orphaned",
        }
    }
    pub fn parse(s: &str) -> Option<HookEvent> {
        ALL_EVENTS.iter().copied().find(|e| e.as_str() == s)
    }
    pub fn can_veto(self) -> bool {
        self.as_str().starts_with("pre-")
    }
}

#[derive(Default)]
pub struct HookCtx<'a> {
    pub project: Option<&'a str>,
    pub project_path: Option<&'a Path>,
    pub owner: Option<(u32, u32)>,
    pub root: Option<&'a Path>,
    pub snapshot: Option<(u64, PathBuf, SnapshotKind)>,
    pub stage: Option<(Stage, Stage)>,
    pub reason: String,
    pub extra: Vec<(&'static str, String)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HookListing {
    pub event: String,
    pub path: PathBuf,
    pub scope: &'static str,
    pub will_run: bool,
    pub skip_reason: Option<String>,
}

fn check_file(path: &Path, expected_uid: u32) -> Result<(), String> {
    let md = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !md.is_file() {
        return Err("not a regular file".into());
    }
    let mode = md.permissions().mode();
    if mode & 0o111 == 0 {
        return Err("not executable".into());
    }
    if mode & 0o022 != 0 {
        return Err("writable by group or others".into());
    }
    if mode & 0o6000 != 0 {
        return Err("setuid/setgid".into());
    }
    if md.uid() != expected_uid {
        return Err(format!("owned by uid {} (expected {expected_uid})", md.uid()));
    }
    Ok(())
}

fn project_hooks_allowed(ctx: &Ctx, project: &str) -> Result<(), String> {
    match ctx.cfg.global.project_hooks.as_str() {
        "on" => Ok(()),
        "allowlist" if ctx.cfg.global.project_hooks_allow.iter().any(|p| p == project) => Ok(()),
        "allowlist" => Err("project not in global.project_hooks_allow".into()),
        _ => Err("global.project_hooks = off".into()),
    }
}

fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| !p.file_name().unwrap_or_default().to_string_lossy().starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn list(ctx: &Ctx, event: HookEvent, h: &HookCtx) -> Vec<HookListing> {
    let mut out = Vec::new();
    let root_uid = if privilege::is_root() { 0 } else { unsafe { libc::geteuid() } };
    for path in dir_entries(&ctx.cfg.global.hooks_dir.join(event.as_str())) {
        let res = check_file(&path, root_uid);
        out.push(HookListing {
            event: event.as_str().into(),
            path,
            scope: "global",
            will_run: res.is_ok(),
            skip_reason: res.err(),
        });
    }
    if let (Some(name), Some(pp), Some((uid, _))) = (h.project, h.project_path, h.owner) {
        for path in dir_entries(&pp.join(".bpm/hooks").join(event.as_str())) {
            let res = project_hooks_allowed(ctx, name).and_then(|_| check_file(&path, uid));
            out.push(HookListing {
                event: event.as_str().into(),
                path,
                scope: "project",
                will_run: res.is_ok(),
                skip_reason: res.err(),
            });
        }
    }
    out
}

fn env_for(ctx: &Ctx, event: HookEvent, h: &HookCtx, scope: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let mut put = |k: &str, v: String| {
        env.insert(k.to_string(), v);
    };
    put("BPM_EVENT", event.as_str().into());
    put("BPM_HOOK_SCOPE", scope.into());
    put("BPM_DRY_RUN", if ctx.opts.dry_run { "1" } else { "0" }.into());
    put("BPM_INVOKER", ctx.invoker.user.clone());
    put("BPM_REASON", h.reason.clone());
    put("BPM_PROJECT", h.project.unwrap_or("").into());
    put("BPM_PROJECT_PATH", h.project_path.map(|p| p.display().to_string()).unwrap_or_default());
    put("BPM_ROOT", h.root.map(|p| p.display().to_string()).unwrap_or_default());
    if let Some((id, path, kind)) = &h.snapshot {
        put("BPM_SNAPSHOT_ID", id.to_string());
        put("BPM_SNAPSHOT_PATH", path.display().to_string());
        put("BPM_SNAPSHOT_KIND", kind.to_string());
    }
    if let Some((from, to)) = h.stage {
        put("BPM_STAGE_FROM", from.to_string());
        put("BPM_STAGE_TO", to.to_string());
    }
    for (k, v) in &h.extra {
        put(k, v.clone());
    }
    env
}

fn run_one(
    ctx: &Ctx,
    path: &Path,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    run_as: Option<(u32, u32)>,
) -> Result<(), String> {
    let mut cmd = Command::new(path);
    cmd.env_clear()
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LANG", "C.UTF-8")
        .env("HOME", if run_as.is_some() { "/tmp" } else { "/root" })
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some((uid, gid)) = run_as {
        privilege::drop_to(&mut cmd, uid, gid);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(serde_json::to_string(env).unwrap_or_default().as_bytes());
    }
    let pid = child.id() as i32;
    let mut stderr = child.stderr.take();
    let mut stdout = child.stdout.take();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(e) = stderr.as_mut() {
            let _ = e.take(64 * 1024).read_to_string(&mut s);
        }
        s
    });
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = stdout.as_mut() {
            let _ = o.take(64 * 1024).read_to_string(&mut s);
        }
        s
    });
    let timeout = ctx.cfg.global.hook_timeout;
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if start.elapsed() > timeout => {
                unsafe { libc::killpg(pid, libc::SIGTERM) };
                let grace = Instant::now();
                while grace.elapsed() < Duration::from_secs(5) {
                    if let Ok(Some(_)) = child.try_wait() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                unsafe { libc::killpg(pid, libc::SIGKILL) };
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    let stderr_text = err_reader.join().unwrap_or_default();
    let stdout_text = out_reader.join().unwrap_or_default();
    if !stdout_text.trim().is_empty() {
        tracing::debug!("hook {} stdout: {}", path.display(), stdout_text.trim());
    }
    match status {
        None => Err(format!("timed out after {}", humantime::format_duration(timeout))),
        Some(st) if st.success() => Ok(()),
        Some(st) => Err(format!(
            "exit {}: {}",
            st.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            stderr_text.trim()
        )),
    }
}

/// Run hooks for `event`. A failing pre-* hook returns `BpmError::HookVeto`.
pub fn run(ctx: &Ctx, event: HookEvent, h: &HookCtx) -> Result<()> {
    if ctx.opts.no_hooks {
        return Ok(());
    }
    for l in list(ctx, event, h) {
        if !l.will_run {
            if l.scope == "global" || ctx.cfg.global.project_hooks != "off" {
                tracing::warn!("hook {} skipped: {}", l.path.display(), l.skip_reason.clone().unwrap_or_default());
            }
            continue;
        }
        let env = env_for(ctx, event, h, l.scope);
        let cwd = h.project_path.filter(|p| p.is_dir()).or(h.root).unwrap_or(Path::new("/"));
        let run_as = if l.scope == "project" { h.owner } else { None };
        tracing::debug!("running {} hook {}", event.as_str(), l.path.display());
        if let Err(detail) = run_one(ctx, &l.path, &env, cwd, run_as) {
            if event.can_veto() {
                return Err(BpmError::HookVeto { hook: l.path, detail }.into());
            }
            tracing::warn!("hook {} failed: {detail}", l.path.display());
        }
    }
    Ok(())
}
