//! The ranking rule of a comparison: advisory, transparent and total.
//!
//! A rule is an ordered list of named criteria. Members are sorted by those criteria one after the
//! other; members that no criterion tells apart are ordered by task id, so the order is always
//! strict. The only inputs are the fields of [`RankInput`], which the comparison prints for every
//! member: there are no weights, no scores and nothing that is not shown. The result is a
//! suggestion for a person; nothing in fleet acts on it.

use crate::diff::ScopeState;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, TaskId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Criterion {
    /// The member's task completed (a failed or unfinished one ranks below).
    Completed,
    /// Every mandatory check passed in fleet's own run, for exactly the commit and tree the
    /// member's branch has now.
    Verified,
    /// No path outside the allow globs of the contract's mandatory `diff` checks was touched.
    InScope,
    /// Fewer changed lines (inserted plus deleted) first; an unknown size last.
    SmallerDiff,
    /// Shorter time from the first dispatch to the end first; an unknown time last.
    ShorterTime,
    /// Fewer attempts first.
    FewerAttempts,
}

impl Criterion {
    pub const ALL: [Criterion; 6] = [
        Criterion::Completed,
        Criterion::Verified,
        Criterion::InScope,
        Criterion::SmallerDiff,
        Criterion::ShorterTime,
        Criterion::FewerAttempts,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Criterion::Completed => "completed",
            Criterion::Verified => "verified",
            Criterion::InScope => "in-scope",
            Criterion::SmallerDiff => "diff",
            Criterion::ShorterTime => "time",
            Criterion::FewerAttempts => "attempts",
        }
    }

    pub fn parse(s: &str) -> Option<Criterion> {
        Criterion::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// One phrase that says which member comes first.
    pub fn describe(self) -> &'static str {
        match self {
            Criterion::Completed => "members whose task completed before those whose did not",
            Criterion::Verified => {
                "members whose mandatory checks all passed in fleet's own run for the branch's current commit and tree before those that did not"
            }
            Criterion::InScope => {
                "members whose paths are known to be inside the allow globs of the contract's mandatory diff checks, then those whose scope is not known (no such check, or the diff could not be read in full), then those that touched a path outside"
            }
            Criterion::SmallerDiff => {
                "smaller diff first (lines inserted plus deleted, a binary file counts 100, a mode or type change 10); unknown size last"
            }
            Criterion::ShorterTime => {
                "shorter time from first dispatch to the end first; unknown time last"
            }
            Criterion::FewerAttempts => "fewer attempts first",
        }
    }
}

impl std::fmt::Display for Criterion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a member stands where it does relative to the one ranked just above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecidedBy {
    /// It is first.
    First,
    /// The first criterion in which it differs from the member above.
    Criterion(Criterion),
    /// No criterion told them apart; the task id did.
    TaskId,
}

impl DecidedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            DecidedBy::First => "first",
            DecidedBy::Criterion(c) => c.as_str(),
            DecidedBy::TaskId => "task id",
        }
    }
}

/// An ordered list of criteria, each used once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RankRule {
    order: Vec<Criterion>,
}

impl Default for RankRule {
    /// Completed members first, then verified ones, then those inside the contract's scope, then
    /// the smaller diff, then the shorter time.
    fn default() -> RankRule {
        RankRule {
            order: vec![
                Criterion::Completed,
                Criterion::Verified,
                Criterion::InScope,
                Criterion::SmallerDiff,
                Criterion::ShorterTime,
            ],
        }
    }
}

impl RankRule {
    /// A rule from criterion names; at least one, each known and used once.
    pub fn parse(names: &[&str]) -> Result<RankRule> {
        if names.is_empty() {
            return Err(FleetError::invalid(
                "a ranking rule needs at least one criterion",
            ));
        }
        let mut order = Vec::with_capacity(names.len());
        for name in names {
            let c = Criterion::parse(name).ok_or_else(|| {
                FleetError::invalid(format!(
                    "unknown ranking criterion {} (known: {})",
                    show(name),
                    Criterion::ALL.map(Criterion::as_str).join(", ")
                ))
            })?;
            if order.contains(&c) {
                return Err(FleetError::invalid(format!(
                    "ranking criterion {c} is used twice"
                )));
            }
            order.push(c);
        }
        Ok(RankRule { order })
    }

    pub fn criteria(&self) -> &[Criterion] {
        &self.order
    }

    /// The rule as one line, with nothing left out.
    pub fn formula(&self) -> String {
        let steps: Vec<&str> = self.order.iter().map(|c| c.as_str()).collect();
        format!(
            "sort by ({}), then by task id a-z; advisory only, nothing is merged or picked from it",
            steps.join(", ")
        )
    }
}

