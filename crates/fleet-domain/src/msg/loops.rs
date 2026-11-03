//! Loop and storm detection. Two agents can keep each other busy forever: A asks, B answers, A
//! answers the answer. Agents that know they are watched leave `in_reply_to` out, so a chain is
//! also detected by what happened before: a message from B to A right after one from A to B is a
//! reply whether it says so or not.
//!
//! Only a *genuine exchange* between two distinct tasks counts. A task that writes many messages
//! to a peer who never answers is not a loop (the per-sender quotas and rates stop it), and a task
//! cannot build a chain out of its own messages. Two limits apply per pair of tasks, across every
//! channel of a run:
//! * a *reply depth*: a message that ends a chain of replies longer than the limit;
//! * a *pair budget*: the number of times the two tasks changed direction (a message from one right
//!   after one from the other) before a person looks.
//!
//! Crossing either refuses the message and asks the caller to mute the sender (or the channel
//! when nobody else is there) and open a decision gate; this module only counts.

use crate::ids::TaskId;
use crate::msg::limits::MessagingLimits;
use std::collections::BTreeMap;

/// The two tasks of a pair in a fixed order, so `(a, b)` and `(b, a)` are one pair.
pub fn pair_key(a: &TaskId, b: &TaskId) -> (TaskId, TaskId) {
    if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

/// Which limit a message crossed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trip {
    ReplyDepth {
        depth: u32,
        limit: u32,
        pair: (TaskId, TaskId),
    },
    PairBudget {
        exchanges: u32,
        limit: u32,
        pair: (TaskId, TaskId),
    },
    /// A task keeps sending messages that are refused, too many in a minute.
    Flood {
        task: TaskId,
        refused: u32,
        limit: u32,
    },
    /// A task caused more refusals in all than it may, until a person looks.
    RefusalBudget {
        task: TaskId,
        refused: u32,
        limit: u32,
    },
}

impl Trip {
    /// The two tasks involved, when the limit is about a pair.
    pub fn pair(&self) -> Option<&(TaskId, TaskId)> {
        match self {
            Trip::ReplyDepth { pair, .. } | Trip::PairBudget { pair, .. } => Some(pair),
            Trip::Flood { .. } | Trip::RefusalBudget { .. } => None,
        }
    }

