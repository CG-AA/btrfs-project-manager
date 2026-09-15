//! Human byte sizes ("20G", "50M") for config and output.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ByteSize(pub u64);

impl FromStr for ByteSize {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let n: f64 = num.parse().map_err(|_| format!("invalid size: {s:?}"))?;
        let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1,
            "k" | "kb" | "kib" => 1 << 10,
            "m" | "mb" | "mib" => 1 << 20,
            "g" | "gb" | "gib" => 1 << 30,
            "t" | "tb" | "tib" => 1 << 40,
            other => return Err(format!("invalid size unit {other:?} in {s:?}")),
        };
        Ok(ByteSize((n * mult as f64).round() as u64))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&fmt_bytes(self.0))
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&fmt_bytes(self.0))
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(n) => Ok(ByteSize(n)),
            Raw::Str(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else if v >= 10.0 {
        format!("{v:.0}{}", UNITS[i])
    } else {
        format!("{v:.1}{}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_and_format() {
        assert_eq!("20G".parse::<ByteSize>().unwrap().0, 20 << 30);
        assert_eq!("50M".parse::<ByteSize>().unwrap().0, 50 << 20);
        assert_eq!("1.5k".parse::<ByteSize>().unwrap().0, 1536);
        assert_eq!("123".parse::<ByteSize>().unwrap().0, 123);
        assert!("12Q".parse::<ByteSize>().is_err());
        assert_eq!(fmt_bytes(1536), "1.5K");
        assert_eq!(fmt_bytes(20 << 30), "20G");
    }
}
