//! The rate gate: how often fleet asks the forge, and what it does when the forge says "enough".
//!
//! Three things can stop a call before it starts, and each has a name in the status:
//! the forge said so (`limited`: a 403 or 429 answer, or the remaining budget the forge reports
//! fell to the reserve), our own budget for the hour is used up (`budget`), or nothing
//! stops it (`ok`). A refused call costs nothing and spawns nothing: the caller gets the time at
//! which asking again makes sense. Back-off doubles with every consecutive limit and carries a
//! jitter that the caller draws (the domain draws no random numbers), so several daemons do not
//! come back together. The gate never spins: it never waits itself, it only answers.

use crate::response::RateHeaders;
use fleet_domain::value::Value;

const HOUR_MS: u64 = 3_600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateConfig {
    /// Requests fleet allows itself per hour (a fixed window that starts with the first call).
    pub budget_per_hour: u32,
    /// Calls stop when the forge reports this many requests or fewer remaining.
    pub reserve: u32,
    /// First back-off after a limit answer; doubles per consecutive limit.
    pub base_backoff_ms: u64,
    /// Longest back-off that is computed (an explicit hint of the forge may be longer).
    pub max_backoff_ms: u64,
}

impl Default for RateConfig {
    fn default() -> RateConfig {
        RateConfig {
            budget_per_hour: 300,
            reserve: 10,
            base_backoff_ms: 30_000,
            max_backoff_ms: 15 * 60_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockWhy {
    /// The forge said enough.
    Limited,
    /// fleet's own budget for the hour is used up.
    Budget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    Go,
    /// Do not call; asking again before `until_ms` is pointless.
    Blocked {
        until_ms: u64,
        why: BlockWhy,
    },
}

#[derive(Clone, Debug)]
pub struct RateGate {
    cfg: RateConfig,
    window_start_ms: Option<u64>,
    used: u32,
    blocked_until_ms: u64,
    strikes: u32,
    limit: Option<u64>,
    remaining: Option<u64>,
    reset_ms: Option<u64>,
}

/// What `fleet github status` shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateStatus {
    pub state: &'static str,
    pub until_ms: Option<u64>,
    pub used: u32,
    pub budget: u32,
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub strikes: u32,
}

impl RateStatus {
    pub fn to_value(&self) -> Value {
        let opt = |n: Option<u64>| n.map_or(Value::Null, Value::int);
        Value::object([
            ("state", Value::str(self.state)),
            ("until_ms", opt(self.until_ms)),
            ("used", Value::int(self.used)),
            ("budget", Value::int(self.budget)),
            ("limit", opt(self.limit)),
            ("remaining", opt(self.remaining)),
            ("strikes", Value::int(self.strikes)),
        ])
    }
}

impl RateGate {
    pub fn new(cfg: RateConfig) -> RateGate {
        RateGate {
            cfg,
            window_start_ms: None,
            used: 0,
            blocked_until_ms: 0,
            strikes: 0,
            limit: None,
            remaining: None,
            reset_ms: None,
        }
    }

    fn roll_window(&mut self, now_ms: u64) {
        match self.window_start_ms {
            Some(start) if now_ms.saturating_sub(start) < HOUR_MS => {}
            _ => {
                self.window_start_ms = Some(now_ms);
                self.used = 0;
            }
        }
    }

    /// Asks whether a call may start now. A call that may start is counted.
    pub fn admit(&mut self, now_ms: u64) -> Admit {
        if now_ms < self.blocked_until_ms {
            return Admit::Blocked {
                until_ms: self.blocked_until_ms,
                why: BlockWhy::Limited,
            };
        }
        self.roll_window(now_ms);
        if self.used >= self.cfg.budget_per_hour {
            let start = self.window_start_ms.unwrap_or(now_ms);
            return Admit::Blocked {
                until_ms: start.saturating_add(HOUR_MS),
                why: BlockWhy::Budget,
            };
        }
        self.used = self.used.saturating_add(1);
        Admit::Go
    }

    /// Takes what any answer says about the limit. An answer that leaves no more than the reserve
    /// blocks calls until the window the forge names resets.
    pub fn observe(&mut self, now_ms: u64, h: &RateHeaders) {
        if h.limit.is_some() {
            self.limit = h.limit;
        }
        if let Some(remaining) = h.remaining {
            self.remaining = Some(remaining);
            if let Some(reset) = h.reset_epoch_s {
                let reset_ms = reset.saturating_mul(1000);
                self.reset_ms = Some(reset_ms);
                if remaining <= u64::from(self.cfg.reserve) && reset_ms > now_ms {
                    self.blocked_until_ms = self.blocked_until_ms.max(reset_ms);
                }
            }
            if remaining > u64::from(self.cfg.reserve) {
                self.strikes = 0;
            }
        }
    }

    /// The forge said enough. Returns the time before which no call is made: the larger of what
    /// the forge asked for and the doubling back-off, plus up to 25 % of it chosen by `jitter`
    /// (0..=1000, drawn by the caller).
    pub fn limited(&mut self, now_ms: u64, h: &RateHeaders, jitter: u32) -> u64 {
        self.observe(now_ms, h);
        let backoff = self
            .cfg
            .base_backoff_ms
            .saturating_mul(1u64 << self.strikes.min(16))
            .min(self.cfg.max_backoff_ms);
        let hint = h
            .retry_after_s
            .map(|s| s.saturating_mul(1000))
            .or_else(|| {
                h.reset_epoch_s
                    .map(|r| r.saturating_mul(1000).saturating_sub(now_ms))
            })
            .unwrap_or(0);
        let wait = hint.max(backoff);
        let jitter = u64::from(jitter.min(1000));
        let wait = wait.saturating_add(wait / 4 * jitter / 1000);
        let until = now_ms.saturating_add(wait);
        self.blocked_until_ms = self.blocked_until_ms.max(until);
        self.strikes = self.strikes.saturating_add(1);
        self.blocked_until_ms
    }

    pub fn status(&self, now_ms: u64) -> RateStatus {
        let (state, until_ms) = if now_ms < self.blocked_until_ms {
            ("limited", Some(self.blocked_until_ms))
        } else if self.used >= self.cfg.budget_per_hour
            && self
                .window_start_ms
                .is_some_and(|s| now_ms.saturating_sub(s) < HOUR_MS)
        {
            (
                "budget",
                self.window_start_ms.map(|s| s.saturating_add(HOUR_MS)),
            )
        } else {
            ("ok", None)
        };
        RateStatus {
            state,
            until_ms,
            used: self.used,
            budget: self.cfg.budget_per_hour,
            limit: self.limit,
            remaining: self.remaining,
            strikes: self.strikes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RateConfig {
        RateConfig {
            budget_per_hour: 3,
            reserve: 5,
            base_backoff_ms: 1000,
            max_backoff_ms: 8000,
        }
    }

    fn none() -> RateHeaders {
        RateHeaders::default()
    }

    #[test]
    fn the_budget_is_counted_per_hour_and_comes_back() {
        let mut g = RateGate::new(cfg());
        for _ in 0..3 {
            assert_eq!(g.admit(1_000), Admit::Go);
        }
        assert_eq!(
            g.admit(2_000),
            Admit::Blocked {
                until_ms: 1_000 + HOUR_MS,
                why: BlockWhy::Budget
            }
        );
        assert_eq!(g.status(2_000).state, "budget");
        assert_eq!(g.admit(1_000 + HOUR_MS), Admit::Go);
        assert_eq!(g.status(1_000 + HOUR_MS).used, 1);
    }

    #[test]
    fn a_limit_blocks_calls_and_the_back_off_doubles_up_to_the_cap() {
        let mut g = RateGate::new(cfg());
        assert_eq!(g.admit(0), Admit::Go);
        let first = g.limited(0, &none(), 0);
        assert_eq!(first, 1000);
        assert_eq!(
            g.admit(500),
            Admit::Blocked {
                until_ms: 1000,
                why: BlockWhy::Limited
            }
        );
        assert_eq!(g.status(500).state, "limited");
        let second = g.limited(1000, &none(), 0);
        assert_eq!(second, 1000 + 2000);
        let third = g.limited(3000, &none(), 0);
        assert_eq!(third, 3000 + 4000);
        let fourth = g.limited(7000, &none(), 0);
        assert_eq!(fourth, 7000 + 8000);
        let fifth = g.limited(15_000, &none(), 0);
        assert_eq!(fifth, 15_000 + 8000, "capped");
        assert_eq!(g.status(15_000).strikes, 5);
    }

    #[test]
    fn the_forges_hint_wins_when_it_is_longer_and_jitter_only_adds() {
        let mut g = RateGate::new(cfg());
        let h = RateHeaders {
            retry_after_s: Some(60),
            ..none()
        };
        assert_eq!(g.limited(0, &h, 0), 60_000);
        let mut g = RateGate::new(cfg());
        assert_eq!(g.limited(0, &h, 1000), 60_000 + 15_000);
        let mut g = RateGate::new(cfg());
        assert_eq!(g.limited(0, &h, 5000), 75_000, "jitter is clamped");
        let mut g = RateGate::new(cfg());
        let reset = RateHeaders {
            reset_epoch_s: Some(100),
            ..none()
        };
        assert_eq!(g.limited(10_000, &reset, 0), 10_000 + 90_000);
        // a reset in the past does not shorten the back-off
        let mut g = RateGate::new(cfg());
        let past = RateHeaders {
            reset_epoch_s: Some(1),
            ..none()
        };
        assert_eq!(g.limited(50_000, &past, 0), 51_000);
    }

    #[test]
    fn a_nearly_empty_window_stops_calls_until_it_resets() {
        let mut g = RateGate::new(cfg());
        let h = RateHeaders {
            limit: Some(5000),
            remaining: Some(5),
            reset_epoch_s: Some(100),
            ..none()
        };
        g.observe(10_000, &h);
        assert_eq!(
            g.admit(20_000),
            Admit::Blocked {
                until_ms: 100_000,
                why: BlockWhy::Limited
            }
        );
        assert_eq!(g.status(20_000).remaining, Some(5));
        assert_eq!(g.status(20_000).limit, Some(5000));
        assert_eq!(g.admit(100_000), Admit::Go);
        // plenty left: nothing blocks and the strikes are forgotten
        let mut g = RateGate::new(cfg());
        g.limited(0, &none(), 0);
        g.observe(
            5_000,
            &RateHeaders {
                remaining: Some(4000),
                reset_epoch_s: Some(9999),
                ..none()
            },
        );
        assert_eq!(g.status(5_000).strikes, 0);
    }

    #[test]
    fn a_blocked_gate_does_not_count_calls() {
        let mut g = RateGate::new(cfg());
        g.limited(0, &none(), 0);
        for _ in 0..10 {
            assert!(matches!(g.admit(10), Admit::Blocked { .. }));
        }
        assert_eq!(g.status(10).used, 0);
    }

    #[test]
    fn the_status_is_a_document() {
        let g = RateGate::new(cfg());
        let v = g.status(0).to_value();
        assert_eq!(v.get("state").and_then(Value::as_str), Some("ok"));
        assert_eq!(v.get("budget").and_then(Value::as_u64), Some(3));
        assert!(v.get("until_ms").is_some_and(Value::is_null));
    }
}
