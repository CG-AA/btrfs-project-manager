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

/// `bpm <args…> --no-sudo [--config FILE]`, through `sudo -n` unless already root or `no_sudo`.
/// The subcommand stays the first argument, so a sudoers rule such as
/// `/usr/local/bin/bpm snap *` matches; bpm's global flags are accepted after it.
pub fn self_command(args: &[OsString], config: Option<&Path>, via_sudo: bool) -> std::io::Result<Command> {
    let exe = std::env::current_exe()?;
    let mut cmd = if via_sudo {
        let mut c = Command::new("sudo");
        c.arg("-n").arg("--").arg(exe);
        c
    } else {
        Command::new(exe)
    };
    // flags must not end up after a `--` (they would become positional arguments)
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    cmd.args(&args[..split]).arg("--no-sudo");
    let has_config = args.iter().any(|a| a == "--config" || a.to_string_lossy().starts_with("--config="));
    if let (Some(c), false) = (config, has_config) {
        cmd.arg("--config").arg(c);
    }
    cmd.args(&args[split..]);
    Ok(cmd)
}

/// Replace this process with `sudo -n -- <exe> <args…> --no-sudo`. Returns only on failure.
pub fn reexec_with_sudo(args: &[OsString], config: Option<&Path>) -> anyhow::Error {
    let mut cmd = match self_command(args, config, true) {
        Ok(c) => c,
        Err(e) => return e.into(),
    };
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
///
/// One `pre_exec` closure drops everything in the only working order: supplementary groups and
/// the group need privileges that are gone once the uid changes. (std runs its own uid/gid
/// changes before `pre_exec` closures, so mixing `Command::uid` with a `setgroups` closure fails.)
pub fn drop_to(cmd: &mut Command, uid: u32, gid: u32) {
    if !is_root() {
        return;
    }
    unsafe {
        cmd.pre_exec(move || {
            if libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn self_command_keeps_the_subcommand_first() {
        let args: Vec<OsString> = ["snap", "demo", "--kind", "hook"].iter().map(Into::into).collect();
        let c = self_command(&args, Some(Path::new("/etc/bpm/x.toml")), true).unwrap();
        let got: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(got[..3], ["-n", "--", std::env::current_exe().unwrap().to_string_lossy().as_ref()]);
        assert_eq!(got[3..], ["snap", "demo", "--kind", "hook", "--no-sudo", "--config", "/etc/bpm/x.toml"]);
        let args: Vec<OsString> = ["restore", "p", "1", "--", "-odd"].iter().map(Into::into).collect();
        let got: Vec<String> =
            self_command(&args, None, false).unwrap().get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(got, ["restore", "p", "1", "--no-sudo", "--", "-odd"]);
    }
}
