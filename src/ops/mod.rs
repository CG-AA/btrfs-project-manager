//! One module per subcommand.

pub mod adopt;
pub mod archive;
pub mod config_cmd;
pub mod diff;
pub mod doctor;
pub mod freeze;
pub mod hold;
pub mod hooks_cmd;
pub mod list;
pub mod migrate;
pub mod recompress;
pub mod restore;
pub mod rm;
pub mod setup;
pub mod snap;
pub mod status;
pub mod tick;
pub mod wrap;

use crate::cli::Cmd;
use crate::config::EffectiveConfig;
use crate::ctx::Ctx;
use crate::error::usage;
use crate::project::{self, ProjectRef};
use crate::store::{SnapshotMeta, Unit, snapid};
use anyhow::Result;
use std::time::Duration;

pub fn dispatch(ctx: &Ctx, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Setup(a) => setup::run(ctx, a),
        Cmd::Adopt(a) => adopt::run(ctx, a),
        Cmd::Tick(a) => tick::run(ctx, a),
        Cmd::Snap(a) => snap::run(ctx, a),
        Cmd::Wrap(a) => wrap::run(ctx, a),
        Cmd::List(a) => list::run(ctx, a),
        Cmd::Status(a) => status::run(ctx, a),
        Cmd::Diff(a) => diff::run(ctx, a),
        Cmd::Restore(a) => restore::restore(ctx, a),
        Cmd::Rollback(a) => restore::rollback(ctx, a),
        Cmd::Rm(a) => rm::run(ctx, a),
        Cmd::Hold(a) => hold::run(ctx, a, true),
        Cmd::Unhold(a) => hold::run(ctx, a, false),
        Cmd::Freeze(a) => freeze::freeze(ctx, a),
        Cmd::Unfreeze(a) => freeze::unfreeze(ctx, a),
        Cmd::Recompress(a) => recompress::run(ctx, a),
        Cmd::Archive(a) => archive::archive(ctx, a),
        Cmd::Unarchive(a) => archive::unarchive(ctx, a),
        Cmd::Forget(a) => rm::forget(ctx, a),
        Cmd::Convert(a) => adopt::convert(ctx, a),
        Cmd::Config(a) => config_cmd::run(ctx, a),
        Cmd::Hooks { cmd } => hooks_cmd::run(ctx, cmd),
        Cmd::MigrateFromSnapper(a) => migrate::run(ctx, a),
        Cmd::Doctor(a) => doctor::run(ctx, a),
        Cmd::Completions { .. } => Ok(()),
    }
}

/// Resolve a project argument, defaulting to the project containing the current directory.
pub fn resolve_or_cwd(ctx: &Ctx, spec: Option<&str>) -> Result<ProjectRef> {
    match spec {
        Some(s) => project::resolve(ctx, s),
        None => {
            let cwd = std::env::current_dir()?;
            let (root, name) = project::locate_path(ctx, &cwd)
                .ok_or_else(|| usage("not inside a managed project; name one explicitly"))?;
            project::resolve_in(ctx, &root, &name)
        }
    }
}

pub fn effective(ctx: &Ctx, pref: &ProjectRef) -> Result<EffectiveConfig> {
    project::effective(ctx, &pref.root, pref.name(), pref.path())
}

/// Resolve a snapshot selector for `unit`; `held` means the frozen project's last good snapshot.
pub fn select<'a>(ctx: &Ctx, unit: &Unit, snaps: &'a [SnapshotMeta], sel: &str) -> Result<&'a SnapshotMeta> {
    let frozen_ref = unit.read_state().ok().and_then(|st| st.frozen).and_then(|f| f.ref_snap);
    snapid::resolve_str(sel, snaps, &ctx.tz, frozen_ref)
}

pub fn lock_timeout(ctx: &Ctx, explicit: Option<Duration>) -> Duration {
    explicit.unwrap_or(ctx.cfg.global.lock_timeout)
}
