//! `bpm hooks list|run`.

use crate::cli::HooksCmd;
use crate::ctx::Ctx;
use crate::error::usage;
use crate::hooks::{self, ALL_EVENTS, HookCtx, HookEvent};
use crate::output::{Table, emit};
use crate::project;
use anyhow::Result;

pub fn run(ctx: &Ctx, cmd: HooksCmd) -> Result<()> {
    match cmd {
        HooksCmd::List { project } => {
            let pref = project.as_deref().map(|p| project::resolve(ctx, p)).transpose()?;
            let mut all = Vec::new();
            for ev in ALL_EVENTS {
                let h = HookCtx {
                    project: pref.as_ref().map(|p| p.name()),
                    project_path: pref.as_ref().map(|p| p.path()),
                    owner: pref.as_ref().map(|p| (p.record.owner_uid, p.record.owner_gid)),
                    ..Default::default()
                };
                all.extend(hooks::list(ctx, *ev, &h));
            }
            emit(ctx, &all, || {
                let mut t = Table::new(&["EVENT", "SCOPE", "RUNS", "PATH"]);
                for l in &all {
                    t.row(vec![
                        l.event.clone(),
                        l.scope.into(),
                        if l.will_run {
                            "yes".into()
                        } else {
                            format!("no: {}", l.skip_reason.clone().unwrap_or_default())
                        },
                        l.path.display().to_string(),
                    ]);
                }
                if t.is_empty() {
                    format!(
                        "no hooks installed (global dir {}; events: {})",
                        ctx.cfg.global.hooks_dir.display(),
                        ALL_EVENTS.iter().map(|e| e.as_str()).collect::<Vec<_>>().join(", ")
                    )
                } else {
                    t.render()
                }
            });
            Ok(())
        }
        HooksCmd::Run(a) => {
            let ev = HookEvent::parse(&a.event).ok_or_else(|| usage(format!("unknown event {:?}", a.event)))?;
            let pref = project::resolve(ctx, &a.project)?;
            let h = HookCtx {
                project: Some(pref.name()),
                project_path: Some(pref.path()),
                owner: Some((pref.record.owner_uid, pref.record.owner_gid)),
                root: Some(&pref.root.path),
                reason: "bpm hooks run".into(),
                ..Default::default()
            };
            hooks::run(ctx, ev, &h)?;
            emit(ctx, &serde_json::json!({"event": ev.as_str(), "ok": true}), || {
                format!("{} hooks for {} ran successfully", ev.as_str(), pref.name())
            });
            Ok(())
        }
    }
}
