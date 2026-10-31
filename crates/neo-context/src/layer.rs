//! The eight layers of a pack, in the one order they are ever rendered in.
//!
//! A lower layer never displaces a higher one: when a pack is too large, layers 7, 6 and 5 give
//! way first (in that order), then optional parts of layer 2; layers 1, 3 and 4 are never shortened.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerId {
    /// Non-negotiable rules and how to report. Fixed text, never truncated.
    Invariants,
    /// The project goal, constraints and glossary. Shortened by section priority only.
    Charter,
    /// Where the task sits in the goal tree and the plan.
    PlanPosition,
    /// What to do, and exactly how fleet will verify it.
    TaskSpec,
    /// Verified knowledge with its sources (a static, human-written file until milestone N3).
    Knowledge,
    /// Bounded excerpts of repository files the task touches, at a recorded commit.
    WorkingSet,
    /// Recent events of the task and its dependencies, and everything that agents or tools wrote.
    RecentEvents,
    /// What is left of the token budget.
    Scratch,
}

impl LayerId {
    pub const ALL: [LayerId; 8] = [
        LayerId::Invariants,
        LayerId::Charter,
        LayerId::PlanPosition,
        LayerId::TaskSpec,
        LayerId::Knowledge,
        LayerId::WorkingSet,
        LayerId::RecentEvents,
        LayerId::Scratch,
    ];

    /// Layers 1 to 5 hold nothing an agent or a tool wrote. Their digests are the stable part of a
    /// pack: for the same plan, goal ledger, results of fleet's own checks and configuration they
    /// are the same whatever the attempt, the events, the working set or the handoff. They change
    /// when that state does (layer 3 lists the plan, layer 4 what fleet's own runs said).
    pub const STABLE: [LayerId; 5] = [
        LayerId::Invariants,
        LayerId::Charter,
        LayerId::PlanPosition,
        LayerId::TaskSpec,
        LayerId::Knowledge,
    ];

    /// 1 to 8.
    pub fn number(self) -> u8 {
        match self {
            LayerId::Invariants => 1,
            LayerId::Charter => 2,
            LayerId::PlanPosition => 3,
            LayerId::TaskSpec => 4,
            LayerId::Knowledge => 5,
            LayerId::WorkingSet => 6,
            LayerId::RecentEvents => 7,
            LayerId::Scratch => 8,
        }
    }

    pub fn from_number(n: u64) -> Option<LayerId> {
        LayerId::ALL
            .iter()
            .copied()
            .find(|l| u64::from(l.number()) == n)
    }

    pub fn name(self) -> &'static str {
        match self {
            LayerId::Invariants => "invariants",
            LayerId::Charter => "charter",
            LayerId::PlanPosition => "plan",
            LayerId::TaskSpec => "task",
            LayerId::Knowledge => "knowledge",
            LayerId::WorkingSet => "working-set",
            LayerId::RecentEvents => "events",
            LayerId::Scratch => "scratch",
        }
    }

    pub fn from_name(s: &str) -> Option<LayerId> {
        LayerId::ALL.iter().copied().find(|l| l.name() == s)
    }

    /// One line for `fleet context explain`.
    pub fn purpose(self) -> &'static str {
        match self {
            LayerId::Invariants => "rules that never change, and how to report; never truncated",
            LayerId::Charter => {
                "project goal, constraints, glossary; cut only by section priority, last of all"
            }
            LayerId::PlanPosition => "this task's place in the goal tree: done, remaining, dependencies; never cut",
            LayerId::TaskSpec => "what to do and exactly how fleet will verify it; never cut",
            LayerId::Knowledge => "verified facts with their sources (static file until N3); dropped third",
            LayerId::WorkingSet => "bounded excerpts of the files the task touches, at a recorded commit; dropped second",
            LayerId::RecentEvents => "recent events and everything agents wrote, labelled UNVERIFIED; dropped first",
            LayerId::Scratch => "the token budget that is left for the agent",
        }
    }

    /// True for the layers whose items may be removed to make the pack fit.
    pub fn sheddable(self) -> bool {
        matches!(
            self,
            LayerId::Knowledge | LayerId::WorkingSet | LayerId::RecentEvents
        )
    }
}

impl fmt::Display for LayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.number(), self.name())
    }
}

/// Who wrote a piece of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// Fleet, its operator, or the repository at a recorded commit.
    Trusted,
    /// An agent or a program an agent controls. Rendered with a visible `UNVERIFIED` label and
    /// quoted line by line; only ever placed in layer 7.
    Unverified,
}

/// What to do with text that agents or tools wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum UnverifiedPolicy {
    /// Show it in layer 7, labelled UNVERIFIED and quoted.
    #[default]
    Label,
    /// Leave it out altogether.
    Exclude,
}

impl UnverifiedPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            UnverifiedPolicy::Label => "label",
            UnverifiedPolicy::Exclude => "exclude",
        }
    }

    pub fn parse(s: &str) -> Option<UnverifiedPolicy> {
        match s {
            "label" => Some(UnverifiedPolicy::Label),
            "exclude" => Some(UnverifiedPolicy::Exclude),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_names_and_order_agree() {
        for (i, l) in LayerId::ALL.iter().enumerate() {
            assert_eq!(usize::from(l.number()), i + 1);
            assert_eq!(LayerId::from_number(u64::from(l.number())), Some(*l));
            assert_eq!(LayerId::from_name(l.name()), Some(*l));
            assert!(!l.purpose().is_empty());
        }
        assert!(LayerId::ALL.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(LayerId::from_number(0), None);
        assert_eq!(LayerId::from_number(9), None);
        assert_eq!(LayerId::from_name("nope"), None);
        assert_eq!(LayerId::STABLE.len(), 5);
        assert_eq!(LayerId::WorkingSet.to_string(), "6 working-set");
    }

    #[test]
    fn only_the_layers_that_give_way_are_sheddable() {
        let shed: Vec<u8> = LayerId::ALL
            .iter()
            .filter(|l| l.sheddable())
            .map(|l| l.number())
            .collect();
        assert_eq!(shed, vec![5, 6, 7]);
    }

    #[test]
    fn policy_round_trips() {
        for p in [UnverifiedPolicy::Label, UnverifiedPolicy::Exclude] {
            assert_eq!(UnverifiedPolicy::parse(p.as_str()), Some(p));
        }
        assert_eq!(UnverifiedPolicy::parse("maybe"), None);
        assert_eq!(UnverifiedPolicy::default(), UnverifiedPolicy::Label);
    }
}
