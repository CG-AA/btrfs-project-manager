//! Change detection from subvolume transaction counters.
//!
//! A snapshot inherits the source's `ctransid` and snapshotting does not bump it, so equal
//! `ctransid` means no content change since that snapshot (a bare chmod moves it too, verified
//! on kernel 7.0). `generation` moving past the snapshot's creation transaction is a second
//! signal, for inode updates that skip `ctransid` (log replay after a crash). But changes that
//! touch no inode move it as well: a balance or metadata defragment rewrites tree blocks, and
//! only `generation` follows. `delta` reports that case as `TreeOnly`; `mechanics::observe::changed`
//! settles it by asking the kernel whether any inode item is newer than the snapshot.

use crate::btrfs::SubvolInfo;
use crate::store::{ProjectState, SnapshotMeta};
use crate::util::time::age;
use jiff::Timestamp;
use serde::Serialize;
use std::time::Duration;

/// What the transaction counters alone say about live versus a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delta {
    /// Same `ctransid`, and no tree block written since the snapshot was created.
    Unchanged,
    /// `ctransid` moved: an inode was updated.
    Content,
    /// Only `generation` moved: tree blocks were rewritten. Content may or may not differ.
    TreeOnly,
}

pub fn delta(live: &SubvolInfo, snap: &SnapshotMeta) -> Delta {
    if live.ctransid != snap.source_ctransid {
        Delta::Content
    } else if live.generation > snap.snapshot_otransid {
        Delta::TreeOnly
    } else {
        Delta::Unchanged
    }
}

/// The settled answer to "does live differ from this snapshot?".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Change {
    /// Identical: counters unchanged, or only tree blocks were rewritten (balance).
    None,
    /// `ctransid` moved.
    Content,
    /// Only `generation` moved, and an inode item is newer than the snapshot.
    Inode,
    /// Only `generation` moved and the inode check could not run (needs root). Treated as a
    /// change, and never as proof of identity.
    Unverified,
}

impl Change {
    pub fn identical(self) -> bool {
        self == Change::None
    }
}

/// A content change since the last observation not caused by bpm itself: resets the idle clock.
/// Snapshots do not count as observations, so a change captured by a hook or manual snapshot
/// before the tick saw it is still activity.
pub fn user_changed(live: &SubvolInfo, st: &ProjectState) -> bool {
    let seen = if st.seen_ctransid == 0 { st.snap_ctransid } else { st.seen_ctransid };
    live.ctransid > seen.max(st.tool_ctransid)
}

#[derive(Debug, PartialEq, Eq)]
pub enum SnapDecision {
    Take(String),
    Skip(&'static str),
}

pub struct SnapInputs<'a> {
    pub live: &'a SubvolInfo,
    pub newest: Option<&'a SnapshotMeta>,
    /// `live` against `newest`, settled by `mechanics::observe::changed`.
    pub change: Change,
    pub now: Timestamp,
    pub min_interval: Duration,
    pub free: u64,
    pub min_free: u64,
}

pub fn decide_auto_snapshot(i: &SnapInputs) -> SnapDecision {
    if i.change.identical() {
        return SnapDecision::Skip("unchanged");
    }
    if let Some(n) = i.newest {
        if age(i.now, n.created) < i.min_interval {
            return SnapDecision::Skip("min_interval");
        }
    }
    if i.free < i.min_free {
        return SnapDecision::Skip("low free space");
    }
    let reason = match (i.change, i.newest) {
        (Change::Inode, Some(n)) => {
            format!("changed (inode newer than snapshot #{}; generation {})", n.id, i.live.generation)
        }
        (Change::Unverified, Some(n)) => {
            format!("metadata rewritten since snapshot #{}; content check unavailable", n.id)
        }
        _ => {
            let from = i.newest.map(|n| n.source_ctransid.to_string()).unwrap_or_else(|| "none".into());
            format!("changed (ctransid {from} -> {})", i.live.ctransid)
        }
    };
    SnapDecision::Take(reason)
}

