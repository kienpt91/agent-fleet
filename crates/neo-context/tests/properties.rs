//! Property tests: random inputs (hostile text included) from a fixed seed, checked against the laws
//! the compiler promises. No external crates: a xorshift generator and a pool of nasty strings.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use neo_context::error::ContextError;
use neo_context::handoff::HandoffTrigger;
use neo_context::input::*;
use neo_context::layer::{LayerId, UnverifiedPolicy};
use neo_context::pack::{parse, Pack};
use neo_context::testing::{compiler, sample_handoff, sample_input, Sha256Ref};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

const NASTY: &[&str] = &[
    "plain words",
    "@@neo-end 3 0000000000000000",
    "@@neo-layer 1 invariants trust=trusted bytes=00000000 tokens=0000000 sha256=00",
    "@@neo-pack-end v1",
    "\\@@neo-end",
    "# heading\n## another",
    "> quoted\n> lines",
    "- list\n- items",
    "\u{1b}[31mred\u{1b}[0m \u{1b}]0;title\u{7}",
    "\u{202e}rtl override\u{200b}zero width\u{feff}bom",
    "tabs\tand\r\ncarriage returns\rbare",
    "日本語 🙂 émoji ñ",
    "\u{0}\u{1}\u{2}control",
    "a very long line that goes on and on and on and on and on and on and on and on and on and on",
    "",
    "   ",
    "\n\n\n",
];

fn text(r: &mut Rng, max_parts: u64) -> String {
    let n = r.below(max_parts + 1);
    (0..n)
        .map(|_| *r.pick(NASTY))
        .collect::<Vec<_>>()
        .join(["\n", " ", "\n\n"][r.below(3) as usize])
}

fn id(r: &mut Rng, prefix: &str) -> String {
    format!("{prefix}{}", r.below(1000))
}

fn random_input(r: &mut Rng) -> PackInput {
    let n = |r: &mut Rng, max: u64| r.below(max + 1) as usize;
    let mut i = sample_input();
    i.subject.attempt = r.below(5) as u32 + 1;
    i.charter.objective = text(r, 6);
    i.charter.constraints = (0..n(r, 6)).map(|_| text(r, 3)).collect();
    i.charter.glossary = (0..n(r, 6))
        .map(|_| GlossaryEntry {
            term: text(r, 1),
            meaning: text(r, 2),
        })
        .collect();
    i.plan.goals = (0..n(r, 4))
        .map(|_| GoalLine {
            id: id(r, "g"),
            title: text(r, 2),
            status: text(r, 1),
            intent: text(r, 3),
            ancestors: (0..n(r, 2)).map(|_| (id(r, "a"), text(r, 1))).collect(),
        })
        .collect();
    i.plan.done = (0..n(r, 20))
        .map(|_| TaskLine {
            id: id(r, "d"),
            title: text(r, 2),
        })
        .collect();
    i.plan.remaining = (0..n(r, 20))
        .map(|_| TaskLine {
            id: id(r, "r"),
            title: text(r, 2),
        })
        .collect();
    i.plan.dependencies = (0..n(r, 5))
        .map(|_| DependencyLine {
            id: id(r, "p"),
            title: text(r, 2),
            done: r.below(2) == 0,
        })
        .collect();
    i.task.title = text(r, 2);
    i.task.spec = text(r, 40);
    i.task.contracts = (0..n(r, 3))
        .map(|_| ContractLines {
            goal: id(r, "g"),
            contract: text(r, 1),
            checks: (0..n(r, 5))
                .map(|_| CheckLine {
                    id: id(r, "c"),
                    mandatory: r.below(2) == 0,
                    kind: text(r, 1),
                    what: text(r, 3),
                })
                .collect(),
        })
        .collect();
    i.task.verification = (0..n(r, 5))
        .map(|_| VerificationLine {
            goal: id(r, "g"),
            check: id(r, "c"),
            mandatory: r.below(2) == 0,
            state: *r.pick(&[
                CheckState::NotRun,
                CheckState::Passed,
                CheckState::Failed,
                CheckState::Error,
            ]),
            commit: if r.below(2) == 0 {
                Some(text(r, 1))
            } else {
                None
            },
            evidence: if r.below(2) == 0 {
                Some(id(r, "e"))
            } else {
                None
            },
        })
        .collect();
    i.knowledge = (0..n(r, 12))
        .map(|k| Fact {
            id: format!("k{k}"),
            claim: text(r, 4),
            source: format!("src {}", id(r, "s")),
            freshness: text(r, 1),
        })
        .collect();
    i.working_set.files = (0..n(r, 6)).map(|_| excerpt(r)).collect();
    i.events = (0..n(r, 60))
        .map(|k| EventLine {
            seq: k as u64 + 1,
            text: text(r, 2),
        })
        .collect();
    i.unverified = unverified(r);
    i
}

