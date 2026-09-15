//! Command-line interface.

use clap::{ArgAction, Args, Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

fn dur(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| e.to_string())
}

#[derive(Parser, Debug)]
#[command(
    name = "bpm",
    version,
    about = "btrfs project manager: per-project subvolumes, change-driven snapshots, idle lifecycle",
    long_about = "bpm keeps every top-level project directory of a root (like /space) in its own btrfs subvolume, \
keeps build directories out of snapshots as nested subvolumes, snapshots projects when they change, \
freezes cleanup when a project suddenly loses content, and thins, collapses and recompresses idle projects."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Args, Debug, Clone)]
pub struct GlobalArgs {
    /// Machine-readable JSON on stdout
    #[arg(long, global = true)]
    pub json: bool,
    /// Show what would change without changing anything
    #[arg(long, global = true)]
    pub dry_run: bool,
    /// More logging (-v debug, -vv trace)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,
    /// Only warnings and errors
    #[arg(short, long, global = true)]
    pub quiet: bool,
    /// Never re-exec through sudo
    #[arg(long, global = true)]
    pub no_sudo: bool,
    /// Do not run user hooks
    #[arg(long, global = true)]
    pub no_hooks: bool,
    /// Config file (default /etc/bpm/config.toml, or $BPM_CONFIG)
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Restrict to one root
    #[arg(long, global = true, value_name = "DIR")]
    pub root: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Install config, create the store subvolume, install and enable the systemd timer
    #[command(alias = "init")]
    Setup(SetupArgs),
    /// Convert plain directories into project subvolumes and take the first snapshot
    Adopt(AdoptArgs),
    /// Scheduled run: discover, enforce banlists, snapshot changed projects, thin, lifecycle
    Tick(TickArgs),
    /// Take a snapshot now
    Snap(SnapArgs),
    /// Snapshot before and after running a command
    Wrap(WrapArgs),
    /// List snapshots of a project
    #[command(alias = "ls")]
    List(ListArgs),
    /// Overview of all projects, or details of one
    Status(StatusArgs),
    /// Compare two snapshots, or a snapshot with the live tree
    Diff(DiffArgs),
    /// Copy files back from a snapshot, or recreate a deleted project
    Restore(RestoreArgs),
    /// Replace a whole project with a snapshot (the current state is snapshotted first)
    Rollback(RollbackArgs),
    /// Delete snapshots
    #[command(alias = "delete")]
    Rm(RmArgs),
    /// Protect snapshots from any automatic deletion
    Hold(HoldArgs),
    /// Remove the protection added by `hold`
    Unhold(HoldArgs),
    /// Stop all automatic snapshot deletion for a project
    Freeze(FreezeArgs),
    /// Accept the current state after a freeze and resume cleanup
    Unfreeze(UnfreezeArgs),
    /// Rewrite a project's live tree with stronger zstd compression
    Recompress(RecompressArgs),
    /// Write a snapshot as a zstd-compressed btrfs send stream
    Archive(ArchiveArgs),
    /// Receive an archive back into the store (and recreate the project)
    Unarchive(UnarchiveArgs),
    /// Remove a project from the store (optionally deleting its snapshots)
    Forget(ForgetArgs),
    /// Convert a plain banned directory into a nested subvolume now
    Convert(ConvertArgs),
    /// Show merged configuration
    Config(ConfigArgs),
    /// List or run hooks
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },
    /// Hand /space over from snapper to bpm
    MigrateFromSnapper(MigrateArgs),
    /// Check for problems and optionally repair them
    Doctor(DoctorArgs),
    /// Print shell completions
    Completions { shell: clap_complete::Shell },
}

impl Cmd {
    pub fn needs_root(&self) -> bool {
        match self {
            Cmd::List(_) | Cmd::Diff(_) | Cmd::Config(_) | Cmd::Completions { .. } | Cmd::Wrap(_) => false,
            Cmd::Status(a) => a.du,
            Cmd::Doctor(a) => a.fix,
            Cmd::Hooks { cmd } => matches!(cmd, HooksCmd::Run(_)),
            Cmd::Snap(a) => !a.claude_hook,
            Cmd::Setup(a) => !a.print_claude_hook,
            _ => true,
        }
    }
}

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Do not install systemd units
    #[arg(long)]
    pub no_units: bool,
    /// Install units but do not enable the timer
    #[arg(long)]
    pub no_enable: bool,
    /// Overwrite an existing /etc/bpm/config.toml with the default
    #[arg(long)]
    pub force_config: bool,
    /// Print the Claude Code PreToolUse hook snippet and exit
    #[arg(long)]
    pub print_claude_hook: bool,
    /// Binary path used in the systemd unit
    #[arg(long, default_value = "/usr/local/bin/bpm")]
    pub bin: PathBuf,
}

