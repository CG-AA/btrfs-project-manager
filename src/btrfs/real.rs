//! Production backend: ioctls for reads, the `btrfs` CLI for mutations.

use super::{Btrfs, DefragOpts, DuStats, FsUsage, SubvolInfo, Uuid, ioctl};
use anyhow::{Context, Result, bail};
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

pub struct RealBtrfs {
    pub bin: PathBuf,
}

impl Default for RealBtrfs {
    fn default() -> Self {
        RealBtrfs { bin: PathBuf::from("btrfs") }
    }
}

impl RealBtrfs {
    fn run(&self, args: &[&std::ffi::OsStr]) -> Result<String> {
        let start = Instant::now();
        let out = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("spawn {}", self.bin.display()))?;
        let argv = args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ");
        tracing::debug!(ms = start.elapsed().as_millis() as u64, status = ?out.status, "btrfs {argv}");
        if !out.status.success() {
            bail!("btrfs {argv}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

fn os(s: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(s)
}

impl Btrfs for RealBtrfs {
    fn kind(&self) -> &'static str {
        "real"
    }

    fn subvol_info(&self, path: &Path) -> Result<SubvolInfo> {
        ioctl::get_subvol_info(path)
    }

    fn is_subvolume(&self, path: &Path) -> Result<bool> {
        let md = std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
        Ok(md.is_dir() && md.ino() == 256)
    }

    fn statfs(&self, path: &Path) -> Result<FsUsage> {
        let c = CString::new(path.as_os_str().as_bytes())?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            bail!("statvfs {}: {}", path.display(), std::io::Error::last_os_error());
        }
        Ok(FsUsage { total: s.f_blocks * s.f_frsize, free: s.f_bavail * s.f_frsize })
    }

    fn sync(&self, path: &Path) -> Result<()> {
        let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        if unsafe { libc::syncfs(f.as_raw_fd()) } != 0 {
            bail!("syncfs {}: {}", path.display(), std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn create_subvolume(&self, path: &Path) -> Result<()> {
        self.run(&[os("subvolume"), os("create"), path.as_os_str()]).map(|_| ())
    }

    fn snapshot(&self, src: &Path, dst: &Path, readonly: bool) -> Result<()> {
        let mut args = vec![os("subvolume"), os("snapshot")];
        if readonly {
            args.push(os("-r"));
        }
        args.push(src.as_os_str());
        args.push(dst.as_os_str());
        self.run(&args).map(|_| ())
    }

    fn delete_subvolume(&self, path: &Path, recursive: bool) -> Result<()> {
        let mut args = vec![os("subvolume"), os("delete")];
        if recursive {
            args.push(os("-R"));
        }
        args.push(path.as_os_str());
        self.run(&args).map(|_| ())
    }

    fn set_readonly(&self, path: &Path, ro: bool) -> Result<()> {
        self.run(&[
            os("property"),
            os("set"),
            os("-f"),
            os("-ts"),
            path.as_os_str(),
            os("ro"),
            os(if ro { "true" } else { "false" }),
        ])
        .map(|_| ())
    }

    fn defragment(&self, path: &Path, opts: &DefragOpts) -> Result<()> {
        let c = format!("-c{}", opts.compress);
        let level = opts.level.map(|l| l.to_string());
        let mut args = vec![os("filesystem"), os("defragment"), os("-r"), os(&c)];
        if let Some(l) = &level {
            args.push(os("-L"));
            args.push(os(l));
        }
        if opts.flush {
            args.push(os("-f"));
        }
        args.push(path.as_os_str());
        self.run(&args).map(|_| ())
    }

    fn du(&self, path: &Path) -> Result<DuStats> {
        let out = self.run(&[os("filesystem"), os("du"), os("-s"), os("--raw"), path.as_os_str()])?;
        parse_du(&out).with_context(|| format!("parse `btrfs fi du` output for {}", path.display()))
    }

    fn send(&self, snapshot: &Path, compressed_data: bool, sink: &mut dyn Write) -> Result<u64> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(["send", "-q"]);
        if compressed_data {
            cmd.arg("--compressed-data");
        }
        let mut child = cmd
            .arg(snapshot)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn btrfs send")?;
        let mut stdout = child.stdout.take().unwrap();
        let n = std::io::copy(&mut stdout, sink)?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("btrfs send {}: {}", snapshot.display(), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(n)
    }

    fn receive(&self, source: &mut dyn Read, dest_dir: &Path) -> Result<()> {
        let mut child = Command::new(&self.bin)
            .args(["receive", "-q"])
            .arg(dest_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn btrfs receive")?;
        {
            let mut stdin = child.stdin.take().unwrap();
            std::io::copy(source, &mut stdin)?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("btrfs receive {}: {}", dest_dir.display(), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    fn find_by_uuid(&self, fs_path: &Path, uuid: Uuid) -> Result<Option<PathBuf>> {
        let out = self.run(&[os("subvolume"), os("list"), os("-u"), os("-a"), fs_path.as_os_str()])?;
        let want = uuid.to_string();
        Ok(out.lines().find_map(|l| {
            let mut it = l.split_whitespace();
            let mut found = false;
            let mut path = None;
            while let Some(tok) = it.next() {
                match tok {
                    "uuid" => found = it.next() == Some(want.as_str()),
                    "path" => path = it.next().map(PathBuf::from),
                    _ => {}
                }
            }
            if found { path } else { None }
        }))
    }
}

pub fn parse_du(out: &str) -> Result<DuStats> {
    let line = out
        .lines()
        .find(|l| l.trim_start().chars().next().is_some_and(|c| c.is_ascii_digit()))
        .context("no data line")?;
    let nums: Vec<u64> = line.split_whitespace().take(3).map(|t| t.parse().unwrap_or(0)).collect();
    Ok(DuStats {
        total: nums.first().copied().unwrap_or(0),
        exclusive: nums.get(1).copied().unwrap_or(0),
        set_shared: nums.get(2).copied().unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn du_parsing() {
        let s = "     Total   Exclusive  Set shared  Filename\n2147483648           0  1556925644  /space/slime_os\n";
        let d = parse_du(s).unwrap();
        assert_eq!((d.total, d.exclusive, d.set_shared), (2147483648, 0, 1556925644));
        let s2 = "     Total   Exclusive  Set shared  Filename\n  4096   4096   -  /x\n";
        assert_eq!(parse_du(s2).unwrap().set_shared, 0);
    }
}
