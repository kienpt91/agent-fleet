//! Handoff packets: what is carried from one attempt of a task to the next, and to whoever looks at
//! the task afterwards.
//!
//! A packet is built from fleet's own state: the manifest of the pack the attempt had (with the
//! digest of its stable layers 1 to 5), what fleet's runs of the checks said, and pointers to the
//! evidence. A summary the worker wrote may be attached, but it is stored and shown as UNVERIFIED
//! data and is never the source of anything: the next pack compiles layers 1 to 5 from the ledger
//! and the repository, and shows the summary in layer 7 only.

use crate::error::ContextError;
use crate::input::{CheckState, VerificationLine};
use crate::limits::*;
use crate::pack::{LayerRecord, Manifest, Pack, Reader};
use crate::text::{quote, Hygiene};
use fleet_domain::approval::Digest;
use fleet_domain::value::Value;

pub const HANDOFF_SCHEMA: &str = "neo-handoff/1";

/// Why a handoff was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffTrigger {
    TaskCompleted,
    TaskFailed,
    /// An attempt ended and the task goes back in line.
    Retry,
    /// The daemon found the task running after a crash or a restart.
    Recovery,
    /// An operator asked (`fleet handoff <task>`).
    OnDemand,
}

impl HandoffTrigger {
    pub const ALL: [HandoffTrigger; 5] = [
        HandoffTrigger::TaskCompleted,
        HandoffTrigger::TaskFailed,
        HandoffTrigger::Retry,
        HandoffTrigger::Recovery,
        HandoffTrigger::OnDemand,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            HandoffTrigger::TaskCompleted => "task-completed",
            HandoffTrigger::TaskFailed => "task-failed",
            HandoffTrigger::Retry => "retry",
            HandoffTrigger::Recovery => "recovery",
            HandoffTrigger::OnDemand => "on-demand",
        }
    }

    pub fn parse(s: &str) -> Option<HandoffTrigger> {
        HandoffTrigger::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

/// The part of a pack a handoff keeps: where to find it and what it was made of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackSlice {
    pub digest: String,
    /// The digest over layers 1 to 5: a pack compiled later from the same state has the same one.
    pub stable_digest: String,
    pub profile: String,
    pub layers: Vec<LayerRecord>,
}

