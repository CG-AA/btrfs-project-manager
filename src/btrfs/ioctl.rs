//! BTRFS_IOC_GET_SUBVOL_INFO: unprivileged subvolume metadata.

use super::{SubvolInfo, Uuid};
use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use std::os::fd::AsRawFd;
use std::path::Path;

/// _IOR(BTRFS_IOCTL_MAGIC 0x94, 60, struct btrfs_ioctl_get_subvol_info_args /* 504 bytes */)
pub const BTRFS_IOC_GET_SUBVOL_INFO: u64 = 0x81f8_943c;
pub const ARGS_SIZE: usize = 504;

// Offsets within struct btrfs_ioctl_get_subvol_info_args (include/uapi/linux/btrfs.h).
const OFF_TREEID: usize = 0;
const OFF_NAME: usize = 8;
const OFF_PARENT_ID: usize = 264;
const OFF_GENERATION: usize = 280;
const OFF_FLAGS: usize = 288;
const OFF_UUID: usize = 296;
const OFF_PARENT_UUID: usize = 312;
const OFF_RECEIVED_UUID: usize = 328;
const OFF_CTRANSID: usize = 344;
const OFF_OTRANSID: usize = 352;
const OFF_CTIME: usize = 376;
const OFF_OTIME: usize = 392;

fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

fn uuid_at(b: &[u8], off: usize) -> Uuid {
    Uuid(b[off..off + 16].try_into().unwrap())
}

fn ts_at(b: &[u8], off: usize) -> Option<Timestamp> {
    let sec = u64_at(b, off) as i64;
    let nsec = u32::from_le_bytes(b[off + 8..off + 12].try_into().unwrap()) as i32;
    if sec == 0 {
        return None;
    }
    Timestamp::new(sec, nsec).ok()
}

pub fn parse_args(b: &[u8; ARGS_SIZE]) -> SubvolInfo {
    let name_raw = &b[OFF_NAME..OFF_PARENT_ID];
    let end = name_raw.iter().position(|c| *c == 0).unwrap_or(name_raw.len());
    let opt = |u: Uuid| if u.is_nil() { None } else { Some(u) };
    SubvolInfo {
        id: u64_at(b, OFF_TREEID),
        name: String::from_utf8_lossy(&name_raw[..end]).into_owned(),
        parent_id: u64_at(b, OFF_PARENT_ID),
        generation: u64_at(b, OFF_GENERATION),
        flags: u64_at(b, OFF_FLAGS),
        uuid: uuid_at(b, OFF_UUID),
        parent_uuid: opt(uuid_at(b, OFF_PARENT_UUID)),
        received_uuid: opt(uuid_at(b, OFF_RECEIVED_UUID)),
        ctransid: u64_at(b, OFF_CTRANSID),
        otransid: u64_at(b, OFF_OTRANSID),
        ctime: ts_at(b, OFF_CTIME),
        otime: ts_at(b, OFF_OTIME),
    }
}

/// Info about the subvolume containing `path` (the path itself need not be the root).
pub fn get_subvol_info(path: &Path) -> Result<SubvolInfo> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = [0u8; ARGS_SIZE];
    let r = unsafe { libc::ioctl(f.as_raw_fd(), BTRFS_IOC_GET_SUBVOL_INFO as _, buf.as_mut_ptr()) };
    // The kernel handler can return a positive leftover from its tree search on success
    // (observed: 1 on kernel 7.0), so only a negative return is an error.
    if r < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ENOTTY) {
            bail!("{} is not on a btrfs filesystem", path.display());
        }
        bail!("GET_SUBVOL_INFO {}: {e}", path.display());
    }
    Ok(parse_args(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_code_matches_ior_encoding() {
        // _IOR(type, nr, size) = (2 << 30) | (size << 16) | (type << 8) | nr
        let code = (2u64 << 30) | ((ARGS_SIZE as u64) << 16) | (0x94 << 8) | 60;
        assert_eq!(code, BTRFS_IOC_GET_SUBVOL_INFO);
    }

    #[test]
    fn parses_layout() {
        let mut b = [0u8; ARGS_SIZE];
        b[OFF_TREEID..OFF_TREEID + 8].copy_from_slice(&260u64.to_le_bytes());
        b[OFF_NAME..OFF_NAME + 6].copy_from_slice(b"@space");
        b[OFF_GENERATION..OFF_GENERATION + 8].copy_from_slice(&202402u64.to_le_bytes());
        b[OFF_FLAGS..OFF_FLAGS + 8].copy_from_slice(&1u64.to_le_bytes());
        b[OFF_UUID] = 0x0b;
        b[OFF_PARENT_UUID + 15] = 0x49;
        b[OFF_CTRANSID..OFF_CTRANSID + 8].copy_from_slice(&202388u64.to_le_bytes());
        b[OFF_OTRANSID..OFF_OTRANSID + 8].copy_from_slice(&202392u64.to_le_bytes());
        b[OFF_OTIME..OFF_OTIME + 8].copy_from_slice(&1_789_393_407u64.to_le_bytes());
        let i = parse_args(&b);
        assert_eq!(i.id, 260);
        assert_eq!(i.name, "@space");
        assert_eq!(i.generation, 202402);
        assert!(i.readonly());
        assert_eq!(i.uuid.0[0], 0x0b);
        assert_eq!(i.parent_uuid.unwrap().0[15], 0x49);
        assert!(i.received_uuid.is_none());
        assert_eq!((i.ctransid, i.otransid), (202388, 202392));
        assert!(i.ctime.is_none());
        assert_eq!(i.otime.unwrap().as_second(), 1_789_393_407);
    }

    #[test]
    fn live_on_btrfs_root_if_available() {
        if let Ok(info) = get_subvol_info(Path::new("/")) {
            assert!(info.id >= 5);
            assert!(info.generation >= info.ctransid.min(info.generation));
        }
    }
}
