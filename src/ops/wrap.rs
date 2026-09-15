//! `bpm wrap -- cmd…`: pre snapshot, run the command as the calling user, post snapshot.

use crate::cli::WrapArgs;
use crate::ctx::Ctx;
use anyhow::{Context, Result, bail};
use std::process::{Command, Stdio};

fn spawn_snap(
    ctx: &Ctx,
    root: &std::path::Path,
    name: &str,
    kind: &str,
    reason: &str,
    pair: Option<u64>,
) -> Result<u64> {
    let config = ctx.opts.config.clone().or_else(|| std::env::var_os("BPM_CONFIG").map(Into::into));
    let mut args: Vec<std::ffi::OsString> =
        ["snap", name, "--kind", kind, "--reason", reason, "--json", "--root"].iter().map(Into::into).collect();
    args.push(root.into());
    if let Some(p) = pair {
        args.push("--pair".into());
        args.push(p.to_string().into());
    }
    let via_sudo = !(crate::privilege::is_root() || ctx.opts.no_sudo);
    let mut cmd = crate::privilege::self_command(&args, config.as_deref(), via_sudo)?;
    let out = cmd.stdin(Stdio::null()).stderr(Stdio::inherit()).output().context("spawn bpm snap")?;
    if !out.status.success() {
        bail!("bpm snap --kind {kind} failed");
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("parse bpm snap output")?;
    v[0]["id"].as_u64().context("bpm snap returned no id")
}

pub fn run(ctx: &Ctx, a: WrapArgs) -> Result<()> {
    let pref = super::resolve_or_cwd(ctx, a.project.as_deref())?;
    let reason = if a.reason.is_empty() { a.command.join(" ").chars().take(200).collect() } else { a.reason.clone() };
    if ctx.opts.dry_run {
        eprintln!(
            "[dry-run] {}: would take a pre snapshot, run `{}`, then take a post snapshot",
            pref.name(),
            a.command.join(" ")
        );
        return Ok(());
    }
    let pre = spawn_snap(ctx, &pref.root.path, pref.name(), "pre", &reason, None)?;
    if !ctx.opts.quiet {
        eprintln!("bpm: {}: pre snapshot #{pre}", pref.name());
    }
    let status =
        Command::new(&a.command[0]).args(&a.command[1..]).status().with_context(|| format!("run {}", a.command[0]))?;
    let post = spawn_snap(ctx, &pref.root.path, pref.name(), "post", &reason, Some(pre))?;
    if !ctx.opts.quiet {
        eprintln!("bpm: {}: post snapshot #{post} (compare with `bpm diff {} {pre} {post}`)", pref.name(), pref.name());
    }
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
