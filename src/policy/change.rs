//! Change detection from subvolume transaction counters.
//!
//! A snapshot inherits the source's `ctransid` and snapshotting does not bump it, so equal
//! `ctransid` means no content change since that snapshot. `generation` is compared with the
//! snapshot's creation transaction as a second signal, because some metadata-only changes
//! (a bare chmod) move `generation` without moving `ctransid`.

use crate::btrfs::SubvolInfo;
use crate::store::{ProjectState, SnapshotMeta};
use crate::util::time::age;
use jiff::Timestamp;
use std::time::Duration;

pub fn identical(live: &SubvolInfo, newest: &SnapshotMeta) -> bool {
    live.ctransid == newest.source_ctransid && live.generation <= newest.snapshot_otransid
}

pub fn changed_since(live: &SubvolInfo, newest: Option<&SnapshotMeta>) -> bool {
    newest.is_none_or(|n| !identical(live, n))
}

/// A content change not caused by bpm itself: resets the idle clock.
pub fn user_changed(live: &SubvolInfo, st: &ProjectState) -> bool {
    live.ctransid > st.snap_ctransid.max(st.tool_ctransid)
}

#[derive(Debug, PartialEq, Eq)]
pub enum SnapDecision {
    Take(String),
    Skip(&'static str),
}

pub struct SnapInputs<'a> {
    pub live: &'a SubvolInfo,
    pub newest: Option<&'a SnapshotMeta>,
    pub now: Timestamp,
    pub min_interval: Duration,
    pub free: u64,
    pub min_free: u64,
}

pub fn decide_auto_snapshot(i: &SnapInputs) -> SnapDecision {
    if !changed_since(i.live, i.newest) {
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
    let from = i.newest.map(|n| n.source_ctransid.to_string()).unwrap_or_else(|| "none".into());
    SnapDecision::Take(format!("changed (ctransid {from} -> {})", i.live.ctransid))
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
    fn identity_rules() {
        let s = snap(50, 60, "2026-09-14T10:00:00Z");
        assert!(identical(&live(50, 60), &s), "right after snapshot: gen == otransid");
        assert!(!identical(&live(61, 61), &s), "content change");
        assert!(!identical(&live(50, 61), &s), "metadata-only change seen via generation");
        assert!(changed_since(&live(1, 1), None));
    }

    #[test]
    fn user_change_ignores_tool_changes() {
        let mut st = ProjectState { snap_ctransid: 50, ..Default::default() };
        assert!(user_changed(&live(55, 55), &st));
        st.tool_ctransid = 55;
        assert!(!user_changed(&live(55, 55), &st));
        assert!(user_changed(&live(56, 56), &st));
    }

    #[test]
    fn auto_snapshot_decision() {
        let s = snap(50, 60, "2026-09-14T10:00:00Z");
        let now: Timestamp = "2026-09-14T10:03:00Z".parse().unwrap();
        let mut i = SnapInputs {
            live: &live(70, 70),
            newest: Some(&s),
            now,
            min_interval: Duration::from_secs(300),
            free: 100 << 30,
            min_free: 20 << 30,
        };
        assert_eq!(decide_auto_snapshot(&i), SnapDecision::Skip("min_interval"));
        i.now = "2026-09-14T10:05:00Z".parse().unwrap();
        assert!(matches!(decide_auto_snapshot(&i), SnapDecision::Take(_)));
        i.free = 1 << 30;
        assert_eq!(decide_auto_snapshot(&i), SnapDecision::Skip("low free space"));
        let l = live(50, 60);
        i.live = &l;
        assert_eq!(decide_auto_snapshot(&i), SnapDecision::Skip("unchanged"));
    }
}
