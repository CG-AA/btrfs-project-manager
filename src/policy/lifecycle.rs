//! Idle stages. Orphaned and archived are set by the executor, not by idle time.

use crate::config::LifecyclePolicy;
use crate::store::Stage;
use std::time::Duration;

pub fn stage_for_idle(idle: Duration, p: &LifecyclePolicy) -> Stage {
    if idle >= p.cold_after {
        Stage::Cold
    } else if idle >= p.dormant_after {
        Stage::Dormant
    } else {
        Stage::Active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stages() {
        let p = LifecyclePolicy {
            dormant_after: Duration::from_secs(14 * 86400),
            cold_after: Duration::from_secs(60 * 86400),
            adopt_grace: Duration::from_secs(86400),
        };
        assert_eq!(stage_for_idle(Duration::from_secs(13 * 86400), &p), Stage::Active);
        assert_eq!(stage_for_idle(Duration::from_secs(14 * 86400), &p), Stage::Dormant);
        assert_eq!(stage_for_idle(Duration::from_secs(61 * 86400), &p), Stage::Cold);
    }
}
