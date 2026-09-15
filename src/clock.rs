//! Time source abstraction so lifecycle logic is testable.

use jiff::{SignedDuration, Timestamp};
use std::sync::Mutex;
use std::time::Duration;

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

pub struct FakeClock(Mutex<Timestamp>);

impl FakeClock {
    pub fn at(rfc3339: &str) -> Self {
        FakeClock(Mutex::new(rfc3339.parse().expect("valid timestamp")))
    }
    pub fn advance(&self, d: Duration) {
        let mut t = self.0.lock().unwrap();
        *t = t.checked_add(SignedDuration::try_from(d).unwrap()).unwrap();
    }
    pub fn set(&self, ts: Timestamp) {
        *self.0.lock().unwrap() = ts;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap()
    }
}
