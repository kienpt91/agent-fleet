//! NEO use cases: the goal ledger and verification contracts, on top of the orchestrator.
//!
//! The ledger is a projection of the event log, like the fleet state: `Orchestrator::commit`
//! folds every stored event into it, and a restart rebuilds it by replay, so goals, contracts and
//! evidence survive a crash exactly as runs and tasks do. The use cases here validate a request
//! against the domain, record events, and start the verification jobs whose results become
//! evidence; the scheduler rule (a task with a contract completes only after fleet's own run
//! passed it) is enforced in the review flow (`verify.rs`).

mod goals;
mod verify;
pub mod views;

use crate::api::Reply;
use fleet_domain::value::Value;

/// A reply document with its schema name first.
pub(crate) fn data(schema: &str, pairs: Vec<(&str, Value)>) -> Reply {
    let mut map: Vec<(String, Value)> = vec![("schema".to_string(), Value::str(schema))];
    map.extend(pairs.into_iter().map(|(k, v)| (k.to_string(), v)));
    Reply::Data(Value::object(map))
}
