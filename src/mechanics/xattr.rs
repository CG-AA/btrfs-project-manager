//! Copy extended attributes (including POSIX ACLs) from one inode to another.

use anyhow::{Result, bail};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn cpath(p: &Path) -> Result<CString> {
    Ok(CString::new(p.as_os_str().as_bytes())?)
}

pub fn copy_xattrs(src: &Path, dst: &Path) -> Result<()> {
    let (s, d) = (cpath(src)?, cpath(dst)?);
    let len = unsafe { libc::llistxattr(s.as_ptr(), std::ptr::null_mut(), 0) };
    if len <= 0 {
        return Ok(());
    }
    let mut names = vec![0u8; len as usize];
    let len = unsafe { libc::llistxattr(s.as_ptr(), names.as_mut_ptr() as *mut libc::c_char, names.len()) };
    if len < 0 {
        return Ok(());
    }
    for name in names[..len as usize].split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let cname = CString::new(name)?;
        let vlen = unsafe { libc::lgetxattr(s.as_ptr(), cname.as_ptr(), std::ptr::null_mut(), 0) };
        if vlen < 0 {
            continue;
        }
        let mut val = vec![0u8; vlen as usize];
        let vlen =
            unsafe { libc::lgetxattr(s.as_ptr(), cname.as_ptr(), val.as_mut_ptr() as *mut libc::c_void, val.len()) };
        if vlen < 0 {
            continue;
        }
        let r = unsafe {
            libc::lsetxattr(d.as_ptr(), cname.as_ptr(), val.as_ptr() as *const libc::c_void, vlen as usize, 0)
        };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            // security.* / trusted.* need privileges; skip those quietly when unprivileged
            if e.raw_os_error() == Some(libc::EPERM) || e.raw_os_error() == Some(libc::ENOTSUP) {
                continue;
            }
            bail!("setxattr {} on {}: {e}", String::from_utf8_lossy(name), dst.display());
        }
    }
    Ok(())
}

/// Copy atime/mtime of `src` onto `dst` (no symlink following).
pub fn copy_times(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(src)?;
    let d = cpath(dst)?;
    let times = [
        libc::timespec { tv_sec: md.atime(), tv_nsec: md.atime_nsec() },
        libc::timespec { tv_sec: md.mtime(), tv_nsec: md.mtime_nsec() },
    ];
    let r = unsafe { libc::utimensat(libc::AT_FDCWD, d.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    if r != 0 {
        bail!("utimensat {}: {}", dst.display(), std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copies_user_xattrs_and_times() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let ca = cpath(&a).unwrap();
        let name = CString::new("user.bpmtest").unwrap();
        let ok = unsafe { libc::lsetxattr(ca.as_ptr(), name.as_ptr(), b"v".as_ptr() as *const _, 1, 0) } == 0;
        copy_xattrs(&a, &b).unwrap();
        if ok {
            let cb = cpath(&b).unwrap();
            let mut buf = [0u8; 4];
            let n = unsafe { libc::lgetxattr(cb.as_ptr(), name.as_ptr(), buf.as_mut_ptr() as *mut _, 4) };
            assert_eq!(n, 1);
        }
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::open(&a).unwrap().set_modified(old).unwrap();
        copy_times(&a, &b).unwrap();
        assert_eq!(std::fs::metadata(&b).unwrap().modified().unwrap(), old);
    }
}
