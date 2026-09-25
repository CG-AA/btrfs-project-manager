//! Filesystem primitives: atomic writes, renames that never copy, reflink copies, ownership.

use anyhow::{Context, Result, bail};
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Destructive operations on user trees and store directories. `--dry-run` swaps in `DryRunFs`,
/// so every caller (including `doctor --fix` closures) is dry-run safe without its own check.
pub trait Fs: Send + Sync {
    fn remove_dir_all(&self, path: &Path) -> Result<()>;
    fn remove_dir(&self, path: &Path) -> Result<()>;
    fn remove_file(&self, path: &Path) -> Result<()>;
    /// rename(2) only: fails with EXDEV instead of copying.
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    fn rename_exchange(&self, a: &Path, b: &Path) -> Result<()>;
}

pub struct RealFs;

impl Fs for RealFs {
    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
    }
    fn remove_dir(&self, path: &Path) -> Result<()> {
        fs::remove_dir(path).with_context(|| format!("remove {}", path.display()))
    }
    fn remove_file(&self, path: &Path) -> Result<()> {
        fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        rename_strict(from, to)
    }
    fn rename_exchange(&self, a: &Path, b: &Path) -> Result<()> {
        rename_exchange(a, b)
    }
}

pub struct DryRunFs;

impl Fs for DryRunFs {
    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        tracing::info!("[dry-run] remove {}", path.display());
        Ok(())
    }
    fn remove_dir(&self, path: &Path) -> Result<()> {
        tracing::info!("[dry-run] rmdir {}", path.display());
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> Result<()> {
        tracing::info!("[dry-run] remove {}", path.display());
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        tracing::info!("[dry-run] rename {} -> {}", from.display(), to.display());
        Ok(())
    }
    fn rename_exchange(&self, a: &Path, b: &Path) -> Result<()> {
        tracing::info!("[dry-run] swap {} <-> {}", a.display(), b.display());
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReflinkMode {
    /// Production: fail rather than duplicate data.
    Always,
    /// Tests on filesystems without reflink support.
    Auto,
}

/// Write `bytes` to `path` via a temp file in the same directory, fsync, rename, fsync dir.
/// A failed write (ENOSPC) removes its temp file, so it cannot keep a store directory non-empty.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().context("atomic write target has no parent")?;
    let tmp = dir.join(format!(".{}.tmp.{}", path.file_name().unwrap().to_string_lossy(), std::process::id()));
    let written = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes).with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all().with_context(|| format!("fsync {}", tmp.display()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written?;
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Is `name` a `write_atomic` temp file (`.<target>.tmp.<pid>`), for example one left by a
/// process that was killed mid-write?
pub fn is_atomic_tmp(name: &str) -> bool {
    name.starts_with('.')
        && name
            .rsplit_once(".tmp.")
            .is_some_and(|(stem, pid)| stem.len() > 1 && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
}

fn cstr(p: &Path) -> Result<CString> {
    Ok(CString::new(p.as_os_str().as_bytes())?)
}

/// Atomically swap two paths (renameat2 RENAME_EXCHANGE). Works for dir <-> subvolume.
pub fn rename_exchange(a: &Path, b: &Path) -> Result<()> {
    let (ca, cb) = (cstr(a)?, cstr(b)?);
    let r = unsafe { libc::renameat2(libc::AT_FDCWD, ca.as_ptr(), libc::AT_FDCWD, cb.as_ptr(), libc::RENAME_EXCHANGE) };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        bail!("renameat2(EXCHANGE {} <-> {}): {e}", a.display(), b.display());
    }
    Ok(())
}

/// rename(2) only: fails with EXDEV instead of falling back to a copy.
pub fn rename_strict(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).with_context(|| format!("rename {} -> {}", from.display(), to.display()))
}

/// `cp -a --reflink=… -T src dst`: copies a file or merges a directory's contents into dst.
pub fn cp_a(src: &Path, dst: &Path, mode: ReflinkMode) -> Result<()> {
    let reflink = match mode {
        ReflinkMode::Always => "--reflink=always",
        ReflinkMode::Auto => "--reflink=auto",
    };
    let run = |reflink: &str| Command::new("cp").arg("-a").arg(reflink).arg("-T").arg(src).arg(dst).output();
    let mut out = run(reflink).context("spawn cp")?;
    // btrfs refuses to clone between files whose NOCOW (chattr +C) flags differ, and cp -a does
    // not copy that flag: such files are copied instead of cloned
    if !out.status.success()
        && mode == ReflinkMode::Always
        && String::from_utf8_lossy(&out.stderr).contains("failed to clone")
    {
        tracing::warn!(
            "cp: some files under {} cannot be reflinked (NOCOW?); copying their data instead",
            src.display()
        );
        out = run("--reflink=auto").context("spawn cp")?;
    }
    if !out.status.success() {
        bail!("cp -a {} {} failed: {}", src.display(), dst.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn lchown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid)).with_context(|| format!("chown {}", path.display()))
}

/// Copy owner and permission bits of `like` onto `path` (best effort for ownership when not root).
pub fn copy_owner_mode(like: &fs::Metadata, path: &Path) -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        lchown(path, like.uid(), like.gid())?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(like.mode() & 0o7777))
        .with_context(|| format!("chmod {}", path.display()))
}