/// The facts the ranking looks at, for one member: everything else about a member is invisible to
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RankInput {
    pub task: TaskId,
    pub completed: bool,
    pub verified: bool,
    pub scope: ScopeState,
    /// Lines inserted plus deleted; `None` when the diff could not be read.
    pub diff_size: Option<u64>,
    pub duration_ms: Option<u64>,
    pub attempts: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ranked {
    pub task: TaskId,
    /// 1 is the best.
    pub position: usize,
    pub decided_by: DecidedBy,
}

/// Smaller sorts first. Unknown values sort after every known one.
fn key_of(c: Criterion, i: &RankInput) -> u64 {
    let yes_first = |yes: bool| u64::from(!yes);
    match c {
        Criterion::Completed => yes_first(i.completed),
        Criterion::Verified => yes_first(i.verified),
        Criterion::InScope => match i.scope {
            ScopeState::In => 0,
            ScopeState::Unknown => 1,
            ScopeState::Out => 2,
        },
        Criterion::SmallerDiff => i.diff_size.unwrap_or(u64::MAX),
        Criterion::ShorterTime => i.duration_ms.unwrap_or(u64::MAX),
        Criterion::FewerAttempts => u64::from(i.attempts),
    }
}

fn keys(rule: &RankRule, i: &RankInput) -> Vec<u64> {
    rule.order.iter().map(|c| key_of(*c, i)).collect()
}

/// Orders `inputs` best first under `rule`. The result depends on the inputs and the rule only,
/// not on the order the inputs come in.
pub fn rank(rule: &RankRule, inputs: &[RankInput]) -> Vec<Ranked> {
    let mut sorted: Vec<(&RankInput, Vec<u64>)> =
        inputs.iter().map(|i| (i, keys(rule, i))).collect();
    sorted.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.task.cmp(&b.0.task)));
    let mut out = Vec::with_capacity(sorted.len());
    for (n, (input, key)) in sorted.iter().enumerate() {
        let decided_by = match n.checked_sub(1).and_then(|p| sorted.get(p)) {
            None => DecidedBy::First,
            Some((_, above)) => match key.iter().zip(above).position(|(a, b)| a != b) {
                Some(at) => rule
                    .order
                    .get(at)
                    .map_or(DecidedBy::TaskId, |c| DecidedBy::Criterion(*c)),
                None => DecidedBy::TaskId,
            },
        };
        out.push(Ranked {
            task: input.task.clone(),
            position: n + 1,
            decided_by,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }

    fn input(task: &str) -> RankInput {
        RankInput {
            task: tid(task),
            completed: true,
            verified: true,
            scope: ScopeState::In,
            diff_size: Some(10),
            duration_ms: Some(1_000),
            attempts: 1,
        }
    }

    fn order(rule: &RankRule, inputs: &[RankInput]) -> Vec<String> {
        rank(rule, inputs)
            .into_iter()
            .map(|r| r.task.to_string())
            .collect()
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }
    }

    fn random_inputs(rng: &mut Lcg, n: usize) -> Vec<RankInput> {
        (0..n)
            .map(|i| RankInput {
                task: tid(&format!("m{i:02}")),
                completed: rng.next(4) != 0,
                verified: rng.next(2) == 0,
                scope: match rng.next(3) {
                    0 => ScopeState::Out,
                    1 => ScopeState::Unknown,
                    _ => ScopeState::In,
                },
                diff_size: if rng.next(5) == 0 {
                    None
                } else {
                    Some(rng.next(4) * 10)
                },
                duration_ms: if rng.next(5) == 0 {
                    None
                } else {
                    Some(rng.next(3) * 100)
                },
                attempts: rng.next(3) as u32,
            })
            .collect()
    }

    fn shuffled<T: Clone>(rng: &mut Lcg, items: &[T]) -> Vec<T> {
        let mut v = items.to_vec();
        for i in (1..v.len()).rev() {
            let j = rng.next(i as u64 + 1) as usize;
            v.swap(i, j);
        }
        v
    }

    #[test]
    fn the_default_rule_puts_verified_before_unverified_then_the_smaller_diff_then_the_shorter_time(
    ) {
        let rule = RankRule::default();
        let unverified_small = RankInput {
            verified: false,
            diff_size: Some(1),
            ..input("a")
        };
        let verified_big = RankInput {
            diff_size: Some(500),
            ..input("b")
        };
        let verified_small_slow = RankInput {
            diff_size: Some(20),
            duration_ms: Some(9_000),
            ..input("c")
        };
        let verified_small_fast = RankInput {
            diff_size: Some(20),
            duration_ms: Some(2_000),
            ..input("d")
        };
        let got = order(
            &rule,
            &[
                unverified_small,
                verified_big,
                verified_small_slow,
                verified_small_fast,
            ],
        );
        assert_eq!(got, ["d", "c", "b", "a"]);
    }

    #[test]
    fn a_member_that_did_not_complete_ranks_below_one_that_did_whatever_else_is_true() {
        let rule = RankRule::default();
        let failed_but_tiny = RankInput {
            completed: false,
            verified: true,
            diff_size: Some(0),
            duration_ms: Some(1),
            ..input("a")
        };
        let slow_and_big = RankInput {
            verified: false,
            diff_size: Some(9_999),
            duration_ms: Some(9_999_999),
            ..input("z")
        };
        assert_eq!(order(&rule, &[failed_but_tiny, slow_and_big]), ["z", "a"]);
    }

    #[test]
    fn ties_fall_to_the_task_id_and_say_so() {
        let rule = RankRule::default();
        let ranked = rank(&rule, &[input("b"), input("a"), input("c")]);
        let names: Vec<&str> = ranked.iter().map(|r| r.task.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(ranked[0].decided_by, DecidedBy::First);
        assert_eq!(ranked[1].decided_by, DecidedBy::TaskId);
        assert_eq!(ranked[2].position, 3);
    }

    #[test]
    fn each_position_names_the_criterion_that_separated_it_from_the_one_above() {
        let rule = RankRule::default();
        let a = input("a");
        let b = RankInput {
            diff_size: Some(11),
            ..input("b")
        };
        let c = RankInput {
            verified: false,
            ..input("c")
        };
        let ranked = rank(&rule, &[c, b, a]);
        assert_eq!(
            ranked[1].decided_by,
            DecidedBy::Criterion(Criterion::SmallerDiff)
        );
        assert_eq!(
            ranked[2].decided_by,
            DecidedBy::Criterion(Criterion::Verified)
        );
    }

    #[test]
    fn a_scope_that_is_not_known_ranks_between_inside_and_outside() {
        let rule = RankRule::parse(&["in-scope"]).unwrap();
        let with = |t: &str, scope| RankInput { scope, ..input(t) };
        let got = order(
            &rule,
            &[
                with("a", ScopeState::Out),
                with("b", ScopeState::Unknown),
                with("c", ScopeState::In),
            ],
        );
        assert_eq!(got, ["c", "b", "a"]);
    }

    #[test]
    fn unknown_sizes_and_times_rank_after_known_ones() {
        let rule = RankRule::parse(&["diff", "time"]).unwrap();
        let unknown = RankInput {
            diff_size: None,
            ..input("a")
        };
        let known = RankInput {
            diff_size: Some(1_000_000),
            ..input("b")
        };
        assert_eq!(order(&rule, &[unknown.clone(), known.clone()]), ["b", "a"]);
        let slow_unknown = RankInput {
            duration_ms: None,
            ..input("a")
        };
        let slow_known = RankInput {
            duration_ms: Some(u64::MAX - 1),
            ..input("b")
        };
        let same_diff = RankRule::parse(&["time"]).unwrap();
        assert_eq!(order(&same_diff, &[slow_unknown, slow_known]), ["b", "a"]);
    }

    #[test]
    fn the_configured_order_decides_which_criterion_wins() {
        let small_unverified = RankInput {
            verified: false,
            diff_size: Some(1),
            ..input("a")
        };
        let big_verified = RankInput {
            diff_size: Some(100),
            ..input("b")
        };
        let inputs = [small_unverified, big_verified];
        let verified_first = RankRule::parse(&["verified", "diff"]).unwrap();
        let small_first = RankRule::parse(&["diff", "verified"]).unwrap();
        assert_eq!(order(&verified_first, &inputs), ["b", "a"]);
        assert_eq!(order(&small_first, &inputs), ["a", "b"]);
    }

    #[test]
    fn a_rule_is_parsed_strictly() {
        assert!(RankRule::parse(&[]).is_err());
        let unknown = RankRule::parse(&["verified", "luck"]).unwrap_err();
        assert!(unknown.message.contains("luck"), "{}", unknown.message);
        assert!(unknown.message.contains("attempts"), "{}", unknown.message);
        let twice = RankRule::parse(&["diff", "diff"]).unwrap_err();
        assert!(twice.message.contains("twice"));
        for c in Criterion::ALL {
            assert_eq!(Criterion::parse(c.as_str()), Some(c));
            assert!(!c.describe().is_empty());
        }
    }

    #[test]
    fn the_formula_prints_every_criterion_and_the_final_tie_break() {
        let f = RankRule::default().formula();
        assert_eq!(
            f,
            "sort by (completed, verified, in-scope, diff, time), then by task id a-z; advisory only, nothing is merged or picked from it"
        );
        let all: Vec<&str> = Criterion::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(RankRule::parse(&all).unwrap().criteria().len(), 6);
    }

    #[test]
    fn property_the_order_is_deterministic_and_does_not_depend_on_input_order() {
        let mut rng = Lcg(7);
        for round in 0..200 {
            let n = 1 + rng.next(12) as usize;
            let inputs = random_inputs(&mut rng, n);
            let rules = [
                RankRule::default(),
                RankRule::parse(&["attempts", "time", "diff"]).unwrap(),
                RankRule::parse(&["in-scope"]).unwrap(),
            ];
            for rule in &rules {
                let first = rank(rule, &inputs);
                assert_eq!(first, rank(rule, &inputs), "round {round}");
                let again = rank(rule, &shuffled(&mut rng, &inputs));
                assert_eq!(
                    first, again,
                    "round {round}: input order changed the ranking"
                );
            }
        }
    }

    #[test]
    fn property_the_order_is_total_and_strict() {
        let mut rng = Lcg(99);
        for _ in 0..200 {
            let n = 1 + rng.next(12) as usize;
            let inputs = random_inputs(&mut rng, n);
            let ranked = rank(&RankRule::default(), &inputs);
            // every member exactly once, positions 1..=n without gaps or ties
            assert_eq!(ranked.len(), inputs.len());
            for (i, r) in ranked.iter().enumerate() {
                assert_eq!(r.position, i + 1);
            }
            let mut seen: Vec<&TaskId> = ranked.iter().map(|r| &r.task).collect();
            seen.sort();
            seen.dedup();
            assert_eq!(seen.len(), inputs.len());
        }
    }

    #[test]
    fn property_a_member_is_never_ranked_below_one_it_beats_on_the_first_criterion_that_differs() {
        let mut rng = Lcg(5);
        let rule = RankRule::default();
        for _ in 0..200 {
            let inputs = random_inputs(&mut rng, 8);
            let ranked = rank(&rule, &inputs);
            let by_task = |t: &TaskId| inputs.iter().find(|i| &i.task == t).unwrap();
            for pair in ranked.windows(2) {
                let (hi, lo) = (by_task(&pair[0].task), by_task(&pair[1].task));
                let kh = keys(&rule, hi);
                let kl = keys(&rule, lo);
                assert!(kh <= kl, "{hi:?} ranked above {lo:?}");
                if kh == kl {
                    assert!(hi.task < lo.task, "a tie is broken by task id");
                    assert_eq!(pair[1].decided_by, DecidedBy::TaskId);
                }
            }
        }
    }

    #[test]
    fn property_only_the_listed_criteria_matter() {
        // no hidden inputs: changing a field the rule does not name changes nothing
        let mut rng = Lcg(1234);
        let rule = RankRule::parse(&["verified", "diff"]).unwrap();
        for _ in 0..200 {
            let inputs = random_inputs(&mut rng, 9);
            let tweaked: Vec<RankInput> = inputs
                .iter()
                .map(|i| RankInput {
                    completed: !i.completed,
                    scope: match i.scope {
                        ScopeState::In => ScopeState::Out,
                        _ => ScopeState::In,
                    },
                    duration_ms: Some(rng.next(1_000)),
                    attempts: rng.next(9) as u32,
                    ..i.clone()
                })
                .collect();
            assert_eq!(rank(&rule, &inputs), rank(&rule, &tweaked));
        }
    }

    #[test]
    fn property_improving_one_member_never_hurts_it() {
        // monotone: making a member verified (all else equal) cannot lower its position
        let mut rng = Lcg(42);
        let rule = RankRule::default();
        for _ in 0..200 {
            let inputs = random_inputs(&mut rng, 8);
            let target = rng.next(8) as usize;
            let before = rank(&rule, &inputs);
            let mut better = inputs.clone();
            better[target].verified = true;
            better[target].completed = true;
            better[target].scope = ScopeState::In;
            let after = rank(&rule, &better);
            let pos = |r: &[Ranked], t: &TaskId| r.iter().find(|x| &x.task == t).unwrap().position;
            let t = &inputs[target].task;
            assert!(pos(&after, t) <= pos(&before, t));
        }
    }
}
