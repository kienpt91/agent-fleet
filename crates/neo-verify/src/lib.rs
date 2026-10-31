//! Verification context of NEO: what it takes for work to count as done, and what was seen when
//! fleet itself checked.
//!
//! A [`contract::VerificationContract`] is a list of checks (a command with an expected exit
//! code, a `refcheck-json` gate, a file assertion, a diff constraint). Running a check is an
//! adapter concern; this crate judges what was observed ([`judge`]) and keeps the result as
//! immutable [`evidence::Evidence`]. Like `fleet-domain` it is pure: std only, no I/O, no clock.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

pub mod contract;
pub mod evidence;
pub mod glob;
pub mod judge;
pub mod limits;
