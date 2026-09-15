//! Root handling: sudo re-exec for mutating commands, privilege drop for project hooks.

use crate::error::BpmError;
use anyhow::Result;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Replace this process with `sudo -n -- <exe> --no-sudo <args…>`. Returns only on failure.
pub fn reexec_with_sudo(args: &[OsString], config: Option<&Path>) -> anyhow::Error {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return e.into(),
    };
    let mut cmd = Command::new("sudo");
    cmd.arg("-n").arg("--").arg(exe).arg("--no-sudo");
    let has_config = args.iter().any(|a| a == "--config" || a.to_string_lossy().starts_with("--config="));
    if let (Some(c), false) = (config, has_config) {
        cmd.arg("--config").arg(c);
    }
    cmd.args(args);
    let err = cmd.exec();
    BpmError::NeedsRoot(format!("passwordless `sudo -n` failed ({err}); run with sudo, or set global.sudo = false"))
        .into()
}

pub fn require_root_or_exec(
    needs_root: bool,
    no_sudo: bool,
    use_sudo: bool,
    args: &[OsString],
    config: Option<&Path>,
) -> Result<()> {
    if !needs_root || is_root() || no_sudo {
        return Ok(());
    }
    if !use_sudo {
        return Err(BpmError::NeedsRoot("run with sudo".into()).into());
    }
    Err(reexec_with_sudo(args, config))
}

/// Configure `cmd` to run as `uid:gid` with no supplementary groups (requires root).
pub fn drop_to(cmd: &mut Command, uid: u32, gid: u32) {
    if !is_root() {
        return;
    }
    unsafe {
        cmd.pre_exec(|| {
            if libc::setgroups(0, std::ptr::null()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.gid(gid).uid(uid);
}
