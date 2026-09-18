//! /proc scanning: who has files open under a directory.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct OpenUse {
    pub pid: u32,
    pub comm: String,
    pub path: PathBuf,
    pub writing: bool,
    pub cwd: bool,
}

/// Processes with an fd (or cwd) under `dir`. Needs root to see other users' processes.
pub fn open_under(dir: &Path) -> Vec<OpenUse> {
    let Ok(dir) = fs::canonicalize(dir) else {
        return vec![];
    };
    let mut out = Vec::new();
    let Ok(procs) = fs::read_dir("/proc") else {
        return out;
    };
    let me = std::process::id();
    for p in procs.flatten() {
        let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let base = p.path();
        let comm = fs::read_to_string(base.join("comm")).unwrap_or_default().trim().to_string();
        if let Ok(cwd) = fs::read_link(base.join("cwd")) {
            if cwd.starts_with(&dir) {
                out.push(OpenUse { pid, comm: comm.clone(), path: cwd, writing: false, cwd: true });
            }
        }
        let Ok(fds) = fs::read_dir(base.join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            if !target.starts_with(&dir) {
                continue;
            }
            let writing = fs::read_to_string(base.join("fdinfo").join(fd.file_name()))
                .ok()
                .and_then(|s| {
                    s.lines().find_map(|l| l.strip_prefix("flags:")).and_then(|v| u32::from_str_radix(v.trim(), 8).ok())
                })
                .map(|flags| flags & libc::O_ACCMODE as u32 != libc::O_RDONLY as u32)
                .unwrap_or(false);
            out.push(OpenUse { pid, comm: comm.clone(), path: target, writing, cwd: false });
        }
    }
    out
}

pub fn writers_under(dir: &Path) -> Vec<OpenUse> {
    open_under(dir).into_iter().filter(|u| u.writing).collect()
}

pub fn describe(uses: &[OpenUse]) -> String {
    uses.iter().take(5).map(|u| format!("{}[{}] {}", u.comm, u.pid, u.path.display())).collect::<Vec<_>>().join(", ")
}

/// `describe` with paths relative to `dir` (`.` for `dir` itself), for a tree about to be renamed.
pub fn describe_under(uses: &[OpenUse], dir: &Path) -> String {
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    uses.iter()
        .take(5)
        .map(|u| {
            let rel = u.path.strip_prefix(&dir).map(|r| r.to_path_buf()).unwrap_or_else(|_| u.path.clone());
            let rel = if rel.as_os_str().is_empty() { PathBuf::from(".") } else { rel };
            let what = if u.cwd {
                "working directory"
            } else if u.writing {
                "open for writing"
            } else {
                "open"
            };
            format!("{}[{}] {} ({what})", u.comm, u.pid, rel.display())
        })
        .collect::<Vec<_>>()
        .join(", ")
}
