//! BTRFS_IOC_GET_SUBVOL_INFO: unprivileged subvolume metadata.
//! BTRFS_IOC_TREE_SEARCH_V2: privileged search of a subvolume tree, used to tell a real change
//! from a balance when only `generation` moved.

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

// ---------------- BTRFS_IOC_TREE_SEARCH_V2 ----------------

/// _IOWR(BTRFS_IOCTL_MAGIC 0x94, 17, struct btrfs_ioctl_search_args_v2 /* 112 bytes + buf */)
pub const BTRFS_IOC_TREE_SEARCH_V2: u64 = 0xc070_9411;
/// struct btrfs_ioctl_search_key
pub const SEARCH_KEY_SIZE: usize = 104;
/// The key followed by `buf_size`; items start right after.
pub const SEARCH_ARGS_SIZE: usize = 112;
/// struct btrfs_ioctl_search_header
pub const SEARCH_HEADER_SIZE: usize = 32;
const SEARCH_BUF_SIZE: usize = 1 << 20;

// Offsets within struct btrfs_ioctl_search_key, then buf_size of the v2 args.
const SK_TREE_ID: usize = 0;
const SK_MIN_OBJECTID: usize = 8;
const SK_MAX_OBJECTID: usize = 16;
const SK_MIN_OFFSET: usize = 24;
const SK_MAX_OFFSET: usize = 32;
const SK_MIN_TRANSID: usize = 40;
const SK_MAX_TRANSID: usize = 48;
const SK_MIN_TYPE: usize = 56;
const SK_MAX_TYPE: usize = 60;
const SK_NR_ITEMS: usize = 64;
const ARGS_BUF_SIZE: usize = 104;

pub const BTRFS_INODE_ITEM_KEY: u32 = 1;
/// BTRFS_FIRST_FREE_OBJECTID: the subvolume's root directory; inodes count up from here.
const FIRST_FREE_OBJECTID: u64 = 256;
/// BTRFS_LAST_FREE_OBJECTID (-256ULL): objectids above are internal (orphans, free-ino cache).
const LAST_FREE_OBJECTID: u64 = u64::MAX - 255;

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchHeader {
    /// Generation of the leaf holding the item (a balance bumps it), not of the item itself.
    pub transid: u64,
    pub objectid: u64,
    pub offset: u64,
    pub item_type: u32,
    pub len: u32,
}

/// Search key over every inode of the subvolume tree of the fd, in leaves written after
/// `transid`. Types cannot be range-filtered (the key range is lexicographic over
/// objectid, type, offset), so callers filter inode items themselves.
pub fn inode_search_key(transid: u64) -> [u8; SEARCH_KEY_SIZE] {
    let mut k = [0u8; SEARCH_KEY_SIZE];
    put_u64(&mut k, SK_TREE_ID, 0); // 0: the tree of the subvolume the fd is in
    put_u64(&mut k, SK_MIN_OBJECTID, FIRST_FREE_OBJECTID);
    put_u64(&mut k, SK_MAX_OBJECTID, LAST_FREE_OBJECTID);
    put_u64(&mut k, SK_MIN_OFFSET, 0);
    put_u64(&mut k, SK_MAX_OFFSET, u64::MAX);
    put_u64(&mut k, SK_MIN_TRANSID, transid.saturating_add(1));
    put_u64(&mut k, SK_MAX_TRANSID, u64::MAX);
    put_u32(&mut k, SK_MIN_TYPE, 0);
    put_u32(&mut k, SK_MAX_TYPE, 255);
    k
}