    /// One sentence for the log and the gate. Ids and numbers only: no peer text.
    pub fn describe(&self) -> String {
        match self {
            Trip::ReplyDepth { depth, limit, pair } => format!(
                "a reply chain between task {} and task {} reached depth {depth} (the limit is {limit})",
                pair.0, pair.1
            ),
            Trip::PairBudget {
                exchanges,
                limit,
                pair,
            } => format!(
                "task {} and task {} would have gone back and forth {exchanges} times (the limit is {limit})",
                pair.0, pair.1
            ),
            Trip::Flood {
                task,
                refused,
                limit,
            } => format!(
                "task {task} sent {refused} refused messages in a minute (the limit is {limit})"
            ),
            Trip::RefusalBudget {
                task,
                refused,
                limit,
            } => format!(
                "task {task} has had {refused} messages refused in all (the limit is {limit})"
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct PairState {
    /// Times the direction changed: a message from one task right after one from the other.
    alternations: u32,
    /// Who wrote the last message between the two, and how deep its chain was.
    last_from: Option<TaskId>,
    last_depth: u32,
}

/// What has passed between the tasks of one run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exchanges {
    pairs: BTreeMap<(TaskId, TaskId), PairState>,
}

impl Exchanges {
    /// The depth a message from `from` to `to` has because of what came before: one more than the
    /// last message between the two when that one was written by `to`, else zero.
    pub fn implicit_depth(&self, from: &TaskId, to: &TaskId) -> u32 {
        match self.pairs.get(&pair_key(from, to)) {
            Some(p) if p.last_from.as_ref() == Some(to) => p.last_depth.saturating_add(1),
            _ => 0,
        }
    }

    /// Times the two have changed direction so far.
    pub fn count(&self, a: &TaskId, b: &TaskId) -> u32 {
        self.pairs
            .get(&pair_key(a, b))
            .map_or(0, |p| p.alternations)
    }

    /// Whether a message from `from` to `to` answers one of `to`'s: a change of direction.
    fn alternates(&self, from: &TaskId, to: &TaskId) -> bool {
        self.pairs
            .get(&pair_key(from, to))
            .is_some_and(|p| p.last_from.as_ref() == Some(to))
    }

    /// The first limit the message would cross, if any. `depth` is the depth the message has
    /// (the larger of what `in_reply_to` says and [`Exchanges::implicit_depth`]).
    pub fn check(
        &self,
        from: &TaskId,
        to: &[TaskId],
        depth: u32,
        limits: &MessagingLimits,
    ) -> Option<Trip> {
        if depth > limits.max_reply_depth {
            let peer = to.first().unwrap_or(from);
            return Some(Trip::ReplyDepth {
                depth,
                limit: limits.max_reply_depth,
                pair: pair_key(from, peer),
            });
        }
        to.iter().find_map(|t| {
            let next = self
                .count(from, t)
                .saturating_add(u32::from(self.alternates(from, t)));
            (next > limits.max_pair_exchanges).then(|| Trip::PairBudget {
                exchanges: next,
                limit: limits.max_pair_exchanges,
                pair: pair_key(from, t),
            })
        })
    }

    /// Notes an accepted message.
    pub fn record(&mut self, from: &TaskId, to: &[TaskId], depth: u32) {
        for t in to {
            let turn = self.alternates(from, t);
            let p = self.pairs.entry(pair_key(from, t)).or_default();
            if turn {
                p.alternations = p.alternations.saturating_add(1);
            }
            p.last_from = Some(from.clone());
            p.last_depth = depth;
        }
    }

    /// A person unmuted something: a fresh budget.
    pub fn reset(&mut self) {
        self.pairs.clear();
    }

    pub fn pairs(&self) -> usize {
        self.pairs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }

    fn limits(depth: u32, budget: u32) -> MessagingLimits {
        MessagingLimits {
            max_reply_depth: depth,
            max_pair_exchanges: budget,
            ..MessagingLimits::default()
        }
    }

    #[test]
    fn a_pair_is_the_same_pair_in_either_order() {
        assert_eq!(pair_key(&t("a"), &t("b")), pair_key(&t("b"), &t("a")));
        assert_ne!(pair_key(&t("a"), &t("b")), pair_key(&t("a"), &t("c")));
    }

    #[test]
    fn ping_pong_gains_depth_without_any_in_reply_to() {
        let mut ex = Exchanges::default();
        let (a, b) = (t("a"), t("b"));
        assert_eq!(ex.implicit_depth(&a, &b), 0);
        ex.record(&a, std::slice::from_ref(&b), 0);
        assert_eq!(ex.implicit_depth(&b, &a), 1, "B answers A");
        assert_eq!(
            ex.implicit_depth(&a, &b),
            0,
            "A writing again is not an answer"
        );
        ex.record(&b, std::slice::from_ref(&a), 1);
        assert_eq!(ex.implicit_depth(&a, &b), 2);
        ex.record(&a, std::slice::from_ref(&b), 2);
        ex.record(&b, std::slice::from_ref(&a), 3);
        assert_eq!(ex.implicit_depth(&a, &b), 4);
    }

    #[test]
    fn a_third_task_does_not_continue_somebody_elses_chain() {
        let mut ex = Exchanges::default();
        ex.record(&t("a"), &[t("b")], 0);
        assert_eq!(ex.implicit_depth(&t("c"), &t("a")), 0);
        assert_eq!(ex.implicit_depth(&t("b"), &t("c")), 0);
    }

    #[test]
    fn the_depth_limit_trips_just_above_the_limit() {
        let ex = Exchanges::default();
        let l = limits(6, 100);
        assert_eq!(ex.check(&t("a"), &[t("b")], 6, &l), None);
        let trip = ex.check(&t("a"), &[t("b")], 7, &l).unwrap();
        assert_eq!(
            trip,
            Trip::ReplyDepth {
                depth: 7,
                limit: 6,
                pair: (t("a"), t("b"))
            }
        );
        assert!(trip.describe().contains("depth 7") && trip.describe().contains("limit is 6"));
    }

    #[test]
    fn one_way_traffic_never_counts_towards_the_pair_budget() {
        let mut ex = Exchanges::default();
        let l = limits(100, 3);
        for i in 0..200 {
            assert_eq!(
                ex.check(&t("a"), &[t("b")], 0, &l),
                None,
                "message {i} from a to a peer that never answers"
            );
            ex.record(&t("a"), &[t("b")], 0);
        }
        assert_eq!(ex.count(&t("a"), &t("b")), 0);
        assert_eq!(ex.implicit_depth(&t("a"), &t("b")), 0);
    }

    #[test]
    fn the_pair_budget_counts_changes_of_direction_and_trips_on_the_next_one() {
        let mut ex = Exchanges::default();
        let l = limits(100, 4);
        // a, b, a, b, a: four changes of direction
        for (i, (from, to)) in [("a", "b"), ("b", "a"), ("a", "b"), ("b", "a"), ("a", "b")]
            .iter()
            .enumerate()
        {
            assert_eq!(ex.check(&t(from), &[t(to)], 0, &l), None, "message {i}");
            ex.record(&t(from), &[t(to)], 0);
        }
        assert_eq!(ex.count(&t("a"), &t("b")), 4);
        // the same sender again is not a change of direction
        assert_eq!(ex.check(&t("a"), &[t("b")], 0, &l), None);
        // b answering would be the fifth
        let trip = ex.check(&t("b"), &[t("a")], 0, &l).unwrap();
        assert_eq!(
            trip,
            Trip::PairBudget {
                exchanges: 5,
                limit: 4,
                pair: (t("a"), t("b"))
            }
        );
        // another pair is untouched
        assert_eq!(ex.check(&t("a"), &[t("c")], 0, &l), None);
    }

    #[test]
    fn a_broadcast_is_counted_per_recipient_pair() {
        let mut ex = Exchanges::default();
        ex.record(&t("a"), &[t("b"), t("c")], 0);
        ex.record(&t("b"), &[t("a")], 1);
        ex.record(&t("c"), &[t("a")], 1);
        assert_eq!(ex.count(&t("a"), &t("b")), 1);
        assert_eq!(ex.count(&t("a"), &t("c")), 1);
        assert_eq!(ex.count(&t("b"), &t("c")), 0);
        assert_eq!(ex.pairs(), 2);
    }

    #[test]
    fn unmuting_gives_a_fresh_budget() {
        let mut ex = Exchanges::default();
        ex.record(&t("a"), &[t("b")], 3);
        ex.record(&t("b"), &[t("a")], 4);
        ex.reset();
        assert_eq!(ex.count(&t("a"), &t("b")), 0);
        assert_eq!(ex.implicit_depth(&t("b"), &t("a")), 0);
    }

    #[test]
    fn property_a_ping_pong_is_stopped_by_either_limit() {
        // whatever the limits, an exchange that never names in_reply_to cannot run on for ever
        for depth_limit in 1..8u32 {
            for budget in 1..8u32 {
                let l = limits(depth_limit, budget);
                let mut ex = Exchanges::default();
                let mut sent = 0u32;
                let mut from = t("a");
                let mut to = t("b");
                loop {
                    let depth = ex.implicit_depth(&from, &to);
                    if ex.check(&from, &[to.clone()], depth, &l).is_some() {
                        break;
                    }
                    ex.record(&from, &[to.clone()], depth);
                    sent += 1;
                    assert!(sent <= depth_limit.min(budget) + 2, "ran on: {sent}");
                    std::mem::swap(&mut from, &mut to);
                }
                assert!(sent >= 1);
                assert!(sent <= budget.min(depth_limit + 1) + 1);
            }
        }
    }

    #[test]
    fn every_trip_describes_itself_with_ids_and_numbers_only() {
        let flood = Trip::Flood {
            task: t("a"),
            refused: 20,
            limit: 20,
        };
        assert_eq!(
            flood.describe(),
            "task a sent 20 refused messages in a minute (the limit is 20)"
        );
        assert_eq!(flood.pair(), None, "a flood is about one task");
        let budget = Trip::RefusalBudget {
            task: t("a"),
            refused: 100,
            limit: 100,
        };
        assert_eq!(
            budget.describe(),
            "task a has had 100 messages refused in all (the limit is 100)"
        );
        assert_eq!(budget.pair(), None);
        let depth = Trip::ReplyDepth {
            depth: 7,
            limit: 6,
            pair: pair_key(&t("b"), &t("a")),
        };
        assert_eq!(depth.pair(), Some(&(t("a"), t("b"))));
        let pairs = Trip::PairBudget {
            exchanges: 13,
            limit: 12,
            pair: pair_key(&t("a"), &t("b")),
        };
        assert!(pairs.describe().contains("13") && pairs.describe().contains("12"));
        for trip in [flood, budget, depth, pairs] {
            assert!(trip.describe().chars().all(|c| !c.is_control()));
        }
    }
}
