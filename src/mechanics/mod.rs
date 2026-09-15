//! Multi-step procedures that change subvolumes: snapshot, delete, adopt, banlist, rollback,
//! restore, collapse, recompress, archive.

pub mod adopt;
pub mod archive;
pub mod banlist;
pub mod guard;
pub mod observe;
pub mod recompress;
pub mod retire;
pub mod rollback;
pub mod snapshot;
pub mod xattr;

use crate::btrfs::Uuid;
use crate::store::Unit;
use std::path::{Path, PathBuf};

/// Everything needed to snapshot one live subvolume into one store unit.
#[derive(Clone, Debug)]
pub struct Target<'a> {
    pub unit: &'a Unit,
    pub name: &'a str,
    pub live: &'a Path,
    pub root: &'a Path,
    pub expected_uuid: Option<Uuid>,
    pub owner: Option<(u32, u32)>,
    pub stats_exclude: Vec<PathBuf>,
    pub sentinels: Vec<String>,
}

impl<'a> Target<'a> {
    pub fn for_project(pref: &'a crate::project::ProjectRef, eff: &crate::config::EffectiveConfig) -> Target<'a> {
        Target {
            unit: &pref.unit,
            name: &pref.unit.name,
            live: &pref.record.path,
            root: &pref.root.path,
            expected_uuid: Some(pref.record.uuid),
            owner: Some((pref.record.owner_uid, pref.record.owner_gid)),
            stats_exclude: eff.stats_exclude(),
            sentinels: eff.sentinels.clone(),
        }
    }
}