/// What is needed to make a packet (the summary is raw text from the worker, cleaned on the way in).
#[derive(Clone, Debug)]
pub struct HandoffParts {
    pub run: String,
    pub task: String,
    pub attempt: u32,
    pub trigger: HandoffTrigger,
    pub verification: Vec<VerificationLine>,
    pub evidence: Vec<String>,
    pub summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffPacket {
    pub run: String,
    pub task: String,
    pub attempt: u32,
    pub trigger: HandoffTrigger,
    pub pack: PackSlice,
    /// Where fleet's own runs of the checks stood.
    pub verification: Vec<VerificationLine>,
    /// Ids of the evidence the packet points at; the evidence itself stays in the audit log.
    pub evidence: Vec<String>,
    /// A summary the worker wrote. Cleaned and clipped; always UNVERIFIED.
    pub summary: Option<String>,
    digest: String,
}

impl HandoffPacket {
    pub fn new(
        parts: HandoffParts,
        pack: &Pack,
        hygiene: &Hygiene,
        hasher: &dyn Digest,
    ) -> HandoffPacket {
        let m: &Manifest = pack.manifest();
        let summary = parts
            .summary
            .as_deref()
            .map(|s| hygiene.block(s, MAX_SUMMARY).text)
            .filter(|s| !s.is_empty());
        let mut packet = HandoffPacket {
            run: hygiene.line(&parts.run, MAX_ID),
            task: hygiene.line(&parts.task, MAX_ID),
            attempt: parts.attempt,
            trigger: parts.trigger,
            pack: PackSlice {
                digest: pack.digest().to_string(),
                stable_digest: m.stable_digest.clone(),
                profile: m.profile.name.clone(),
                layers: m.layers.clone(),
            },
            verification: parts
                .verification
                .into_iter()
                .take(MAX_CHECKS * 2)
                .collect(),
            evidence: parts
                .evidence
                .iter()
                .take(MAX_POINTERS)
                .map(|e| hygiene.line(e, MAX_ID))
                .collect(),
            summary,
            digest: String::new(),
        };
        packet.digest = packet.compute_digest(hasher);
        packet
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    fn body_value(&self) -> Value {
        Value::object([
            ("schema", Value::str(HANDOFF_SCHEMA)),
            ("run", Value::str(&self.run)),
            ("task", Value::str(&self.task)),
            ("attempt", Value::int(u64::from(self.attempt))),
            ("trigger", Value::str(self.trigger.as_str())),
            (
                "pack",
                Value::object([
                    ("digest", Value::str(&self.pack.digest)),
                    ("stable_digest", Value::str(&self.pack.stable_digest)),
                    ("profile", Value::str(&self.pack.profile)),
                    (
                        "layers",
                        Value::array(self.pack.layers.iter().map(LayerRecord::to_value)),
                    ),
                ]),
            ),
            (
                "verification",
                Value::array(self.verification.iter().map(|v| {
                    Value::object([
                        ("goal", Value::str(&v.goal)),
                        ("check", Value::str(&v.check)),
                        ("mandatory", Value::Bool(v.mandatory)),
                        ("state", Value::str(v.state.as_str())),
                        (
                            "commit",
                            v.commit.as_deref().map_or(Value::Null, Value::str),
                        ),
                        (
                            "evidence",
                            v.evidence.as_deref().map_or(Value::Null, Value::str),
                        ),
                    ])
                })),
            ),
            (
                "evidence",
                Value::array(self.evidence.iter().map(|e| Value::str(e.as_str()))),
            ),
            (
                "summary",
                self.summary.as_deref().map_or(Value::Null, Value::str),
            ),
            ("summary_trust", Value::str("UNVERIFIED")),
        ])
    }

    fn compute_digest(&self, hasher: &dyn Digest) -> String {
        let text = format!("neo-handoff/v1\n{}", self.body_value().canonical());
        hasher.sha256_hex(text.as_bytes())
    }

    /// The packet as a JSON document, digest included.
    pub fn to_value(&self) -> Value {
        let mut map = self.body_value().as_object().cloned().unwrap_or_default();
        map.insert("digest".to_string(), Value::str(&self.digest));
        Value::Object(map)
    }

    /// Reads a stored packet back and checks that its digest is the digest of what it holds.
    pub fn from_value(v: &Value, hasher: &dyn Digest) -> Result<HandoffPacket, ContextError> {
        let r = Reader(v);
        if r.str("schema")? != HANDOFF_SCHEMA {
            return Err(ContextError::Malformed("not a handoff packet".to_string()));
        }
        let pack = r.object("pack")?;
        let layers: Vec<LayerRecord> = pack
            .array("layers")?
            .iter()
            .map(LayerRecord::from_value)
            .collect::<Result<_, _>>()?;
        let verification: Vec<VerificationLine> = r
            .array("verification")?
            .iter()
            .map(|item| {
                let i = Reader(item);
                let state = CheckState::parse(i.str("state")?)
                    .ok_or_else(|| ContextError::Malformed("unknown check state".to_string()))?;
                Ok(VerificationLine {
                    goal: i.str("goal")?.to_string(),
                    check: i.str("check")?.to_string(),
                    mandatory: i.bool("mandatory")?,
                    state,
                    commit: i.opt_str("commit")?.map(str::to_string),
                    evidence: i.opt_str("evidence")?.map(str::to_string),
                })
            })
            .collect::<Result<_, ContextError>>()?;
        let trigger = HandoffTrigger::parse(r.str("trigger")?)
            .ok_or_else(|| ContextError::Malformed("unknown handoff trigger".to_string()))?;
        let packet = HandoffPacket {
            run: r.str("run")?.to_string(),
            task: r.str("task")?.to_string(),
            attempt: r.u32("attempt")?,
            trigger,
            pack: PackSlice {
                digest: pack.hex("digest")?,
                stable_digest: pack.hex("stable_digest")?,
                profile: pack.str("profile")?.to_string(),
                layers,
            },
            verification,
            evidence: r.strings("evidence")?,
            summary: r.opt_str("summary")?.map(str::to_string),
            digest: r.hex("digest")?,
        };
        if packet.compute_digest(hasher) != packet.digest {
            return Err(ContextError::Malformed(
                "a handoff packet does not match its digest".to_string(),
            ));
        }
        Ok(packet)
    }

    /// The packet for people: plain text, with the worker's summary quoted and labelled.
    pub fn render(&self) -> String {
        let mut out = format!(
            "Handoff of {}/{}, attempt {}, trigger {}\ndigest: {}\npack: {} (profile {})\nlayers 1-5 digest: {}\n",
            self.run,
            self.task,
            self.attempt,
            self.trigger.as_str(),
            self.digest,
            self.pack.digest,
            self.pack.profile,
            self.pack.stable_digest
        );
        out.push_str("layers:\n");
        for l in &self.pack.layers {
            out.push_str(&format!(
                "  {} {:<12} {:>6} tokens  {}\n",
                l.id.number(),
                l.id.name(),
                l.tokens,
                l.digest.get(..16).unwrap_or(&l.digest)
            ));
        }
        out.push_str("open verification state (fleet's own runs):\n");
        if self.verification.is_empty() {
            out.push_str("  (no contract governs the task)\n");
        }
        for v in &self.verification {
            out.push_str(&format!(
                "  goal {} check {} ({}): {}{}{}\n",
                v.goal,
                v.check,
                if v.mandatory { "mandatory" } else { "optional" },
                v.state.as_str(),
                v.commit
                    .as_ref()
                    .map_or(String::new(), |c| format!(" on {c}")),
                v.evidence
                    .as_ref()
                    .map_or(String::new(), |e| format!(" (evidence {e})")),
            ));
        }
        out.push_str(&format!(
            "evidence pointers: {}\n",
            if self.evidence.is_empty() {
                "none".to_string()
            } else {
                self.evidence.join(", ")
            }
        ));
        match &self.summary {
            Some(s) => {
                out.push_str("summary written by the worker (UNVERIFIED: stored as data, never used as a source):\n");
                out.push_str(&quote(s));
                out.push('\n');
            }
            None => out.push_str("no summary from the worker\n"),
        }
        out
    }
}
