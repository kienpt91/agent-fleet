//! Context compiler of NEO: what an agent is told before it starts, compiled deterministically from
//! state, in a fixed order, within a token budget, with a digest.
//!
//! A [`pack::Pack`] has eight layers (see [`layer::LayerId`]). The [`compile::Compiler`] builds one
//! from a [`input::PackInput`]: no model, no clock, no randomness; the same inputs give the same
//! bytes and the same SHA-256 digest. Text that agents wrote is placed in layer 7 only, labelled
//! UNVERIFIED and quoted line by line, or left out by policy. The pack is rendered to a canonical
//! text with length-prefixed framing, plus a canonical JSON manifest listing every layer with its
//! token count, digest and what was cut. A [`handoff::HandoffPacket`] carries a pack's manifest, the
//! open verification state and pointers to evidence from one attempt to the next.
//!
//! Like `fleet-domain` the crate is pure: std and `fleet-domain` only. Hashing reaches it through the
//! domain's `Digest` port.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

pub mod compile;
pub mod error;
pub mod estimate;
pub mod handoff;
pub mod input;
pub mod invariants;
pub mod layer;
pub mod limits;
pub mod pack;
pub mod secret;
pub mod text;

mod build;
mod draft;
/// A reference SHA-256 and fixtures, for the tests of this crate and of the layers above it.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
