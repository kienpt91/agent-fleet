//! The operator's view of context packs: `context show|diff|verify|explain` and `handoff`.
//! Reads never change anything; `handoff` records the packet it makes.

use super::data;
use crate::api::Reply;
use crate::orchestrator::{Key, Orchestrator};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::value::Value;
use neo_context::handoff::HandoffTrigger;
use neo_context::layer::LayerId;
use neo_context::limits::LAYER_PERCENT;
use neo_context::pack::{diff, read_document, Manifest, ManifestDiff};

fn no_pack(key: &Key, digest: Option<&str>) -> FleetError {
    match digest {
        Some(d) => FleetError::not_found(format!(
            "no stored context pack of {}/{} has a digest starting with {}",
            key.0,
            key.1,
            fleet_domain::ids::show(d)
        )),
        None => FleetError::not_found(format!(
            "no compiled context is stored for {}/{}: its run has to use context = \"compiled\" and the task has to have been started",
            key.0, key.1
        )),
    }
}

fn layers_value(m: &Manifest) -> Value {
    Value::array(
        m.layers
            .iter()
            .map(neo_context::pack::LayerRecord::to_value),
    )
}

fn tokens_pair(a: u64, b: u64) -> Value {
    Value::object([("a", Value::int(a)), ("b", Value::int(b))])
}

fn diff_layers(d: &ManifestDiff) -> Value {
    Value::array(d.layers.iter().map(|l| {
        Value::object([
            ("id", Value::int(u64::from(l.id.number()))),
            ("name", Value::str(l.id.name())),
            ("changed", Value::Bool(l.changed)),
            ("tokens", tokens_pair(l.tokens.0, l.tokens.1)),
            (
                "digest",
                Value::object([
                    ("a", Value::str(l.digest.0.as_str())),
                    ("b", Value::str(l.digest.1.as_str())),
                ]),
            ),
        ])
    }))
}

impl Orchestrator {
    fn stored_manifest(
        &self,
        key: &Key,
        digest: Option<&str>,
    ) -> Result<(String, String, Manifest)> {
        let stored = self
            .ports
            .context
            .load_pack(&key.0, &key.1, digest)?
            .ok_or_else(|| no_pack(key, digest))?;
        let (doc_digest, manifest) =
            read_document(&stored.document).map_err(super::context::context_error)?;
        if doc_digest != stored.digest {
            return Err(FleetError::internal(
                "a stored context pack does not match its own digest",
            ));
        }
        Ok((stored.digest, stored.text, manifest))
    }

    pub(crate) fn context_show(&self, task: &str, digest: Option<&str>) -> Result<Reply> {
        let key = self.state.find_task(task)?;
        let (digest, text, m) = self.stored_manifest(&key, digest)?;
        Ok(data(
            "fleet.context.show/1",
            vec![
                ("run", Value::str(key.0.as_str())),
                ("task", Value::str(key.1.as_str())),
                ("digest", Value::str(digest)),
                ("stable_digest", Value::str(m.stable_digest.as_str())),
                ("profile", Value::str(m.profile.name.as_str())),
                ("tokens", Value::int(m.total_tokens)),
                ("max_tokens", Value::int(u64::from(m.profile.max_tokens))),
                ("layers", layers_value(&m)),
                ("manifest", m.to_value()),
                ("text", Value::str(text)),
            ],
        ))
    }

    pub(crate) fn context_diff(&self, task: &str, a: &str, b: &str) -> Result<Reply> {
        let key = self.state.find_task(task)?;
        let (da, _, ma) = self.stored_manifest(&key, Some(a))?;
        let (db, _, mb) = self.stored_manifest(&key, Some(b))?;
        let d = diff(&ma, &mb);
        Ok(data(
            "fleet.context.diff/1",
            vec![
                ("run", Value::str(key.0.as_str())),
                ("task", Value::str(key.1.as_str())),
                ("a", Value::str(da.as_str())),
                ("b", Value::str(db.as_str())),
                ("same", Value::Bool(da == db)),
                ("stable_same", Value::Bool(d.stable_same)),
                ("tokens", tokens_pair(d.total_tokens.0, d.total_tokens.1)),
                ("layers", diff_layers(&d)),
            ],
        ))
    }

