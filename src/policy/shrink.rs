//! Shrink guard: detect that a project lost a large part of its content.
//!
//! The comparison is against a high-water mark since the last acknowledgement, so a series of
//! small deletions trips the guard once the cumulative loss crosses the threshold.

use crate::config::ShrinkPolicy;
use crate::store::RefStats;
use crate::util::walk::TreeStats;

pub fn evaluate(
    reference: &RefStats,
    prev: Option<&TreeStats>,
    new: &TreeStats,
    p: &ShrinkPolicy,
    sentinels: &[String],
) -> Vec<String> {
    let mut reasons = Vec::new();
    if !p.enabled || !new.complete {
        return reasons;
    }
    let pct = |from: u64, to: u64| {
        if from == 0 { 0.0 } else { (from.saturating_sub(to)) as f64 / from as f64 }
    };
    let rf = reference.files;
    if rf >= 10 && (new.files as f64) <= p.catastrophic * rf as f64 {
        reasons.push(format!("catastrophic: files {rf} -> {} (snapshot {} had {rf})", new.files, reference.files_snap));
    } else if pct(rf, new.files) > p.ratio && rf.saturating_sub(new.files) >= p.min_files {
        reasons.push(format!(
            "files {rf} -> {} (-{:.0}%, high-water in snapshot {})",
            new.files,
            pct(rf, new.files) * 100.0,
            reference.files_snap
        ));
    }
    let rb = reference.bytes;
    if pct(rb, new.bytes) > p.ratio && rb.saturating_sub(new.bytes) >= p.min_bytes.0 {
        reasons.push(format!(
            "bytes {} -> {} (-{:.0}%, high-water in snapshot {})",
            crate::util::bytes::fmt_bytes(rb),
            crate::util::bytes::fmt_bytes(new.bytes),
            pct(rb, new.bytes) * 100.0,
            reference.bytes_snap
        ));
    }
    if let Some(prev) = prev {
        for s in sentinels {
            if prev.sentinels.get(s) == Some(&true) && new.sentinels.get(s) == Some(&false) {
                reasons.push(format!("sentinel {s} removed"));
            }
        }
    }
    reasons
}

pub fn raise(reference: &mut RefStats, new: &TreeStats, new_id: u64) {
    if !new.complete {
        return;
    }
    if new.files >= reference.files {
        reference.files = new.files;
        reference.files_snap = new_id;
    }
    if new.bytes >= reference.bytes {
        reference.bytes = new.bytes;
        reference.bytes_snap = new_id;
    }
}

pub fn reset(new: &TreeStats, new_id: u64) -> RefStats {
    RefStats { files: new.files, files_snap: new_id, bytes: new.bytes, bytes_snap: new_id }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::bytes::ByteSize;

    fn policy() -> ShrinkPolicy {
        ShrinkPolicy {
            enabled: true,
            ratio: 0.30,
            min_files: 100,
            min_bytes: ByteSize(50 << 20),
            catastrophic: 0.10,
            exclude: vec![".git".into()],
            sentinels: vec![".git".into()],
            thin_after_freeze: true,
        }
    }

    fn stats(files: u64, bytes: u64, git: bool) -> TreeStats {
        let mut s = TreeStats { files, bytes, complete: true, ..Default::default() };
        s.sentinels.insert(".git".into(), git);
        s
    }

    #[test]
    fn large_drop_freezes() {
        let base = stats(2000, 300 << 20, true);
        let r = reset(&base, 1);
        let v = evaluate(&r, Some(&base), &stats(1200, 180 << 20, true), &policy(), &[".git".into()]);
        assert_eq!(v.len(), 2, "{v:?}");
    }

    #[test]
    fn drip_deletion_trips_at_cumulative_threshold() {
        let mut r = reset(&stats(2000, 10 << 20, true), 1);
        let mut files = 2000u64;
        let mut prev = stats(files, 10 << 20, true);
        let mut tripped_at = None;
        for step in 1..=5 {
            files -= 240; // 12% of the original each step
            let new = stats(files, 10 << 20, true);
            let v = evaluate(&r, Some(&prev), &new, &policy(), &[".git".into()]);
            if !v.is_empty() {
                tripped_at = Some(step);
                break;
            }
            raise(&mut r, &new, step + 1);
            prev = new;
        }
        assert_eq!(tripped_at, Some(3), "36% cumulative loss crosses 30%");
    }

    #[test]
    fn small_projects_growth_and_sentinels() {
        let r = reset(&stats(50, 1 << 20, true), 1);
        assert!(evaluate(&r, None, &stats(20, 1 << 19, true), &policy(), &[]).is_empty(), "below absolute floors");
        assert!(
            !evaluate(&r, None, &stats(3, 1 << 19, true), &policy(), &[]).is_empty(),
            "catastrophic ignores floors"
        );
        let prev = stats(50, 1 << 20, true);
        let v = evaluate(&r, Some(&prev), &stats(50, 1 << 20, false), &policy(), &[".git".into()]);
        assert_eq!(v, vec!["sentinel .git removed"]);
        let mut r2 = r.clone();
        raise(&mut r2, &stats(80, 2 << 20, true), 9);
        assert_eq!((r2.files, r2.files_snap), (80, 9));
        let mut incomplete = stats(1, 1, true);
        incomplete.complete = false;
        assert!(evaluate(&r2, None, &incomplete, &policy(), &[]).is_empty());
    }
}
