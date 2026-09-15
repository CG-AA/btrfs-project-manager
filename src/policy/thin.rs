//! Retention: age windows with deterministic "oldest snapshot per bucket" representatives.

use crate::store::{Frozen, KindClass, SnapshotMeta, newest};
use crate::util::time::age;
use jiff::Timestamp;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Windows {
    pub keep_all: Duration,
    pub hourly: Duration,
    pub daily: Duration,
    pub weekly: Duration,
    pub safety_ttl: Duration,
}

impl From<&crate::config::ThinPolicy> for Windows {
    fn from(p: &crate::config::ThinPolicy) -> Self {
        Windows { keep_all: p.keep_all, hourly: p.hourly, daily: p.daily, weekly: p.weekly, safety_ttl: p.safety_ttl }
    }
}

impl From<&crate::config::ContainerCfg> for Windows {
    fn from(c: &crate::config::ContainerCfg) -> Self {
        Windows {
            keep_all: c.keep_all,
            hourly: Duration::ZERO,
            daily: c.daily,
            weekly: c.weekly,
            safety_ttl: Duration::from_secs(7 * 86400),
        }
    }
}

/// May policy (thinning or collapse) ever consider this snapshot for deletion?
pub fn policy_deletable(
    m: &SnapshotMeta,
    now: Timestamp,
    safety_ttl: Duration,
    frozen: Option<&Frozen>,
    thin_after_freeze: bool,
) -> bool {
    if m.hold {
        return false;
    }
    let class_ok = match m.kind.class() {
        KindClass::Thinnable => true,
        KindClass::Safety => age(now, m.created) > safety_ttl,
        KindClass::Keep => false,
    };
    let frozen_ok = match frozen {
        None => true,
        Some(f) => thin_after_freeze && m.created > f.since,
    };
    class_ok && frozen_ok
}

const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
const WEEK: i64 = 7 * DAY;
/// 1970-01-01 was a Thursday; shift so weeks start on Monday.
const WEEK_OFFSET: i64 = 4 * DAY;

pub fn select_deletions(
    snaps: &[SnapshotMeta],
    now: Timestamp,
    w: &Windows,
    frozen: Option<&Frozen>,
    thin_after_freeze: bool,
) -> Vec<u64> {
    let newest_id = newest(snaps).map(|m| m.id);
    let mut cands: Vec<&SnapshotMeta> = snaps
        .iter()
        .filter(|m| Some(m.id) != newest_id && policy_deletable(m, now, w.safety_ttl, frozen, thin_after_freeze))
        .collect();
    cands.sort_by_key(|m| (m.created, m.id));

    let mut keep: HashSet<u64> = cands.iter().filter(|m| age(now, m.created) <= w.keep_all).map(|m| m.id).collect();
    for (width, window, offset) in [(HOUR, w.hourly, 0), (DAY, w.daily, 0), (WEEK, w.weekly, WEEK_OFFSET)] {
        if window.is_zero() {
            continue;
        }
        let mut seen = HashSet::new();
        for m in &cands {
            if age(now, m.created) > window {
                continue;
            }
            let bucket = (m.created.as_second() - offset).div_euclid(width);
            if seen.insert(bucket) {
                keep.insert(m.id);
            }
        }
    }
    cands.iter().filter(|m| !keep.contains(&m.id)).map(|m| m.id).collect()
}

