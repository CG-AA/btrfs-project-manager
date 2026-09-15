//! `bpm recompress`.

use crate::cli::RecompressArgs;
use crate::ctx::Ctx;
use crate::error::{BpmError, usage};
use crate::mechanics::recompress;
use crate::output::emit;
use crate::project::{self, ProjectRef};
use crate::store::Stage;
use anyhow::Result;

pub fn run(ctx: &Ctx, a: RecompressArgs) -> Result<()> {
    let mut targets: Vec<ProjectRef> = a.projects.iter().map(|p| project::resolve(ctx, p)).collect::<Result<_>>()?;
    if a.all_cold {
        for root in ctx.roots() {
            let store = ctx.store(&root);
            for (unit, record) in store.units()? {
                if unit.read_state()?.stage == Stage::Cold {
                    targets.push(ProjectRef { root: root.clone(), store: store.clone(), unit, record });
                }
            }
        }
    }
    if targets.is_empty() {
        return Err(usage("name projects or pass --all-cold"));
    }
    let mut reports = Vec::new();
    let mut failed = 0;
    for pref in targets {
        let eff = super::effective(ctx, &pref)?;
        let lu = pref.unit.lock(ctx.cfg.global.lock_timeout)?;
        let mut st = pref.unit.read_state()?;
        let mut snaps = pref.unit.snapshots()?;
        let level = a.level.unwrap_or(eff.policy.recompress.level);
        let r = recompress::recompress(ctx, &lu, &pref, &eff, &mut st, &mut snaps, level, a.force);
        lu.write_state(&st)?;
        match r {
            Ok(r) => reports.push((pref.name().to_string(), r)),
            Err(e) => {
                failed += 1;
                tracing::error!("{}: {e:#}", pref.name());
            }
        }
    }
    emit(ctx, &reports, || {
        reports
            .iter()
            .map(|(n, r)| {
                if r.done {
                    format!("{n}: recompressed at zstd:{} (snapshot #{})", r.level, r.new_snapshot.unwrap_or(0))
                } else {
                    format!("{n}: skipped: {}", r.skipped.clone().unwrap_or_default())
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    if failed > 0 {
        return Err(BpmError::Partial { failed }.into());
    }
    Ok(())
}