pub fn set_owner_mode(path: &Path, uid: u32, gid: u32, mode: u32) -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        lchown(path, uid, gid)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
        .with_context(|| format!("chmod {}", path.display()))
}

pub fn dir_is_empty(path: &Path) -> Result<bool> {
    Ok(fs::read_dir(path)?.next().is_none())
}

/// Is `path` exactly a mount point (per /proc/self/mountinfo)? Nested subvolumes are not.
pub fn is_mount_point(path: &Path) -> bool {
    let Ok(canon) = fs::canonicalize(path) else {
        return false;
    };
    let Ok(info) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    info.lines().any(|l| {
        l.split_whitespace().nth(4).map(|mp| unescape_mount(mp) == canon.as_os_str().to_string_lossy()).unwrap_or(false)
    })
}

fn unescape_mount(s: &str) -> String {
    s.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\")
}

/// Relative path check for config-provided project paths: no absolute, no `..`, not empty.
pub fn safe_relative(p: &str) -> Option<PathBuf> {
    let t = p.trim().trim_end_matches('/');
    if t.is_empty() {
        return None;
    }
    let pb = PathBuf::from(t);
    if pb.is_absolute() || pb.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        return None;
    }
    Some(pb)
}

pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_write_and_exchange() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x.toml");
        write_atomic(&f, b"a=1", 0o644).unwrap();
        write_atomic(&f, b"a=2", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "a=2");
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
        // a failing write leaves no temp file behind: the target is a directory, so rename fails
        let blocker = d.path().join("dir.toml");
        fs::create_dir(&blocker).unwrap();
        fs::write(blocker.join("inside"), "").unwrap();
        assert!(write_atomic(&blocker, b"a=3", 0o644).is_err());
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 2);
        fs::remove_dir_all(&blocker).unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        fs::create_dir(&a).unwrap();
        fs::write(a.join("in_a"), "").unwrap();
        fs::create_dir(&b).unwrap();
        rename_exchange(&a, &b).unwrap();
        assert!(b.join("in_a").exists());
    }
    #[test]
    fn atomic_tmp_names() {
        assert!(is_atomic_tmp(".state.toml.tmp.2010439"));
        assert!(is_atomic_tmp(".FROZEN.tmp.1"));
        assert!(!is_atomic_tmp("state.toml"));
        assert!(!is_atomic_tmp("9.tmp"));
        assert!(!is_atomic_tmp(".tmp.12"));
        assert!(!is_atomic_tmp(".state.toml.tmp."));
        assert!(!is_atomic_tmp(".state.toml.tmp.12x"));
    }
    #[test]
    fn relative_paths() {
        assert_eq!(safe_relative("target/"), Some(PathBuf::from("target")));
        assert_eq!(safe_relative("web/node_modules"), Some(PathBuf::from("web/node_modules")));
        assert!(safe_relative("/abs").is_none());
        assert!(safe_relative("../x").is_none());
        assert!(safe_relative("").is_none());
    }
}
