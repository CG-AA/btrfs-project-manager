//! Snapshot selectors: `42`, `latest`, `latest~2`, `2026-09-14`, `2026-09-14T10:00`, `held`.

use super::SnapshotMeta;
use crate::error::{not_found, usage};
use crate::util::time::parse_local_datetime;
use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;

#[derive(Clone, Debug, PartialEq)]
pub enum SnapSelector {
    Id(u64),
    Latest(usize),
    At(Timestamp),
    Held,
}

pub fn parse(s: &str, tz: &TimeZone) -> Result<SnapSelector> {
    let s = s.trim();
    if let Ok(id) = s.parse::<u64>() {
        return Ok(SnapSelector::Id(id));
    }
    if s == "latest" {
        return Ok(SnapSelector::Latest(0));
    }
    if let Some(n) = s.strip_prefix("latest~") {
        return n.parse().map(SnapSelector::Latest).map_err(|_| usage(format!("bad selector {s:?}")));
    }
    if s == "held" {
        return Ok(SnapSelector::Held);
    }
    parse_local_datetime(s, tz).map(SnapSelector::At).ok_or_else(|| {
        usage(format!(
            "bad snapshot selector {s:?} (use an id, latest, latest~N, held, YYYY-MM-DD or YYYY-MM-DDTHH:MM)"
        ))
    })
}

pub fn resolve<'a>(sel: &SnapSelector, snaps: &'a [SnapshotMeta]) -> Result<&'a SnapshotMeta> {
    let mut by_time: Vec<&SnapshotMeta> = snaps.iter().collect();
    by_time.sort_by_key(|m| (m.created, m.id));
    let found = match sel {
        SnapSelector::Id(id) => snaps.iter().find(|m| m.id == *id),
        SnapSelector::Latest(n) => by_time.iter().rev().nth(*n).copied(),
        SnapSelector::At(ts) => by_time.iter().rev().find(|m| m.created <= *ts).copied(),
        SnapSelector::Held => by_time.iter().rev().find(|m| m.hold).copied(),
    };
    found.ok_or_else(|| not_found(format!("no snapshot matches {sel:?}")))
}

pub fn resolve_str<'a>(s: &str, snaps: &'a [SnapshotMeta], tz: &TimeZone) -> Result<&'a SnapshotMeta> {
    resolve(&parse(s, tz)?, snaps)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selectors() {
        let tz = TimeZone::UTC;
        let mut snaps: Vec<SnapshotMeta> = (1..=4).map(|i| SnapshotMeta::sample(i, "p")).collect();
        snaps[1].hold = true;
        assert_eq!(resolve_str("3", &snaps, &tz).unwrap().id, 3);
        assert_eq!(resolve_str("latest", &snaps, &tz).unwrap().id, 4);
        assert_eq!(resolve_str("latest~1", &snaps, &tz).unwrap().id, 3);
        assert_eq!(resolve_str("held", &snaps, &tz).unwrap().id, 2);
        let t = snaps[2].created.strftime("%Y-%m-%dT%H:%M:%S").to_string();
        assert_eq!(resolve_str(&t, &snaps, &tz).unwrap().id, 3);
        assert!(resolve_str("99", &snaps, &tz).is_err());
        assert!(resolve_str("nonsense", &snaps, &tz).is_err());
    }
}
