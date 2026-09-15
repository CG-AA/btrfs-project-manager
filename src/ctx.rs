//! Runtime context shared by every command.

use crate::btrfs::Btrfs;
use crate::clock::Clock;
use crate::config::{Config, RootCfg};
use crate::store::{Origin, Store};
use crate::util::fs::ReflinkMode;
use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct Opts {
    pub json: bool,
    pub dry_run: bool,
    pub verbose: u8,
    pub quiet: bool,
    pub no_sudo: bool,
    pub no_hooks: bool,
    pub root: Option<PathBuf>,
    pub config: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Invoker {
    pub uid: u32,
    pub gid: u32,
    pub user: String,
    pub argv: String,
}

impl Invoker {
    pub fn detect() -> Invoker {
        let euid = unsafe { libc::geteuid() };
        let env_u32 = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok());
        let (uid, gid, user) = if euid == 0 && env_u32("SUDO_UID").is_some() {
            (
                env_u32("SUDO_UID").unwrap(),
                env_u32("SUDO_GID").unwrap_or(0),
                std::env::var("SUDO_USER").unwrap_or_default(),
            )
        } else {
            (
                unsafe { libc::getuid() },
                unsafe { libc::getgid() },
                std::env::var("USER").unwrap_or_else(|_| if euid == 0 { "root".into() } else { String::new() }),
            )
        };
        Invoker { uid, gid, user, argv: std::env::args().collect::<Vec<_>>().join(" ") }
    }
}

pub struct Ctx {
    pub cfg: Config,
    pub cfg_path: Option<PathBuf>,
    pub btrfs: Arc<dyn Btrfs>,
    pub clock: Arc<dyn Clock>,
    pub tz: TimeZone,
    pub opts: Opts,
    pub invoker: Invoker,
    pub reflink: ReflinkMode,
}

impl Ctx {
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    pub fn is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    /// Configured roots, or just the one selected with `--root`.
    pub fn roots(&self) -> Vec<RootCfg> {
        match &self.opts.root {
            Some(p) => vec![self.cfg.root_for(p).cloned().unwrap_or_else(|| RootCfg::adhoc(p.clone()))],
            None => self.cfg.roots.clone(),
        }
    }

    pub fn store(&self, root: &RootCfg) -> Store {
        Store::new(&root.path, &self.cfg.global.store_dir, self.opts.dry_run)
    }

    pub fn is_subvol(&self, path: &Path, ino: u64) -> bool {
        self.btrfs.probe_subvolume(path, ino)
    }

    pub fn origin(&self) -> Origin {
        Origin {
            bpm: env!("CARGO_PKG_VERSION").into(),
            user: self.invoker.user.clone(),
            argv: self.invoker.argv.clone(),
        }
    }

    pub fn free_bytes(&self, path: &Path) -> Result<u64> {
        Ok(self.btrfs.statfs(path)?.free)
    }
}