#[derive(Args, Debug)]
pub struct AdoptArgs {
    /// Project directory names under the root
    pub projects: Vec<String>,
    /// Adopt every unadopted directory
    #[arg(long)]
    pub all: bool,
    /// Start banned directories empty instead of copying their contents
    #[arg(long)]
    pub no_keep_build: bool,
    /// Proceed even if files are open for writing
    #[arg(long)]
    pub force: bool,
    /// Also compare the full path listing after copying
    #[arg(long)]
    pub verify_paths: bool,
}

#[derive(Args, Debug)]
pub struct TickArgs {
    /// Only these projects
    #[arg(long = "project")]
    pub projects: Vec<String>,
    /// Skip heavy operations (adopt, convert, collapse, recompress)
    #[arg(long, conflicts_with = "heavy_only")]
    pub no_heavy: bool,
    /// Only run the heavy operation this root needs next (the separate bpm-heavy service)
    #[arg(long)]
    pub heavy_only: bool,
    /// Wait this long for another tick to finish
    #[arg(long, value_parser = dur, default_value = "0s")]
    pub lock_timeout: Duration,
}

#[derive(Args, Debug)]
pub struct SnapArgs {
    /// Project name or path (default: the project containing the current directory)
    pub project: Option<String>,
    /// Snapshot every managed project
    #[arg(long)]
    pub all: bool,
    /// Snapshot the root container (loose files of the root)
    #[arg(long)]
    pub container: bool,
    #[arg(long, default_value = "")]
    pub reason: String,
    #[arg(long, default_value = "manual")]
    pub kind: String,
    /// Skip if the newest snapshot is younger than this
    #[arg(long, value_parser = dur)]
    pub throttle: Option<Duration>,
    /// Skip if nothing changed since the newest snapshot
    #[arg(long)]
    pub if_changed: bool,
    /// Skip the file-count walk (no shrink-guard data for this snapshot)
    #[arg(long)]
    pub quick: bool,
    /// Hold the new snapshot
    #[arg(long)]
    pub hold: bool,
    /// Pair this snapshot with another (pre/post)
    #[arg(long)]
    pub pair: Option<u64>,
    #[arg(long, value_parser = dur)]
    pub lock_timeout: Option<Duration>,
    /// Claude Code PreToolUse mode: read the hook JSON on stdin, never fail
    #[arg(long)]
    pub claude_hook: bool,
    /// Always exit 0 (errors go to stderr)
    #[arg(long, hide = true)]
    pub never_fail: bool,
}

#[derive(Args, Debug)]
pub struct WrapArgs {
    /// Project name or path (default: current directory)
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long, default_value = "")]
    pub reason: String,
    /// Command to run
    #[arg(trailing_var_arg = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    pub project: Option<String>,
    #[arg(long)]
    pub container: bool,
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub held: bool,
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    pub project: Option<String>,
    /// Also list unadopted directories and other entries
    #[arg(long)]
    pub unadopted: bool,
    /// Include `btrfs filesystem du` sizes (slow, root)
    #[arg(long)]
    pub du: bool,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    pub project: String,
    /// Older side: snapshot selector
    pub from: String,
    /// Newer side: snapshot selector or `live` (default)
    pub to: Option<String>,
    #[arg(long)]
    pub stat: bool,
    #[arg(long)]
    pub name_only: bool,
    /// Only paths under this relative path
    #[arg(long)]
    pub path: Option<String>,
}