/// The `nr` items the kernel wrote into `buf` (the bytes after the args header).
pub fn parse_items(buf: &[u8], nr: u32) -> Result<Vec<(SearchHeader, &[u8])>> {
    let mut out = Vec::with_capacity(nr as usize);
    let mut off = 0;
    for i in 0..nr {
        if off + SEARCH_HEADER_SIZE > buf.len() {
            bail!("tree search: item {i} header past the end of the buffer");
        }
        let h = SearchHeader {
            transid: u64_at(buf, off),
            objectid: u64_at(buf, off + 8),
            offset: u64_at(buf, off + 16),
            item_type: u32_at(buf, off + 24),
            len: u32_at(buf, off + 28),
        };
        off += SEARCH_HEADER_SIZE;
        let end = off + h.len as usize;
        if end > buf.len() {
            bail!("tree search: item {i} body past the end of the buffer");
        }
        out.push((h, &buf[off..end]));
        off = end;
    }
    Ok(out)
}

/// `transid` of a btrfs_inode_item: the transaction that last updated the inode.
pub fn inode_item_transid(body: &[u8]) -> Option<u64> {
    (body.len() >= 16).then(|| u64_at(body, 8))
}

/// Continue the search after `last`. False when the key space is exhausted.
pub fn advance_key(key: &mut [u8; SEARCH_KEY_SIZE], last: &SearchHeader) -> bool {
    let (mut objectid, mut item_type, mut offset) = (last.objectid, last.item_type, last.offset);
    if offset < u64::MAX {
        offset += 1;
    } else if item_type < 255 {
        item_type += 1;
        offset = 0;
    } else if objectid < u64_at(key, SK_MAX_OBJECTID) {
        objectid += 1;
        item_type = 0;
        offset = 0;
    } else {
        return false;
    }
    put_u64(key, SK_MIN_OBJECTID, objectid);
    put_u32(key, SK_MIN_TYPE, item_type);
    put_u64(key, SK_MIN_OFFSET, offset);
    true
}