/// Collapse: everything policy may delete except the newest snapshot.
pub fn collapse_deletions(
    snaps: &[SnapshotMeta],
    now: Timestamp,
    safety_ttl: Duration,
    frozen: Option<&Frozen>,
) -> Vec<u64> {
    if frozen.is_some() {
        return vec![];
    }
    let newest_id = newest(snaps).map(|m| m.id);
    snaps
        .iter()
        .filter(|m| Some(m.id) != newest_id && policy_deletable(m, now, safety_ttl, None, false))
        .map(|m| m.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SnapshotKind;
    use crate::util::time::sub;

    fn windows() -> Windows {
        Windows {
            keep_all: Duration::from_secs(6 * 3600),
            hourly: Duration::from_secs(48 * 3600),
            daily: Duration::from_secs(14 * 86400),
            weekly: Duration::from_secs(8 * 7 * 86400),
            safety_ttl: Duration::from_secs(7 * 86400),
        }
    }

    /// One snapshot every 30 minutes for `days`, ending at `now`.
    fn series(now: Timestamp, days: u64) -> Vec<SnapshotMeta> {
        let n = days * 48;
        (0..n)
            .map(|i| {
                let mut m = SnapshotMeta::sample(i + 1, "p");
                m.created = sub(now, Duration::from_secs((n - 1 - i) * 1800));
                m.snapshot_otransid = 1000 + i;
                m
            })
            .collect()
    }

    fn kept(snaps: &[SnapshotMeta], now: Timestamp, w: &Windows) -> Vec<u64> {
        let del: HashSet<u64> = select_deletions(snaps, now, w, None, true).into_iter().collect();
        snaps.iter().map(|m| m.id).filter(|id| !del.contains(id)).collect()
    }

    #[test]
    fn windows_bound_counts() {
        let now: Timestamp = "2026-09-14T12:10:00Z".parse().unwrap();
        let snaps = series(now, 40);
        let k = kept(&snaps, now, &windows());
        // keep_all 6h at 30 min spacing = 13, hourly 48h ≈ 48, daily 14d ≈ 14, weekly: <= 6 in 40 days
        assert!(k.len() <= 13 + 48 + 14 + 7, "kept {}", k.len());
        assert!(k.len() >= 48, "kept {}", k.len());
        assert!(k.contains(&snaps.last().unwrap().id), "newest always kept");
        let oldest_kept = snaps.iter().find(|m| k.contains(&m.id)).unwrap();
        assert!(age(now, oldest_kept.created) > Duration::from_secs(30 * 86400), "weekly reach");
    }

    #[test]
    fn representatives_are_stable_between_ticks() {
        let now: Timestamp = "2026-09-14T12:10:00Z".parse().unwrap();
        let w = windows();
        let snaps = series(now, 20);
        let k1: HashSet<u64> = kept(&snaps, now, &w).into_iter().collect();
        let survivors: Vec<SnapshotMeta> = snaps.iter().filter(|m| k1.contains(&m.id)).cloned().collect();
        let later = crate::util::time::add(now, Duration::from_secs(300));
        let k2: HashSet<u64> = kept(&survivors, later, &w).into_iter().collect();
        let lost: Vec<&u64> = k1.difference(&k2).collect();
        assert!(lost.len() <= 2, "only window-edge snapshots may drop within 5 minutes, lost {lost:?}");
    }

    #[test]
    fn held_keep_kinds_and_safety_ttl() {
        let now: Timestamp = "2026-09-14T12:00:00Z".parse().unwrap();
        let mut snaps = series(now, 30);
        snaps[0].hold = true;
        snaps[1].kind = SnapshotKind::Manual;
        snaps[2].kind = SnapshotKind::Rollback; // 30 days old: past safety ttl
        let last = snaps.len() - 3;
        snaps[last].kind = SnapshotKind::Pre; // recent: inside safety ttl
        let del = select_deletions(&snaps, now, &windows(), None, true);
        assert!(!del.contains(&snaps[0].id));
        assert!(!del.contains(&snaps[1].id));
        assert!(!del.contains(&snaps[last].id));
        // idle project converges: newest + weeklies only, eventually one
        let far = crate::util::time::add(now, Duration::from_secs(70 * 86400));
        let del_far: HashSet<u64> = select_deletions(&snaps, far, &windows(), None, true).into_iter().collect();
        let remaining: Vec<u64> = snaps.iter().map(|m| m.id).filter(|i| !del_far.contains(i)).collect();
        assert_eq!(remaining, vec![snaps[0].id, snaps[1].id, snaps.last().unwrap().id]);
    }

    #[test]
    fn frozen_protects_everything_before_the_freeze() {
        let now: Timestamp = "2026-09-14T12:00:00Z".parse().unwrap();
        let snaps = series(now, 10);
        let frozen =
            Frozen { since: sub(now, Duration::from_secs(86400)), reasons: vec![], trigger_snap: None, ref_snap: None };
        for id in select_deletions(&snaps, now, &windows(), Some(&frozen), true) {
            let m = snaps.iter().find(|m| m.id == id).unwrap();
            assert!(m.created > frozen.since);
        }
        assert!(select_deletions(&snaps, now, &windows(), Some(&frozen), false).is_empty());
        assert!(collapse_deletions(&snaps, now, windows().safety_ttl, Some(&frozen)).is_empty());
    }

    #[test]
    fn collapse_keeps_newest_held_and_keep_kinds() {
        let now: Timestamp = "2026-09-14T12:00:00Z".parse().unwrap();
        let mut snaps = series(now, 2);
        snaps[3].hold = true;
        snaps[4].kind = SnapshotKind::Manual;
        let del: HashSet<u64> = collapse_deletions(&snaps, now, Duration::ZERO, None).into_iter().collect();
        let remaining: Vec<u64> = snaps.iter().map(|m| m.id).filter(|i| !del.contains(i)).collect();
        assert_eq!(remaining, vec![snaps[3].id, snaps[4].id, snaps.last().unwrap().id]);
    }
}
