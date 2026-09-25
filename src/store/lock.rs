//! Advisory flock-based locks with timeout and holder description.

use crate::error::BpmError;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct Lock {
    file: File,
    pub path: PathBuf,
}

impl Lock {
    pub fn acquire(path: &Path, timeout: Duration, what: &str) -> Result<Lock> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("open lock {}", path.display()))?;
        let start = Instant::now();
        loop {
            let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if r == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(err).with_context(|| format!("flock {}", path.display()));
            }
            if start.elapsed() >= timeout {
                let mut holder = String::new();
                let _ = file.read_to_string(&mut holder);
                return Err(BpmError::Locked { holder: format!("{what}: {}", holder.trim()) }.into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // the holder we waited for may have removed or moved the lock file with its directory
        // (`forget`, a relink): this lock then guards nothing, and writing through it would
        // recreate a directory that was just deleted
        if !same_file(&file, path) {
            return Err(BpmError::Locked { holder: format!("{what}: removed or moved while waiting") }.into());
        }
        let desc = format!(
            "pid={} since={} cmd={}",
            std::process::id(),
            jiff::Timestamp::now(),
            std::env::args().collect::<Vec<_>>().join(" ")
        );
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = file.write_all(desc.as_bytes());
        Ok(Lock { file, path: path.to_path_buf() })
    }
}

/// Does some process hold the lock at `path`? Needs only read access, so `doctor` can ask as a
/// normal user; a missing or unreadable lock file counts as not held.
pub fn is_held(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC).open(path) else {
        return false;
    };
    let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}

fn same_file(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exclusive_with_timeout() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("lock");
        let a = Lock::acquire(&p, Duration::ZERO, "t").unwrap();
        let err = Lock::acquire(&p, Duration::from_millis(150), "t").err().unwrap();
        assert_eq!(crate::error::exit_code_for(&err), 4);
        assert!(format!("{err}").contains("pid="));
        drop(a);
        Lock::acquire(&p, Duration::ZERO, "t").unwrap();
    }
    #[test]
    fn removed_while_waiting() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("lock");
        let a = Lock::acquire(&p, Duration::ZERO, "t").unwrap();
        let p2 = p.clone();
        let waiter = std::thread::spawn(move || Lock::acquire(&p2, Duration::from_secs(5), "t"));
        std::thread::sleep(Duration::from_millis(250));
        std::fs::remove_file(&p).unwrap();
        drop(a);
        let err = waiter.join().unwrap().err().unwrap();
        assert_eq!(crate::error::exit_code_for(&err), 4);
        assert!(format!("{err}").contains("removed or moved"));
        assert!(!p.exists());
    }
}