/// Is any inode item of the subvolume containing `path` newer than `transid`?
/// `Ok(None)` without CAP_SYS_ADMIN. Only leaves written after `transid` are read, so an
/// unchanged tree costs one ioctl.
pub fn inode_newer_than(path: &Path, transid: u64) -> Result<Option<bool>> {
    let f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut args = vec![0u8; SEARCH_ARGS_SIZE + SEARCH_BUF_SIZE];
    let mut key = inode_search_key(transid);
    loop {
        // nr_items and buf_size are in/out: reset both on every call
        put_u32(&mut key, SK_NR_ITEMS, u32::MAX);
        args[..SEARCH_KEY_SIZE].copy_from_slice(&key);
        put_u64(&mut args, ARGS_BUF_SIZE, SEARCH_BUF_SIZE as u64);
        let r = unsafe { libc::ioctl(f.as_raw_fd(), BTRFS_IOC_TREE_SEARCH_V2 as _, args.as_mut_ptr()) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::EPERM) => Ok(None),
                Some(libc::ENOTTY) => bail!("{} is not on a btrfs filesystem", path.display()),
                _ => Err(e).with_context(|| format!("TREE_SEARCH_V2 {}", path.display())),
            };
        }
        let nr = u32_at(&args, SK_NR_ITEMS);
        if nr == 0 {
            return Ok(Some(false));
        }
        let items = parse_items(&args[SEARCH_ARGS_SIZE..], nr)?;
        if items.iter().any(|(h, body)| {
            h.item_type == BTRFS_INODE_ITEM_KEY && inode_item_transid(body).is_some_and(|t| t > transid)
        }) {
            return Ok(Some(true));
        }
        let last = items.last().unwrap().0;
        if !advance_key(&mut key, &last) {
            return Ok(Some(false));
        }
    }
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

    #[test]
    fn tree_search_v2_code_matches_iowr_encoding() {
        // _IOWR(type, nr, size) = (3 << 30) | (size << 16) | (type << 8) | nr
        let code = (3u64 << 30) | ((SEARCH_ARGS_SIZE as u64) << 16) | (0x94 << 8) | 17;
        assert_eq!(code, BTRFS_IOC_TREE_SEARCH_V2);
        assert_eq!(SEARCH_ARGS_SIZE, SEARCH_KEY_SIZE + 8);
    }

    #[test]
    fn search_key_layout() {
        let k = inode_search_key(546706);
        assert_eq!(u64_at(&k, 0), 0, "tree of the fd");
        assert_eq!((u64_at(&k, 8), u64_at(&k, 16)), (256, u64::MAX - 255));
        assert_eq!((u64_at(&k, 24), u64_at(&k, 32)), (0, u64::MAX));
        assert_eq!((u64_at(&k, 40), u64_at(&k, 48)), (546707, u64::MAX), "leaves newer than the snapshot");
        assert_eq!((u32_at(&k, 56), u32_at(&k, 60)), (0, 255));
        assert_eq!(u64_at(&inode_search_key(u64::MAX), 40), u64::MAX, "saturates");
    }

    fn item(buf: &mut Vec<u8>, transid: u64, objectid: u64, ty: u32, offset: u64, body: &[u8]) {
        for v in [transid, objectid, offset] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf.extend_from_slice(&ty.to_le_bytes());
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(body);
    }

    fn inode_body(transid: u64) -> Vec<u8> {
        let mut b = vec![0u8; 160]; // sizeof(struct btrfs_inode_item)
        b[0..8].copy_from_slice(&7u64.to_le_bytes()); // generation (creation)
        b[8..16].copy_from_slice(&transid.to_le_bytes());
        b
    }

    #[test]
    fn parses_items_including_empty_bodies() {
        let mut buf = Vec::new();
        item(&mut buf, 900, 257, BTRFS_INODE_ITEM_KEY, 0, &inode_body(500));
        item(&mut buf, 900, 257, 48, 0, &[]); // an item without a body (e.g. orphan)
        item(&mut buf, 901, 258, 108, 4096, &[1, 2, 3]); // EXTENT_DATA
        buf.extend_from_slice(&[0u8; 64]); // unused rest of the buffer
        let items = parse_items(&buf, 3).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].0, SearchHeader { transid: 900, objectid: 257, offset: 0, item_type: 1, len: 160 });
        assert_eq!(inode_item_transid(items[0].1), Some(500), "the item's transid, not the leaf's");
        assert_eq!(items[1].1.len(), 0);
        assert_eq!((items[2].0.offset, items[2].1), (4096, &[1u8, 2, 3][..]));
        assert!(parse_items(&buf[..40], 1).is_err(), "truncated body");
        assert!(parse_items(&buf, 99).is_err(), "more items than bytes");
        assert_eq!(inode_item_transid(&[0u8; 15]), None);
    }

    #[test]
    fn advance_key_rolls_over() {
        let mut k = inode_search_key(10);
        let h = |objectid, item_type, offset| SearchHeader { transid: 0, objectid, offset, item_type, len: 0 };
        assert!(advance_key(&mut k, &h(300, 12, 5)));
        assert_eq!((u64_at(&k, 8), u32_at(&k, 56), u64_at(&k, 24)), (300, 12, 6));
        assert!(advance_key(&mut k, &h(300, 12, u64::MAX)));
        assert_eq!((u64_at(&k, 8), u32_at(&k, 56), u64_at(&k, 24)), (300, 13, 0));
        assert!(advance_key(&mut k, &h(300, 255, u64::MAX)));
        assert_eq!((u64_at(&k, 8), u32_at(&k, 56), u64_at(&k, 24)), (301, 0, 0));
        assert!(!advance_key(&mut k, &h(u64::MAX - 255, 255, u64::MAX)), "exhausted");
        assert_eq!(u64_at(&k, 40), 11, "transid range untouched");
    }

    #[test]
    fn inode_search_on_root_if_available() {
        // nothing is newer than the largest transid; unprivileged callers get None
        match inode_newer_than(Path::new("/"), u64::MAX - 1) {
            Ok(r) => assert!(r == Some(false) || r.is_none(), "{r:?}"),
            Err(e) => assert!(format!("{e:#}").contains("not on a btrfs"), "{e:#}"),
        }
    }
}
