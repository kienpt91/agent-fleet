//! What the orchestrator remembers for the context compiler, rebuilt from the event log like the
//! fleet state and the goal ledger: the newest handoff packet of each task, which handoffs were
//! already made (so a restart does not make them twice) and a short history of each task's events.

use crate::orchestrator::Key;
use fleet_domain::approval::Digest;
use fleet_domain::events::DomainEvent;
use neo_context::handoff::{HandoffPacket, HandoffTrigger};
use neo_context::input::EventLine;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Events remembered per task.
const HISTORY_KEPT: usize = 64;

/// Handoffs are made once per attempt and class: one when the attempt ends the task, one when it
/// ends and the task goes back in line (a retry, or found running after a crash).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HandoffClass {
    TaskEnd,
    Restart,
}

impl HandoffClass {
    pub(crate) fn of(trigger: HandoffTrigger) -> Option<HandoffClass> {
        match trigger {
            HandoffTrigger::TaskCompleted | HandoffTrigger::TaskFailed => {
                Some(HandoffClass::TaskEnd)
            }
            HandoffTrigger::Retry | HandoffTrigger::Recovery => Some(HandoffClass::Restart),
            HandoffTrigger::OnDemand => None,
        }
    }
}

/// What the log says about one compiled pack: where it was recorded, its digests and its manifest.
/// A stored pack is trusted only if it matches such a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompiledRecord {
    pub seq: u64,
    pub digest: String,
    pub stable_digest: String,
    /// The canonical text of the manifest the event carried.
    pub manifest: String,
}

#[derive(Default)]
pub(crate) struct ContextState {
    /// The packet the next brief continues from: the newest one made by the flow itself. An
    /// on-demand packet (`fleet handoff`) is only the brief's source when there is none else, so an
    /// operator looking at a task does not replace what a worker left for its next attempt.
    handoffs: BTreeMap<Key, HandoffPacket>,
    /// The digest of the newest packet of any kind, to not record the same packet twice.
    last_handoff: BTreeMap<Key, String>,
    made: BTreeSet<(Key, u32, HandoffClass)>,
    history: BTreeMap<Key, VecDeque<EventLine>>,
    /// Every `context_compiled` event of each task, oldest first.
    compiled: BTreeMap<Key, Vec<CompiledRecord>>,
}

impl ContextState {
    pub(crate) fn apply(&mut self, seq: u64, event: &DomainEvent, hasher: &dyn Digest) {
        match event {
            DomainEvent::HandoffCreated {
                run,
                task,
                attempt,
                trigger,
                packet,
                ..
            } => {
                // A packet that does not match its own digest was edited after it was written:
                // it is not a handoff.
                let Ok(packet) = HandoffPacket::from_value(&packet.0, hasher) else {
                    return;
                };
                let key = (run.clone(), task.clone());
                if let Some(class) = HandoffTrigger::parse(trigger).and_then(HandoffClass::of) {
                    self.made.insert((key.clone(), *attempt, class));
                }
                self.last_handoff
                    .insert(key.clone(), packet.digest().to_string());
                if packet.trigger != HandoffTrigger::OnDemand || !self.handoffs.contains_key(&key) {
                    self.handoffs.insert(key, packet);
                }
            }
            DomainEvent::ContextCompiled {
                run,
                task,
                pack_digest,
                stable_digest,
                manifest,
                ..
            } => {
                self.compiled
                    .entry((run.clone(), task.clone()))
                    .or_default()
                    .push(CompiledRecord {
                        seq,
                        digest: pack_digest.clone(),
                        stable_digest: stable_digest.clone(),
                        manifest: manifest.0.canonical(),
                    });
            }
            DomainEvent::Warning { .. } => {}
            other => {
                if let (Some(run), Some(task)) = (other.run(), other.task()) {
                    let list = self.history.entry((run.clone(), task.clone())).or_default();
                    list.push_back(EventLine {
                        seq,
                        text: other.headline(),
                    });
                    while list.len() > HISTORY_KEPT {
                        list.pop_front();
                    }
                }
            }
        }
    }

    pub(crate) fn handoff(&self, key: &Key) -> Option<&HandoffPacket> {
        self.handoffs.get(key)
    }

    pub(crate) fn last_handoff_digest(&self, key: &Key) -> Option<&str> {
        self.last_handoff.get(key).map(String::as_str)
    }

    pub(crate) fn was_made(&self, key: &Key, attempt: u32, class: HandoffClass) -> bool {
        self.made.contains(&(key.clone(), attempt, class))
    }

