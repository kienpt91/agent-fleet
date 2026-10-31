//! Rate limiting with an injected clock: a sliding window over the times of recent events. The
//! caller passes `now_ms`; nothing here reads a clock.

use std::collections::VecDeque;

/// Length of the window the per-minute limits use.
pub const MINUTE_MS: u64 = 60_000;

/// The times (milliseconds) of the most recent events, newest last.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlidingWindow {
    stamps: VecDeque<u64>,
}

impl SlidingWindow {
    /// Events that happened less than `window_ms` before `now_ms`. A stamp in the future (a
    /// clock that went backwards) counts as recent: a clock step must not reopen a limit.
    pub fn count(&self, now_ms: u64, window_ms: u64) -> usize {
        self.stamps
            .iter()
            .filter(|t| now_ms.saturating_sub(**t) < window_ms)
            .count()
    }

    /// Whether one more event may happen now without more than `max` in the window.
    pub fn allows(&self, now_ms: u64, window_ms: u64, max: usize) -> bool {
        self.count(now_ms, window_ms) < max
    }

    /// Notes an event. Stamps older than the window are dropped, and at most `cap` are kept, so
    /// the window cannot grow with the number of events.
    pub fn record(&mut self, now_ms: u64, window_ms: u64, cap: usize) {
        while self
            .stamps
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= window_ms)
        {
            self.stamps.pop_front();
        }
        self.stamps.push_back(now_ms);
        while self.stamps.len() > cap.max(1) {
            self.stamps.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic generator, so the property checks need no dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    #[test]
    fn a_fresh_window_allows_up_to_the_limit_and_no_more() {
        let mut w = SlidingWindow::default();
        for i in 0..3 {
            assert!(w.allows(1000 + i, MINUTE_MS, 3), "event {i}");
            w.record(1000 + i, MINUTE_MS, 16);
        }
        assert!(!w.allows(1003, MINUTE_MS, 3));
        assert_eq!(w.count(1003, MINUTE_MS), 3);
    }

    #[test]
    fn events_leave_the_window_exactly_when_it_has_passed() {
        let mut w = SlidingWindow::default();
        w.record(0, MINUTE_MS, 16);
        w.record(10_000, MINUTE_MS, 16);
        assert_eq!(w.count(59_999, MINUTE_MS), 2);
        assert_eq!(w.count(60_000, MINUTE_MS), 1, "the first is a minute old");
        assert_eq!(w.count(70_000, MINUTE_MS), 0);
        assert!(w.allows(70_000, MINUTE_MS, 1));
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_reopen_the_limit() {
        let mut w = SlidingWindow::default();
        w.record(100_000, MINUTE_MS, 16);
        w.record(100_001, MINUTE_MS, 16);
        assert_eq!(w.count(1, MINUTE_MS), 2);
        assert!(!w.allows(1, MINUTE_MS, 2));
    }

    #[test]
    fn the_window_never_holds_more_than_its_cap() {
        let mut w = SlidingWindow::default();
        for i in 0..1000 {
            w.record(i, u64::MAX, 8);
        }
        assert_eq!(w.len(), 8);
        assert!(!w.is_empty());
    }

    #[test]
    fn property_a_limit_is_never_exceeded_whatever_the_arrival_times() {
        for seed in 1..40u64 {
            let mut rng = Lcg(seed);
            let max = 1 + (rng.next() % 6) as usize;
            let mut w = SlidingWindow::default();
            let mut now = 0u64;
            let mut accepted: Vec<u64> = Vec::new();
            for _ in 0..400 {
                now += rng.next() % 4000;
                if w.allows(now, MINUTE_MS, max) {
                    w.record(now, MINUTE_MS, max + 1);
                    accepted.push(now);
                }
                // no minute-long span holds more than `max` accepted events
                let recent = accepted
                    .iter()
                    .filter(|t| now.saturating_sub(**t) < MINUTE_MS)
                    .count();
                assert!(recent <= max, "seed {seed}: {recent} > {max} at {now}");
            }
            assert!(!accepted.is_empty(), "seed {seed}");
        }
    }
}
