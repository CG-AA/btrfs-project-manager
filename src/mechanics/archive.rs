//! Manual archive: `btrfs send | zstd` into the archive directory, and the reverse.

use crate::btrfs::Uuid;
use crate::ctx::Ctx;
use crate::error::{not_found, refused};
use crate::project::ProjectRef;
use crate::store::{Origin, SnapshotKind, SnapshotMeta};
use crate::util::walk::TreeStats;
use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub project: String,
    pub project_uuid: Uuid,
    #[serde(default)]
    pub uuid_history: Vec<Uuid>,
    pub owner_uid: u32,
    pub owner_gid: u32,
    pub original_path: PathBuf,
    pub snapshot_id: u64,
    pub snapshot_uuid: Uuid,
    pub snapshot_otransid: u64,
    pub snapshot_created: Timestamp,
    #[serde(default)]
    pub stats: Option<TreeStats>,
    /// Nested subvolumes (build dirs) that existed in the live project; `send` does not carry them.
    #[serde(default)]
    pub nested: Vec<String>,
    pub compressed_data: bool,
    pub zstd_level: u8,
    pub bytes: u64,
    pub sha256: String,
    pub created: Timestamp,
    pub bpm_version: String,
    pub btrfs_progs: String,
}

pub fn manifest_path(archive: &Path) -> PathBuf {
    crate::util::fs::sibling(archive, ".manifest.toml")
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn progs_version() -> String {
    Command::new("btrfs")
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").to_string())
        .unwrap_or_default()
}

#[derive(Clone, Debug, Serialize)]
pub struct ArchiveReport {
    pub file: PathBuf,
    pub manifest: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

pub fn archive(
    ctx: &Ctx,
    pref: &ProjectRef,
    meta: &SnapshotMeta,
    nested: Vec<String>,
    dest_root: &Path,
    level: u8,
    fast: bool,
) -> Result<ArchiveReport> {
    let dir = dest_root.join(pref.name());
    let stamp = meta.created.strftime("%Y%m%dT%H%M%SZ").to_string();
    let file = dir.join(format!("{}-{stamp}.btrfs.zst", meta.id));
    let part = crate::util::fs::sibling(&file, ".part");
    if file.exists() {
        return Err(refused(format!("{} already exists", file.display())));
    }
    if ctx.opts.dry_run {
        tracing::info!("[dry-run] btrfs send #{} | zstd -{level} > {}", meta.id, file.display());
        return Ok(ArchiveReport { manifest: manifest_path(&file), file, bytes: 0, sha256: String::new() });
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let level = if fast { level.min(3) } else { level };
    let mut zstd = Command::new("zstd");
    zstd.args(["-q", "-T0", &format!("-{level}")]);
    if level > 19 {
        zstd.arg("--ultra");
    }
    if !fast {
        zstd.arg("--long=27");
    }
    let mut child = zstd
        .arg("-o")
        .arg(&part)
        .arg("-")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn zstd")?;
    let send_result = {
        let mut stdin = child.stdin.take().unwrap();
        ctx.btrfs.send(&pref.unit.snapshot_path(meta.id), fast, &mut stdin)
    };
    let out = child.wait_with_output()?;
    if let Err(e) = send_result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if !out.status.success() {
        let _ = std::fs::remove_file(&part);
        bail!("zstd failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let test = Command::new("zstd").args(["-q", "-t", "--long=27"]).arg(&part).status()?;
    if !test.success() {
        let _ = std::fs::remove_file(&part);
        bail!("zstd -t failed for {}", part.display());
    }
    let sha = sha256_file(&part)?;
    let bytes = std::fs::metadata(&part)?.len();
    std::fs::rename(&part, &file)?;
    let manifest = Manifest {
        format: 1,
        project: pref.name().into(),
        project_uuid: pref.record.uuid,
        uuid_history: pref.record.uuid_history.clone(),
        owner_uid: pref.record.owner_uid,
        owner_gid: pref.record.owner_gid,
        original_path: pref.record.path.clone(),
        snapshot_id: meta.id,
        snapshot_uuid: meta.snapshot_uuid,
        snapshot_otransid: meta.snapshot_otransid,
        snapshot_created: meta.created,
        stats: meta.stats.clone(),
        nested,
        compressed_data: fast,
        zstd_level: level,
        bytes,
        sha256: sha.clone(),
        created: ctx.now(),
        bpm_version: env!("CARGO_PKG_VERSION").into(),
        btrfs_progs: progs_version(),
    };
    let mpath = manifest_path(&file);
    crate::util::fs::write_atomic(&mpath, toml::to_string_pretty(&manifest)?.as_bytes(), 0o644)?;
    let shaline = format!("{sha}  {}\n", file.file_name().unwrap().to_string_lossy());
    crate::util::fs::write_atomic(&crate::util::fs::sibling(&file, ".sha256"), shaline.as_bytes(), 0o644)?;
    Ok(ArchiveReport { file, manifest: mpath, bytes, sha256: sha })
}

pub fn read_manifest(archive: &Path) -> Result<Manifest> {
    let p = manifest_path(archive);
    let t = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    Ok(toml::from_str(&t)?)
}

/// Newest archive file for `project` under `dest_root`.
pub fn newest_archive(dest_root: &Path, project: &str) -> Result<PathBuf> {
    let dir = dest_root.join(project);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|_| not_found(format!("no archives in {}", dir.display())))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".btrfs.zst"))
        .collect();
    files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    files.pop().ok_or_else(|| not_found(format!("no archives in {}", dir.display())))
}

/// Receive an archive into the project's store unit as a held `received` snapshot.
pub fn receive(ctx: &Ctx, pref: &ProjectRef, archive: &Path, manifest: &Manifest) -> Result<SnapshotMeta> {
    let sha = sha256_file(archive)?;
    if sha != manifest.sha256 {
        bail!("sha256 mismatch for {}: manifest {}, file {sha}", archive.display(), manifest.sha256);
    }
    let unit = &pref.unit;
    unit.ensure_dir()?;
    let id = unit.next_id()?;
    let tmp = unit.dir.join(format!("{id}.tmp"));
    std::fs::create_dir(&tmp)?;
    let mut child = Command::new("zstd")
        .args(["-q", "-d", "-c", "--long=27"])
        .arg(archive)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn zstd -d")?;
    let mut stdout = child.stdout.take().unwrap();
    let recv = ctx.btrfs.receive(&mut stdout, &tmp);
    drop(stdout);
    let out = child.wait_with_output()?;
    let snap_path = tmp.join("snapshot");
    if let Err(e) = recv.and_then(|_| {
        if out.status.success() {
            Ok(())
        } else {
            bail!("zstd -d failed: {}", String::from_utf8_lossy(&out.stderr).trim())
        }
    }) {
        if snap_path.exists() {
            let _ = ctx.btrfs.delete_subvolume(&snap_path, false);
        }
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let info = ctx.btrfs.subvol_info(&snap_path)?;
    if info.received_uuid != Some(manifest.snapshot_uuid) {
        let _ = ctx.btrfs.delete_subvolume(&snap_path, false);
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("received subvolume has received_uuid {:?}, expected {}", info.received_uuid, manifest.snapshot_uuid);
    }
    let meta = SnapshotMeta {
        format: 1,
        id,
        project: pref.name().into(),
        created: ctx.now(),
        kind: SnapshotKind::Received,
        reason: format!("unarchived from {}", archive.display()),
        pair: None,
        hold: true,
        hold_note: "received from archive".into(),
        source_uuid: manifest.project_uuid,
        source_ctransid: info.ctransid,
        snapshot_uuid: info.uuid,
        snapshot_otransid: info.otransid,
        received_uuid: info.received_uuid,
        stats: manifest.stats.clone(),
        origin: Origin {
            bpm: env!("CARGO_PKG_VERSION").into(),
            user: ctx.invoker.user.clone(),
            argv: ctx.invoker.argv.clone(),
        },
    };
    unit.write_meta(&tmp, &meta)?;
    std::fs::rename(&tmp, unit.snapshot_dir(id))?;
    Ok(meta)
}