#[cfg(test)]
pub(crate) fn live(ctransid: u64, generation: u64) -> SubvolInfo {
    SubvolInfo {
        id: 300,
        name: "p".into(),
        parent_id: 5,
        generation,
        flags: 0,
        uuid: crate::btrfs::Uuid([1; 16]),
        parent_uuid: None,
        received_uuid: None,
        ctransid,
        otransid: 10,
        ctime: None,
        otime: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(ct: u64, ot: u64, created: &str) -> SnapshotMeta {
        let mut m = SnapshotMeta::sample(1, "p");
        m.source_ctransid = ct;
        m.snapshot_otransid = ot;
        m.created = created.parse().unwrap();
        m
    }

    #[test]
    fn delta_rules() {
        let s = snap(50, 60, "2026-09-14T10:00:00Z");
        assert_eq!(delta(&live(50, 60), &s), Delta::Unchanged, "right after snapshot: gen == otransid");
        assert_eq!(delta(&live(50, 59), &s), Delta::Unchanged);
        assert_eq!(delta(&live(61, 61), &s), Delta::Content, "content change");
        assert_eq!(delta(&live(50, 61), &s), Delta::TreeOnly, "balance, or an inode update that skipped ctransid");
        assert!(Change::None.identical());
        for c in [Change::Content, Change::Inode, Change::Unverified] {
            assert!(!c.identical(), "{c:?}");
        }
    }

    #[test]
    fn user_change_ignores_tool_changes() {
        let mut st = ProjectState { seen_ctransid: 50, ..Default::default() };
        assert!(user_changed(&live(55, 55), &st));
        st.tool_ctransid = 55;
        assert!(!user_changed(&live(55, 55), &st));
        assert!(user_changed(&live(56, 56), &st));
    }

    #[test]
    fn user_change_is_measured_from_observation_not_snapshots() {
        // an old state file: only snap_ctransid is known
        let st = ProjectState { snap_ctransid: 50, ..Default::default() };
        assert!(user_changed(&live(55, 55), &st));
        // a hook snapshot may have captured ctransid 60, but the tick last observed 50
        let st = ProjectState { snap_ctransid: 60, seen_ctransid: 50, ..Default::default() };
        assert!(user_changed(&live(60, 60), &st));
    }

    #[test]
    fn auto_snapshot_decision() {
        let s = snap(50, 60, "2026-09-14T10:00:00Z");
        let now: Timestamp = "2026-09-14T10:03:00Z".parse().unwrap();
        let mut i = SnapInputs {
            live: &live(70, 70),
            newest: Some(&s),
            change: Change::Content,
            now,
            min_interval: Duration::from_secs(300),
            free: 100 << 30,
            min_free: 20 << 30,
        };
        assert_eq!(decide_auto_snapshot(&i), SnapDecision::Skip("min_interval"));
        i.now = "2026-09-14T10:05:00Z".parse().unwrap();
        assert_eq!(decide_auto_snapshot(&i), SnapDecision::Take("changed (ctransid 50 -> 70)".into()));
        i.free = 1 << 30;
        assert_eq!(decide_auto_snapshot(&i), SnapDecision::Skip("low free space"));
        i.free = 100 << 30;
        let l = live(50, 61);
        i.live = &l;
        i.change = Change::None;
        assert_eq!(decide_auto_snapshot(&i), SnapDecision::Skip("unchanged"), "balance only");
        i.change = Change::Inode;
        assert_eq!(
            decide_auto_snapshot(&i),
            SnapDecision::Take("changed (inode newer than snapshot #1; generation 61)".into())
        );
        i.change = Change::Unverified;
        assert_eq!(
            decide_auto_snapshot(&i),
            SnapDecision::Take("metadata rewritten since snapshot #1; content check unavailable".into())
        );
        i.newest = None;
        i.change = Change::Content;
        assert_eq!(decide_auto_snapshot(&i), SnapDecision::Take("changed (ctransid none -> 50)".into()));
    }
}