#[derive(Args, Debug)]
pub struct RestoreArgs {
    /// Project name or path
    pub project: String,
    /// Snapshot selector (id, latest, latest~N, held, YYYY-MM-DD[THH:MM])
    pub snapshot: Option<String>,
    /// Paths relative to the project root (or absolute inside it)
    pub paths: Vec<String>,
    /// Copy into this directory instead of the live project
    #[arg(long)]
    pub to: Option<PathBuf>,
    /// Replace existing files (a pre-restore snapshot is taken first)
    #[arg(long)]
    pub overwrite: bool,
    /// Recreate the whole project directory from the snapshot (the directory must be gone)
    #[arg(long)]
    pub recreate: bool,
}

#[derive(Args, Debug)]
pub struct RollbackArgs {
    pub project: String,
    pub snapshot: String,
    /// Delete the current build directories instead of keeping them
    #[arg(long)]
    pub drop_build_dirs: bool,
    /// Proceed even if files are open for writing
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct RmArgs {
    /// Project name or path (with --container: the first snapshot)
    pub project: Option<String>,
    /// Snapshot selectors
    pub snapshots: Vec<String>,
    #[arg(long)]
    pub force_held: bool,
    /// Delete snapshots of the root container; every argument is a snapshot
    #[arg(long)]
    pub container: bool,
}

#[derive(Args, Debug)]
pub struct HoldArgs {
    pub project: String,
    #[arg(required = true)]
    pub snapshots: Vec<String>,
    #[arg(long, default_value = "")]
    pub note: String,
}

#[derive(Args, Debug)]
pub struct FreezeArgs {
    pub project: String,
    #[arg(long, default_value = "frozen manually")]
    pub reason: String,
}

#[derive(Args, Debug)]
pub struct UnfreezeArgs {
    pub project: String,
    /// Also release snapshots that were auto-held by the shrink guard
    #[arg(long)]
    pub release_holds: bool,
}

#[derive(Args, Debug)]
pub struct RecompressArgs {
    pub projects: Vec<String>,
    #[arg(long)]
    pub level: Option<u8>,
    /// Every cold project that needs it
    #[arg(long)]
    pub all_cold: bool,
    /// Skip the compressibility sample
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct ArchiveArgs {
    pub project: String,
    /// Snapshot to archive (default: collapse and archive the current state)
    #[arg(long)]
    pub snapshot: Option<String>,
    /// Archive directory (default global.archive_dir)
    #[arg(long)]
    pub dest: Option<PathBuf>,
    #[arg(long)]
    pub level: Option<u8>,
    /// send --compressed-data with fast zstd
    #[arg(long)]
    pub fast: bool,
    /// Delete the project's other snapshots afterwards (held ones stay)
    #[arg(long)]
    pub delete_snapshots: bool,
    /// Delete the live project afterwards (stage becomes archived)
    #[arg(long)]
    pub delete_live: bool,
    /// Confirm destructive options
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct UnarchiveArgs {
    pub project: String,
    /// Archive file (default: newest for the project)
    pub file: Option<PathBuf>,
    /// Only receive into the store; do not recreate the live project
    #[arg(long)]
    pub no_live: bool,
}

#[derive(Args, Debug)]
pub struct ForgetArgs {
    pub project: String,
    #[arg(long)]
    pub delete_snapshots: bool,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct ConvertArgs {
    pub project: String,
    /// Banned path relative to the project
    pub path: String,
    #[arg(long)]
    pub discard_contents: bool,
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    pub project: Option<String>,
    #[arg(long)]
    pub show_origin: bool,
    /// Print the default config file
    #[arg(long)]
    pub default: bool,
}

#[derive(Subcommand, Debug)]
pub enum HooksCmd {
    /// Show hook files and whether they would run
    List {
        #[arg(long)]
        project: Option<String>,
    },
    /// Run hooks for an event now
    Run(HookRunArgs),
}

#[derive(Args, Debug)]
pub struct HookRunArgs {
    pub event: String,
    pub project: String,
}

#[derive(Args, Debug)]
pub struct MigrateArgs {
    #[arg(long, default_value = "space")]
    pub snapper_config: String,
    /// Adopt every project now (otherwise tick adopts them gradually)
    #[arg(long)]
    pub adopt_all: bool,
    /// Import snapper snapshots into the container store as held snapshots
    #[arg(long)]
    pub import: bool,
    /// Delete snapper's snapshots and config for this root at the end
    #[arg(long)]
    pub delete_snapper: bool,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct DoctorArgs {
    #[arg(long)]
    pub fix: bool,
}
