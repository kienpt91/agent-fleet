//! The compiler: inputs in, pack out. Pure: no model, no clock, no randomness, no I/O; the same
//! inputs, profile, policy and redactor give the same bytes and the same digest on every machine.
//!
//! # Budgets and the order in which a pack shrinks
//!
//! Every layer has a hard budget (a fixed share of the profile's `max_tokens`, see
//! `limits::LAYER_PERCENT`; the shares add up to more than the whole). Compiling goes in three steps:
//!
//! 1. Each layer is made to fit its own budget. Layers 1, 3 and 4 are never shortened: if one does
//!    not fit, compiling fails with [`ContextError::LayerOverflow`]. Layer 2 drops whole sections by
//!    priority (constraints before the glossary; the project goal is never dropped). Layers 5, 6 and 7
//!    drop whole items, lowest priority first.
//! 2. If the pack as a whole is still over the profile's `max_tokens`, items are dropped from layer
//!    7, then from layer 6, then from layer 5, in that order, until it fits.
//! 3. If it still does not fit, sections of layer 2 are dropped by priority. Layers 1, 3 and 4 and the
//!    project goal are never touched; if they alone are too large the result is
//!    [`ContextError::PackOverflow`] and there is no pack. Nothing is ever cut silently: the manifest
//!    names every dropped item, and the layer says so in a line of its own.
//!
//! Layer 8 is computed last from what is left.

use crate::build::*;
use crate::draft::Draft;
use crate::error::ContextError;
use crate::estimate::{Estimator, Profile};
use crate::input::PackInput;
use crate::layer::{LayerId, UnverifiedPolicy};
use crate::limits::PACK_VERSION;
use crate::pack::*;
use crate::text::Hygiene;
use fleet_domain::approval::Digest;
use fleet_domain::redact::Redactor;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Compiler {
    profile: Profile,
    hygiene: Hygiene,
    policy: UnverifiedPolicy,
}

impl Compiler {
    /// `redactor` is run over every piece of text that reaches a layer.
    pub fn new(profile: Profile, redactor: Arc<Redactor>, policy: UnverifiedPolicy) -> Compiler {
        Compiler {
            profile,
            hygiene: Hygiene::new(redactor),
            policy,
        }
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn policy(&self) -> UnverifiedPolicy {
        self.policy
    }

    pub fn hygiene(&self) -> &Hygiene {
        &self.hygiene
    }

    pub fn compile(&self, input: &PackInput, hasher: &dyn Digest) -> Result<Pack, ContextError> {
        let est = self.profile.estimator();
        let h = &self.hygiene;
        let mut drafts: Vec<Draft> = vec![
            invariants_layer(),
            charter_layer(input, h),
            plan_layer(input, h),
            task_layer(input, h),
            knowledge_layer(input, h)?,
            working_set_layer(input, h, hasher),
            events_layer(input, h, hasher, self.policy),
        ];

        // 1. every layer within its own budget
        for d in &mut drafts {
            let budget = self.profile.layer_budget(d.id);
            loop {
                let needed = self.tokens_of(est, d);
                if needed <= budget {
                    break;
                }
                if !d.shed_one() {
                    return Err(ContextError::LayerOverflow {
                        layer: d.id,
                        needed,
                        budget,
                    });
                }
            }
        }

        // 2 and 3. the pack within the profile
        let max = u64::from(self.profile.max_tokens());
        let name = self.profile.name().to_string();
        let envelope = envelope_tokens(est, &name);
        let scratch_frame = self.scratch_tokens(est);
        let total = |drafts: &[Draft]| -> u64 {
            envelope + scratch_frame + drafts.iter().map(|d| self.tokens_of(est, d)).sum::<u64>()
        };
        let shrink_order = [
            LayerId::RecentEvents,
            LayerId::WorkingSet,
            LayerId::Knowledge,
            LayerId::Charter,
        ];
        'shrink: while total(&drafts) > max {
            for id in shrink_order {
                if let Some(d) = drafts.iter_mut().find(|d| d.id == id) {
                    if d.shed_one() {
                        continue 'shrink;
                    }
                }
            }
            return Err(ContextError::PackOverflow {
                needed: total(&drafts),
                max,
            });
        }
        let used = total(&drafts);
        let left = max.saturating_sub(used);
        drafts.push(scratch_layer(self.scratch_body(used, left)));

        // render
        let mut rendered = Vec::with_capacity(drafts.len());
        let mut records = Vec::with_capacity(drafts.len());
        for d in &drafts {
            let body = d.escaped_body();
            let trust = d.trust_label().to_string();
            let tokens = frame_tokens(est, d.id, &trust, body.len());
            let digest = hasher.sha256_hex(body.as_bytes());
            records.push(LayerRecord {
                id: d.id,
                tokens,
                bytes: body.len() as u64,
                budget: self.profile.layer_budget(d.id),
                digest: digest.clone(),
                trust: trust.clone(),
                items: u32::try_from(d.item_count()).unwrap_or(u32::MAX),
                dropped: d.dropped.clone(),
                unverified: u32::try_from(d.unverified_count()).unwrap_or(u32::MAX),
                excluded: d.excluded,
                clipped: d.clipped,
            });
            rendered.push(Rendered {
                id: d.id,
                trust,
                body,
                tokens,
                digest,
            });
        }
        let total_tokens = envelope + records.iter().map(|r| r.tokens).sum::<u64>();
        let text = render(&name, &rendered);
        let manifest = Manifest {
            version: PACK_VERSION,
            profile: ProfileRecord {
                name,
                max_tokens: self.profile.max_tokens(),
                chars_per_token: est.chars_per_token(),
                safety_percent: est.safety_percent(),
            },
            policy: self.policy,
            subject: input.subject.clone(),
            handoff: input.handoff.as_ref().map(|h| h.digest().to_string()),
            stable_digest: stable_digest(&records, hasher),
            layers: records,
            total_tokens,
            remaining_tokens: max.saturating_sub(total_tokens),
        };
        let digest = pack_digest(&manifest, &text, hasher);
        Ok(Pack {
            manifest,
            text,
            digest,
        })
    }

    fn tokens_of(&self, est: Estimator, d: &Draft) -> u64 {
        frame_tokens(est, d.id, d.trust_label(), d.escaped_body().len())
    }

    fn scratch_body(&self, used: u64, left: u64) -> String {
        let est = self.profile.estimator();
        scratch_text(
            self.profile.name(),
            self.profile.max_tokens(),
            est.safety_percent(),
            est.chars_per_token(),
            used,
            left,
        )
    }

    /// The tokens of layer 8, which are the same whatever the numbers in it (fixed width).
    fn scratch_tokens(&self, est: Estimator) -> u64 {
        let draft = scratch_layer(self.scratch_body(0, 0));
        self.tokens_of(est, &draft)
    }
}