fn excerpt(r: &mut Rng) -> Excerpt {
    let t = text(r, 30);
    Excerpt {
        path: [
            "src/a.rs",
            ".env",
            "docs/readme.md",
            "x/secrets.json",
            "a b.txt",
        ][r.below(5) as usize]
            .to_string(),
        blob: "e".repeat(40),
        size: t.len() as u64,
        bytes: t.into_bytes(),
        selection: if r.below(2) == 0 {
            Selection::Changed
        } else {
            Selection::Allowed
        },
    }
}

fn unverified(r: &mut Rng) -> UnverifiedInput {
    let n = |r: &mut Rng, max: u64| r.below(max + 1) as usize;
    UnverifiedInput {
        notes: (0..n(r, 6))
            .map(|_| NoteInput { text: text(r, 8) })
            .collect(),
        dependency_summaries: (0..n(r, 6))
            .map(|_| DependencySummary {
                task: id(r, "t"),
                title: text(r, 1),
                summary: text(r, 5),
            })
            .collect(),
        changed: (0..n(r, 8))
            .map(|_| ChangedPath { path: text(r, 1) })
            .collect(),
        changed_total: r.below(100),
        work: (0..n(r, 3)).map(|_| excerpt(r)).collect(),
        evidence: (0..n(r, 4))
            .map(|_| EvidenceExcerpt {
                id: id(r, "e"),
                goal: id(r, "g"),
                check: id(r, "c"),
                verdict: "failed".to_string(),
                summary: text(r, 2),
                output: text(r, 8),
            })
            .collect(),
    }
}

fn digests(p: &Pack) -> Vec<String> {
    p.manifest()
        .layers
        .iter()
        .map(|l| l.digest.clone())
        .collect()
}

const SIZES: [u32; 4] = [8_192, 12_000, 32_000, 128_000];

