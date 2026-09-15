use anyhow::Result;
use bpm::btrfs::{Btrfs, DryRunBtrfs, real::RealBtrfs};
use bpm::cli::{Cli, Cmd};
use bpm::clock::SystemClock;
use bpm::ctx::{Ctx, Invoker, Opts};
use bpm::util::fs::{DryRunFs, Fs, RealFs, ReflinkMode};
use bpm::{config, error, ops, output, privilege};
use clap::{CommandFactory, Parser};
use std::sync::Arc;

fn run(cli: Cli) -> Result<()> {
    if let Cmd::Completions { shell } = &cli.cmd {
        clap_complete::generate(*shell, &mut Cli::command(), "bpm", &mut std::io::stdout());
        return Ok(());
    }
    let loaded = config::load(cli.global.config.as_deref())?;
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let pass_config = cli.global.config.clone().or_else(|| std::env::var_os("BPM_CONFIG").map(Into::into));
    privilege::require_root_or_exec(
        cli.cmd.needs_root(),
        cli.global.no_sudo,
        loaded.config.global.sudo,
        &raw,
        pass_config.as_deref(),
    )?;
    let real: Arc<dyn Btrfs> = Arc::new(RealBtrfs::default());
    let btrfs: Arc<dyn Btrfs> = if cli.global.dry_run { Arc::new(DryRunBtrfs(real)) } else { real };
    let fs: Arc<dyn Fs> = if cli.global.dry_run { Arc::new(DryRunFs) } else { Arc::new(RealFs) };
    let g = cli.global;
    let ctx = Ctx {
        cfg: loaded.config,
        cfg_path: loaded.path,
        btrfs,
        fs,
        clock: Arc::new(SystemClock),
        tz: jiff::tz::TimeZone::system(),
        opts: Opts {
            json: g.json,
            dry_run: g.dry_run,
            verbose: g.verbose,
            quiet: g.quiet,
            no_sudo: g.no_sudo,
            no_hooks: g.no_hooks,
            root: g.root,
            config: g.config,
        },
        invoker: Invoker::detect(),
        reflink: ReflinkMode::Always,
    };
    ops::dispatch(&ctx, cli.cmd)
}

fn main() {
    let cli = Cli::parse();
    output::init_logging(cli.global.verbose, cli.global.quiet);
    if let Err(e) = run(cli) {
        let code = error::exit_code_for(&e);
        tracing::error!("{e:#}");
        std::process::exit(code);
    }
}
