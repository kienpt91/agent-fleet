//! Wall-clock adapter for the `Clock` port.

use fleet_app::ports::Clock;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // A clock set before 1970 reads as 0 instead of failing; the value only orders events
        // and expires approvals.
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => u64::try_from(d.as_millis()).unwrap_or(u64::MAX),
            Err(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_plausible_time_that_does_not_go_backwards() {
        let c = SystemClock;
        let a = c.now_ms();
        let b = c.now_ms();
        // After 2020-01-01.
        assert!(a > 1_577_836_800_000);
        assert!(b >= a || a - b < 1000);
    }
}