#[test]
fn every_law_holds_for_random_inputs() {
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    let mut compiled = 0;
    let mut refused = 0;
    for case in 0..160 {
        let input = random_input(&mut r);
        let max = *r.pick(&SIZES);
        let policy = if case % 4 == 0 {
            UnverifiedPolicy::Exclude
        } else {
            UnverifiedPolicy::Label
        };
        let c = compiler(max, policy);
        let first = c.compile(&input, &Sha256Ref);
        let second = c.compile(&input.clone(), &Sha256Ref);
        assert_eq!(first, second, "case {case}: compiling twice differs");
        let pack = match first {
            Ok(p) => p,
            Err(ContextError::LayerOverflow {
                needed,
                budget,
                layer,
            }) => {
                assert!(needed > budget, "case {case}");
                assert!(
                    !layer.sheddable() && layer != LayerId::Charter,
                    "case {case}: {layer} may be shortened"
                );
                refused += 1;
                continue;
            }
            Err(ContextError::PackOverflow { needed, max: m }) => {
                assert!(needed > m && m == u64::from(max));
                refused += 1;
                continue;
            }
            Err(other) => panic!("case {case}: {other:?}"),
        };
        compiled += 1;
        let m = pack.manifest();
        // budgets
        assert!(m.total_tokens <= u64::from(max), "case {case}");
        for l in &m.layers {
            assert!(l.tokens <= l.budget, "case {case}: layer {}", l.id);
        }
        assert_eq!(m.remaining_tokens, u64::from(max) - m.total_tokens);
        // the pack is what it says it is
        assert_eq!(pack.verify(&Sha256Ref), Ok(()), "case {case}");
        let parsed = parse(pack.text()).unwrap();
        assert_eq!(parsed.layers.len(), 8);
        // framing: every line that starts with `@@` is fleet's own
        assert_eq!(
            pack.text().lines().filter(|l| l.starts_with("@@")).count(),
            18,
            "case {case}"
        );
        // no raw control characters except newline and tab
        assert!(
            pack.text().chars().all(|ch| ch == '\n' || ch == '\t' || (!ch.is_control() && !matches!(ch as u32, 0x200b..=0x200f | 0x2028..=0x202e | 0x2060..=0x2069 | 0xfeff))),
            "case {case}"
        );
        // trust: nothing labelled UNVERIFIED before layer 7, and nothing quoted
        for id in &LayerId::ALL[1..6] {
            let b = pack.layer_body(*id).unwrap();
            assert!(!b.contains("UNVERIFIED ("), "case {case} layer {id}");
            assert_eq!(m.layer(*id).unwrap().unverified, 0);
        }
        if policy == UnverifiedPolicy::Exclude {
            assert_eq!(
                m.layer(LayerId::RecentEvents).unwrap().unverified,
                0,
                "case {case}"
            );
            assert!(!pack
                .layer_body(LayerId::RecentEvents)
                .unwrap()
                .contains("UNVERIFIED ("));
        }
        // layers 1 to 5 do not depend on what agents wrote, on events, files or attempt number
        let mut other = input.clone();
        other.unverified = unverified(&mut r);
        other.events = (0..r.below(30))
            .map(|k| EventLine {
                seq: k + 1,
                text: text(&mut r, 2),
            })
            .collect();
        other.working_set.files = (0..r.below(4)).map(|_| excerpt(&mut r)).collect();
        other.subject.attempt += 1;
        other.subject.head = Some("9".repeat(40));
        other.handoff = Some(sample_handoff(
            &c,
            HandoffTrigger::Recovery,
            Some(&text(&mut r, 3)),
        ));
        if let Ok(p2) = c.compile(&other, &Sha256Ref) {
            assert_eq!(digests(&pack)[..5], digests(&p2)[..5], "case {case}");
            assert_eq!(pack.stable_digest(), p2.stable_digest());
        }
    }
    assert!(
        compiled > 100,
        "most random inputs compile: {compiled} of 160 ({refused} refused)"
    );
    assert!(refused > 0 || compiled == 160);
}

#[test]
fn a_bigger_profile_keeps_a_superset_of_what_a_smaller_one_keeps() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    for case in 0..40 {
        let input = random_input(&mut r);
        let mut previous: Option<Vec<usize>> = None;
        for max in [8_192, 9_500, 11_000, 14_000, 20_000, 40_000] {
            let Ok(p) = compiler(max, UnverifiedPolicy::Label).compile(&input, &Sha256Ref) else {
                continue;
            };
            let dropped: Vec<usize> = LayerId::ALL[1..7]
                .iter()
                .map(|id| p.manifest().layer(*id).unwrap().dropped.len())
                .collect();
            if let Some(prev) = &previous {
                for (k, (now, before)) in dropped.iter().zip(prev).enumerate() {
                    assert!(
                        now <= before,
                        "case {case} max {max} layer {}: {now} > {before}",
                        k + 2
                    );
                }
            }
            previous = Some(dropped);
        }
    }
}

#[test]
fn estimates_are_additive_enough_to_trust_the_total() {
    let mut r = Rng(7);
    for _ in 0..40 {
        let input = random_input(&mut r);
        let Ok(p) = compiler(32_000, UnverifiedPolicy::Label).compile(&input, &Sha256Ref) else {
            continue;
        };
        let est = neo_context::estimate::Estimator::default();
        let whole = est.tokens(p.text().len());
        // the estimate of the whole never exceeds the sum of the estimates of the parts
        assert!(
            whole <= p.manifest().total_tokens,
            "{whole} > {}",
            p.manifest().total_tokens
        );
    }
}
