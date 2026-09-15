//! Time helpers: ages, human durations, local display, selector parsing.

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use std::time::Duration;

/// Non-negative age of `ts` at `now` (future timestamps have age zero).
pub fn age(now: Timestamp, ts: Timestamp) -> Duration {
    let d = now.duration_since(ts);
    if d.is_negative() { Duration::ZERO } else { d.unsigned_abs() }
}

pub fn add(ts: Timestamp, d: Duration) -> Timestamp {
    ts.checked_add(SignedDuration::try_from(d).unwrap_or(SignedDuration::MAX)).unwrap_or(Timestamp::MAX)
}

pub fn sub(ts: Timestamp, d: Duration) -> Timestamp {
    ts.checked_sub(SignedDuration::try_from(d).unwrap_or(SignedDuration::MAX)).unwrap_or(Timestamp::MIN)
}

/// Compact duration: 45s, 12m, 5h, 3d, 9w.
pub fn fmt_age(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..172_800 => format!("{}h", s / 3600),
        172_800..1_209_600 => format!("{}d", s / 86_400),
        _ => format!("{}w", s / 604_800),
    }
}

pub fn fmt_local(ts: Timestamp, tz: &TimeZone) -> String {
    ts.to_zoned(tz.clone()).strftime("%Y-%m-%d %H:%M").to_string()
}

/// Parse "2026-09-14" (end of that local day) or "2026-09-14T10:00[:SS]" (local).
pub fn parse_local_datetime(s: &str, tz: &TimeZone) -> Option<Timestamp> {
    if !s.contains('T') && !s.contains(' ') {
        let date = s.parse::<jiff::civil::Date>().ok()?;
        let dt = date.at(23, 59, 59, 999_999_999);
        return dt.to_zoned(tz.clone()).ok().map(|z| z.timestamp());
    }
    let s = s.replace(' ', "T");
    let full = if s.len() == 16 { format!("{s}:00") } else { s };
    let dt: jiff::civil::DateTime = full.parse().ok()?;
    dt.to_zoned(tz.clone()).ok().map(|z| z.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ages() {
        let a: Timestamp = "2026-09-14T10:00:00Z".parse().unwrap();
        let b: Timestamp = "2026-09-14T12:30:00Z".parse().unwrap();
        assert_eq!(age(b, a), Duration::from_secs(9000));
        assert_eq!(age(a, b), Duration::ZERO);
        assert_eq!(fmt_age(Duration::from_secs(9000)), "2h");
        assert_eq!(fmt_age(Duration::from_secs(3 * 86400)), "3d");
    }
    #[test]
    fn selectors() {
        let tz = TimeZone::UTC;
        let t = parse_local_datetime("2026-09-14T10:00", &tz).unwrap();
        assert_eq!(t.to_string(), "2026-09-14T10:00:00Z");
        let d = parse_local_datetime("2026-09-14", &tz).unwrap();
        assert!(d > t);
    }
}

/// Serde for `Duration` as compact human strings ("5m", "14d", "8w"); parses anything humantime accepts.
pub mod dur {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn format(d: &Duration) -> String {
        let s = d.as_secs();
        if d.subsec_nanos() != 0 {
            return humantime::format_duration(*d).to_string();
        }
        for (unit, secs) in [("w", 604_800), ("d", 86_400), ("h", 3600), ("m", 60)] {
            if s != 0 && s % secs == 0 {
                return format!("{}{unit}", s / secs);
            }
        }
        format!("{s}s")
    }

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format(d))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        humantime::parse_duration(&s).map_err(|e| serde::de::Error::custom(format!("invalid duration {s:?}: {e}")))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn compact() {
            assert_eq!(format(&Duration::from_secs(60 * 86400)), "60d");
            assert_eq!(format(&Duration::from_secs(8 * 604_800)), "8w");
            assert_eq!(format(&Duration::from_secs(300)), "5m");
            assert_eq!(format(&Duration::from_secs(90)), "90s");
            assert_eq!(format(&Duration::ZERO), "0s");
        }
    }
}
