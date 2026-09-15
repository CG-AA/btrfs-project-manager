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
}