    /// The remembered events of a task, oldest first.
    pub(crate) fn history(&self, key: &Key) -> Vec<EventLine> {
        self.history
            .get(key)
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Forgets a task (its worktree and records were cleaned up).
    pub(crate) fn forget(&mut self, key: &Key) {
        self.handoffs.remove(key);
        self.last_handoff.remove(key);
        self.history.remove(key);
        self.made.retain(|(k, _, _)| k != key);
        self.compiled.remove(key);
    }

    /// The packs the log recorded for a task, oldest first.
    pub(crate) fn compiled(&self, key: &Key) -> &[CompiledRecord] {
        self.compiled
            .get(key)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Where in the log the pack with this digest was recorded (the newest record of it).
    pub(crate) fn compiled_at(&self, key: &Key, digest: &str) -> Option<u64> {
        self.compiled(key)
            .iter()
            .rev()
            .find(|r| r.digest == digest)
            .map(|r| r.seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ToyDigest;
    use fleet_domain::ids::{RunId, Sha, TaskId};
    use fleet_domain::value::{Document, Value};
    use neo_context::layer::UnverifiedPolicy;
    use neo_context::testing::{compiler, sample_handoff};

    fn key(t: &str) -> Key {
        (RunId::parse("r1").unwrap(), TaskId::parse(t).unwrap())
    }

    fn created(packet: &HandoffPacket, attempt: u32, trigger: &str) -> DomainEvent {
        DomainEvent::HandoffCreated {
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse("api").unwrap(),
            attempt,
            trigger: trigger.to_string(),
            digest: packet.digest().to_string(),
            packet: Document(packet.to_value()),
        }
    }

    fn packet() -> HandoffPacket {
        // made with the toy digest the application tests use, so that it verifies here
        let c = compiler(8192, UnverifiedPolicy::Label);
        let pack = c
            .compile(&neo_context::testing::sample_input(), &ToyDigest)
            .unwrap();
        let _ = sample_handoff;
        HandoffPacket::new(
            neo_context::handoff::HandoffParts {
                run: "r1".into(),
                task: "api".into(),
                attempt: 1,
                trigger: HandoffTrigger::Retry,
                verification: vec![],
                evidence: vec![],
                summary: None,
            },
            &pack,
            c.hygiene(),
            &ToyDigest,
        )
    }

    #[test]
    fn a_handoff_event_is_remembered_with_its_class() {
        let mut s = ContextState::default();
        let p = packet();
        s.apply(5, &created(&p, 1, "retry"), &ToyDigest);
        assert_eq!(s.handoff(&key("api")).map(|h| h.digest()), Some(p.digest()));
        assert!(s.was_made(&key("api"), 1, HandoffClass::Restart));
        assert!(!s.was_made(&key("api"), 1, HandoffClass::TaskEnd));
        assert!(!s.was_made(&key("api"), 2, HandoffClass::Restart));
        s.apply(6, &created(&p, 2, "task-completed"), &ToyDigest);
        assert!(s.was_made(&key("api"), 2, HandoffClass::TaskEnd));
        s.apply(7, &created(&p, 3, "on-demand"), &ToyDigest);
        assert!(!s.was_made(&key("api"), 3, HandoffClass::Restart));
        assert!(
            !s.was_made(&key("api"), 3, HandoffClass::TaskEnd),
            "an on-demand handoff is never automatic"
        );
    }

    #[test]
    fn a_packet_that_does_not_match_its_digest_is_not_a_handoff() {
        let mut s = ContextState::default();
        let p = packet();
        let mut doc = p.to_value().as_object().unwrap().clone();
        doc.insert("attempt".to_string(), Value::int(9u64));
        let forged = DomainEvent::HandoffCreated {
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse("api").unwrap(),
            attempt: 9,
            trigger: "retry".into(),
            digest: p.digest().to_string(),
            packet: Document(Value::Object(doc)),
        };
        s.apply(1, &forged, &ToyDigest);
        assert!(s.handoff(&key("api")).is_none());
        assert!(!s.was_made(&key("api"), 9, HandoffClass::Restart));
    }

    #[test]
    fn history_keeps_the_newest_events_of_a_task_and_skips_meta_events() {
        let mut s = ContextState::default();
        let run = RunId::parse("r1").unwrap();
        let t = TaskId::parse("api").unwrap();
        for n in 0..100u64 {
            s.apply(
                n,
                &DomainEvent::TaskReady {
                    run: run.clone(),
                    task: t.clone(),
                },
                &ToyDigest,
            );
        }
        s.apply(
            200,
            &DomainEvent::ContextCompiled {
                run: run.clone(),
                task: t.clone(),
                attempt: 1,
                profile: "p".into(),
                pack_digest: "a".repeat(64),
                stable_digest: "b".repeat(64),
                tokens: 1,
                manifest: Document(Value::object([("k", Value::Null)])),
            },
            &ToyDigest,
        );
        s.apply(
            201,
            &DomainEvent::Warning {
                message: "w".into(),
            },
            &ToyDigest,
        );
        s.apply(
            202,
            &DomainEvent::RunStarted {
                run: run.clone(),
                max_parallel: 1,
            },
            &ToyDigest,
        );
        let h = s.history(&key("api"));
        assert_eq!(h.len(), HISTORY_KEPT);
        assert_eq!(h.first().map(|e| e.seq), Some(36));
        assert_eq!(h.last().map(|e| e.seq), Some(99));
        assert_eq!(h.last().map(|e| e.text.as_str()), Some("r1/api ready"));
        assert!(s.history(&key("other")).is_empty());
        let _ = Sha::parse(&"a".repeat(40));
    }

    #[test]
    fn forgetting_a_task_forgets_everything_about_it() {
        let mut s = ContextState::default();
        let p = packet();
        s.apply(1, &created(&p, 1, "retry"), &ToyDigest);
        s.apply(
            2,
            &DomainEvent::TaskReady {
                run: RunId::parse("r1").unwrap(),
                task: TaskId::parse("api").unwrap(),
            },
            &ToyDigest,
        );
        s.forget(&key("api"));
        assert!(s.handoff(&key("api")).is_none());
        assert!(s.history(&key("api")).is_empty());
        assert!(!s.was_made(&key("api"), 1, HandoffClass::Restart));
    }
    fn packet_of(trigger: HandoffTrigger, attempt: u32, summary: Option<&str>) -> HandoffPacket {
        let c = compiler(8192, UnverifiedPolicy::Label);
        let pack = c
            .compile(&neo_context::testing::sample_input(), &ToyDigest)
            .unwrap();
        HandoffPacket::new(
            neo_context::handoff::HandoffParts {
                run: "r1".into(),
                task: "api".into(),
                attempt,
                trigger,
                verification: vec![],
                evidence: vec![],
                summary: summary.map(str::to_string),
            },
            &pack,
            c.hygiene(),
            &ToyDigest,
        )
    }

    #[test]
    fn an_on_demand_packet_does_not_replace_what_a_worker_left_for_its_next_attempt() {
        let mut s = ContextState::default();
        let retry = packet_of(HandoffTrigger::Retry, 1, Some("what I left"));
        let asked = packet_of(HandoffTrigger::OnDemand, 1, None);
        s.apply(1, &created(&retry, 1, "retry"), &ToyDigest);
        s.apply(2, &created(&asked, 1, "on-demand"), &ToyDigest);
        // the brief continues from the packet the flow made, with the worker's summary
        let brief = s.handoff(&key("api")).unwrap();
        assert_eq!(brief.digest(), retry.digest());
        assert_eq!(brief.summary.as_deref(), Some("what I left"));
        // asking again does not record the same packet twice
        assert_eq!(s.last_handoff_digest(&key("api")), Some(asked.digest()));
        // a later packet of the flow does replace it
        let next = packet_of(HandoffTrigger::TaskFailed, 2, None);
        s.apply(3, &created(&next, 2, "task-failed"), &ToyDigest);
        assert_eq!(
            s.handoff(&key("api")).map(|h| h.digest()),
            Some(next.digest())
        );
        // with none from the flow, an on-demand one is what there is
        let mut lone = ContextState::default();
        lone.apply(1, &created(&asked, 1, "on-demand"), &ToyDigest);
        assert_eq!(
            lone.handoff(&key("api")).map(|h| h.digest()),
            Some(asked.digest())
        );
    }

    #[test]
    fn every_compiled_pack_is_remembered_in_log_order_with_its_manifest() {
        let mut s = ContextState::default();
        let run = RunId::parse("r1").unwrap();
        let task = TaskId::parse("api").unwrap();
        for (seq, digest) in [(10u64, 'a'), (20, 'b'), (30, 'a')] {
            s.apply(
                seq,
                &DomainEvent::ContextCompiled {
                    run: run.clone(),
                    task: task.clone(),
                    attempt: 1,
                    profile: "p".into(),
                    pack_digest: digest.to_string().repeat(64),
                    stable_digest: "c".repeat(64),
                    tokens: 1,
                    manifest: Document(Value::object([("k", Value::int(seq))])),
                },
                &ToyDigest,
            );
        }
        let records = s.compiled(&key("api"));
        assert_eq!(
            records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
        assert_eq!(records[1].manifest, "{\"k\":20}");
        assert_eq!(records[1].stable_digest, "c".repeat(64));
        // the newest record of a digest that was compiled twice
        assert_eq!(s.compiled_at(&key("api"), &"a".repeat(64)), Some(30));
        assert_eq!(s.compiled_at(&key("api"), &"z".repeat(64)), None);
        assert!(s.compiled(&key("other")).is_empty());
        s.forget(&key("api"));
        assert!(s.compiled(&key("api")).is_empty());
    }
}