    /// Compiles the newest stored pack's task again from the state as it is now and compares.
    pub(crate) fn context_verify(&self, task: &str) -> Result<Reply> {
        let key = self.state.find_task(task)?;
        let (stored, _, old) = self.stored_manifest(&key, None)?;
        if !self.uses_compiled_context(&key) {
            return Err(FleetError::conflict(format!(
                "run {} does not use compiled context",
                key.0
            )));
        }
        let current =
            self.compile_pack_as_of(&key, old.subject.attempt, &stored, old.handoff.as_deref())?;
        let d = diff(&old, current.manifest());
        Ok(data(
            "fleet.context.verify/1",
            vec![
                ("run", Value::str(key.0.as_str())),
                ("task", Value::str(key.1.as_str())),
                ("match", Value::Bool(stored == current.digest())),
                ("stored", Value::str(stored.as_str())),
                ("current", Value::str(current.digest())),
                ("stable_same", Value::Bool(d.stable_same)),
                ("layers", diff_layers(&d)),
            ],
        ))
    }

    pub(crate) fn context_explain(
        &self,
        task: Option<&str>,
        profile: Option<&str>,
    ) -> Result<Reply> {
        let (profile, subject) = match (task, profile) {
            (Some(t), _) => {
                let key = self.state.find_task(t)?;
                let compiler = self.compiler_for(&key)?;
                (
                    compiler.profile().clone(),
                    Value::object([
                        ("run", Value::str(key.0.as_str())),
                        ("task", Value::str(key.1.as_str())),
                    ]),
                )
            }
            (None, name) => (self.cfg.context.profile_for(name)?.clone(), Value::Null),
        };
        let est = profile.estimator();
        let layers = Value::array(LayerId::ALL.iter().map(|l| {
            let percent = LAYER_PERCENT
                .get(usize::from(l.number()).saturating_sub(1))
                .copied()
                .unwrap_or(0);
            Value::object([
                ("id", Value::int(u64::from(l.number()))),
                ("name", Value::str(l.name())),
                ("purpose", Value::str(l.purpose())),
                ("percent", Value::int(u64::from(percent))),
                ("budget", Value::int(profile.layer_budget(*l))),
                ("sheddable", Value::Bool(l.sheddable())),
            ])
        }));
        Ok(data(
            "fleet.context.explain/1",
            vec![
                (
                    "profile",
                    Value::object([
                        ("name", Value::str(profile.name())),
                        ("max_tokens", Value::int(u64::from(profile.max_tokens()))),
                        (
                            "chars_per_token",
                            Value::int(u64::from(est.chars_per_token())),
                        ),
                        (
                            "safety_percent",
                            Value::int(u64::from(est.safety_percent())),
                        ),
                    ]),
                ),
                ("policy", Value::str(self.cfg.context.policy.as_str())),
                (
                    "knowledge_file",
                    self.cfg
                        .context
                        .knowledge
                        .as_deref()
                        .map_or(Value::Null, Value::str),
                ),
                (
                    "estimator",
                    Value::str(format!(
                        "tokens = ceil(bytes x {}% / ({} x 100)), counted over UTF-8 bytes",
                        est.safety_percent(),
                        est.chars_per_token()
                    )),
                ),
                (
                    "shrink_order",
                    Value::array([7u64, 6, 5, 2].into_iter().map(Value::int)),
                ),
                ("layers", layers),
                ("subject", subject),
            ],
        ))
    }

    pub(crate) fn handoff_command(&mut self, task: &str) -> Result<Reply> {
        let key = self.state.find_task(task)?;
        let (packet, created) = self.make_handoff(&key, HandoffTrigger::OnDemand)?;
        Ok(data(
            "fleet.handoff/1",
            vec![
                ("run", Value::str(key.0.as_str())),
                ("task", Value::str(key.1.as_str())),
                ("digest", Value::str(packet.digest())),
                ("created", Value::Bool(created)),
                ("trigger", Value::str(packet.trigger.as_str())),
                ("text", Value::str(packet.render())),
                ("packet", packet.to_value()),
            ],
        ))
    }
}
