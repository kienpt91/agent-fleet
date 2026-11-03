//! Why a pack could not be compiled. Never a silent truncation: when the layers that may not be
//! cut do not fit, compiling fails and says by how much.

use crate::layer::LayerId;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextError {
    /// A model profile that cannot be used (name, budget or estimator out of range).
    InvalidProfile(String),
    /// An input the compiler refuses outright (for example a fact without a source).
    InvalidInput(String),
    /// A layer that is never shortened (or whose parts are all required) needs more tokens than
    /// its own budget.
    LayerOverflow {
        layer: LayerId,
        needed: u64,
        budget: u64,
    },
    /// After every layer that may be dropped or shortened was, the pack still needs more tokens
    /// than the profile allows.
    PackOverflow { needed: u64, max: u64 },
    /// A rendered pack or a stored manifest that does not parse or does not match itself.
    Malformed(String),
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContextError::InvalidProfile(why) => write!(f, "invalid context profile: {why}"),
            ContextError::InvalidInput(why) => write!(f, "invalid context input: {why}"),
            ContextError::LayerOverflow {
                layer,
                needed,
                budget,
            } => write!(
                f,
                "layer {} ({}) needs about {needed} tokens but its budget is {budget}; it is never cut, so raise max_tokens of the profile or shorten the input",
                layer.number(),
                layer.name()
            ),
            ContextError::PackOverflow { needed, max } => write!(
                f,
                "the layers that cannot be dropped (invariants, charter, plan, task) need about {needed} tokens but the profile allows {max}; raise max_tokens or shorten the task"
            ),
            ContextError::Malformed(why) => write!(f, "malformed context pack: {why}"),
        }
    }
}

impl std::error::Error for ContextError {}
