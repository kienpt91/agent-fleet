//! Hash chain of the audit log.
//!
//! Every record carries the hash of the record before it and its own hash over
//! `seq`, `ts`, `prev` and the canonical text of its body. Changing, removing, reordering or
//! inserting a record breaks every hash after it. A chain proves consistency, not authorship:
//! anyone who can write the whole file can rebuild a valid chain, so the head (record count and
//! last hash) is also kept in a second place and compared on load to catch truncation.

use crate::approval::Digest;
use crate::value::Value;
use std::fmt;

/// The `prev` of the first record.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub seq: u64,
    pub ts_ms: u64,
    pub prev: String,
    pub body: Value,
    pub hash: String,
}

pub fn compute_hash(hasher: &dyn Digest, seq: u64, ts_ms: u64, prev: &str, body: &Value) -> String {
    let text = format!("{seq}\n{ts_ms}\n{prev}\n{}", body.canonical());
    hasher.sha256_hex(text.as_bytes())
}

/// Builds the next link.
pub fn seal(hasher: &dyn Digest, seq: u64, ts_ms: u64, prev: &str, body: Value) -> Link {
    let hash = compute_hash(hasher, seq, ts_ms, prev, &body);
    Link {
        seq,
        ts_ms,
        prev: prev.to_string(),
        body,
        hash,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainFault {
    /// A record is missing, repeated or out of order.
    Sequence {
        expected: u64,
        found: u64,
    },
    /// The record does not point at the one before it.
    Previous {
        seq: u64,
    },
    /// The record's content does not match its hash.
    Hash {
        seq: u64,
    },
    /// The log is shorter than the head recorded elsewhere, or ends with another record.
    Truncated {
        expected_records: u64,
        found_records: u64,
    },
    HeadMismatch,
}

impl fmt::Display for ChainFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainFault::Sequence { expected, found } => {
                write!(
                    f,
                    "record {found} found where record {expected} was expected"
                )
            }
            ChainFault::Previous { seq } => {
                write!(f, "record {seq} does not follow the record before it")
            }
            ChainFault::Hash { seq } => write!(f, "record {seq} was modified"),
            ChainFault::Truncated {
                expected_records,
                found_records,
            } => write!(
                f,
                "the log has {found_records} records but {expected_records} were recorded"
            ),
            ChainFault::HeadMismatch => {
                write!(f, "the last record does not match the recorded head")
            }
        }
    }
}

impl std::error::Error for ChainFault {}

/// Walks records in order and checks each against the one before.
pub struct ChainVerifier<'a> {
    hasher: &'a dyn Digest,
    next_seq: u64,
    prev: String,
}

impl<'a> ChainVerifier<'a> {
    pub fn new(hasher: &'a dyn Digest) -> Self {
        ChainVerifier {
            hasher,
            next_seq: 1,
            prev: GENESIS.to_string(),
        }
    }

    pub fn check(&mut self, link: &Link) -> Result<(), ChainFault> {
        if link.seq != self.next_seq {
            return Err(ChainFault::Sequence {
                expected: self.next_seq,
                found: link.seq,
            });
        }
        if link.prev != self.prev {
            return Err(ChainFault::Previous { seq: link.seq });
        }
        if compute_hash(self.hasher, link.seq, link.ts_ms, &link.prev, &link.body) != link.hash {
            return Err(ChainFault::Hash { seq: link.seq });
        }
        self.next_seq = link.seq.saturating_add(1);
        self.prev = link.hash.clone();
        Ok(())
    }

    /// Number of records verified so far.
    pub fn records(&self) -> u64 {
        self.next_seq - 1
    }

    /// Hash of the last verified record (the genesis value when there is none).
    pub fn head_hash(&self) -> &str {
        &self.prev
    }

    /// Compares the verified chain with a head recorded elsewhere.
    pub fn check_head(&self, recorded_records: u64, recorded_hash: &str) -> Result<(), ChainFault> {
        let found = self.records();
        if found < recorded_records {
            return Err(ChainFault::Truncated {
                expected_records: recorded_records,
                found_records: found,
            });
        }
        // A head that lags the log by whole records is normal after a crash between the two
        // writes, so only an equal count must have an equal hash.
        if found == recorded_records && self.prev != recorded_hash {
            return Err(ChainFault::HeadMismatch);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::test_support::FakeDigest;

    fn body(n: i64) -> Value {
        Value::object([("n", Value::int(n)), ("type", Value::str("warning"))])
    }

    fn chain(len: u64) -> Vec<Link> {
        let mut out: Vec<Link> = Vec::new();
        let mut prev = GENESIS.to_string();
        for i in 1..=len {
            let link = seal(&FakeDigest, i, 1000 + i, &prev, body(i as i64));
            prev = link.hash.clone();
            out.push(link);
        }
        out
    }

    fn verify(links: &[Link]) -> Result<(), ChainFault> {
        let mut v = ChainVerifier::new(&FakeDigest);
        links.iter().try_for_each(|l| v.check(l))
    }

    #[test]
    fn an_untouched_chain_verifies() {
        assert_eq!(verify(&chain(25)), Ok(()));
        assert_eq!(verify(&[]), Ok(()));
    }

    #[test]
    fn modifying_a_record_is_detected() {
        let mut c = chain(5);
        c[2].body = body(999);
        assert_eq!(verify(&c), Err(ChainFault::Hash { seq: 3 }));
        let mut c = chain(5);
        c[3].ts_ms += 1;
        assert_eq!(verify(&c), Err(ChainFault::Hash { seq: 4 }));
    }

    #[test]
    fn removing_or_reordering_records_is_detected() {
        let mut c = chain(5);
        c.remove(2);
        assert_eq!(
            verify(&c),
            Err(ChainFault::Sequence {
                expected: 3,
                found: 4
            })
        );
        let mut c = chain(5);
        c.swap(1, 2);
        assert!(matches!(verify(&c), Err(ChainFault::Sequence { .. })));
        let mut c = chain(3);
        c.remove(0);
        assert_eq!(
            verify(&c),
            Err(ChainFault::Sequence {
                expected: 1,
                found: 2
            })
        );
    }

    #[test]
    fn a_record_that_points_elsewhere_is_detected() {
        let mut c = chain(4);
        c[2].prev = GENESIS.to_string();
        assert_eq!(verify(&c), Err(ChainFault::Previous { seq: 3 }));
    }

    #[test]
    fn rewriting_a_record_and_its_hash_still_breaks_the_next_one() {
        let mut c = chain(4);
        c[1].body = body(777);
        c[1].hash = compute_hash(&FakeDigest, 2, c[1].ts_ms, &c[1].prev, &c[1].body);
        assert_eq!(verify(&c), Err(ChainFault::Previous { seq: 3 }));
    }

    #[test]
    fn head_comparison_catches_truncation() {
        let c = chain(6);
        let mut v = ChainVerifier::new(&FakeDigest);
        for l in &c[..4] {
            v.check(l).unwrap();
        }
        assert_eq!(
            v.check_head(6, &c[5].hash),
            Err(ChainFault::Truncated {
                expected_records: 6,
                found_records: 4
            })
        );
        assert_eq!(v.check_head(4, &c[3].hash), Ok(()));
        assert_eq!(v.check_head(4, &c[2].hash), Err(ChainFault::HeadMismatch));
        // the head file may lag behind the log after a crash
        assert_eq!(v.check_head(3, &c[2].hash), Ok(()));
    }

    #[test]
    fn verifier_reports_progress() {
        let c = chain(3);
        let mut v = ChainVerifier::new(&FakeDigest);
        assert_eq!((v.records(), v.head_hash()), (0, GENESIS));
        for l in &c {
            v.check(l).unwrap();
        }
        assert_eq!(v.records(), 3);
        assert_eq!(v.head_hash(), c[2].hash);
    }

    #[test]
    fn faults_have_readable_messages() {
        assert!(ChainFault::Hash { seq: 9 }
            .to_string()
            .contains("record 9 was modified"));
        assert!(ChainFault::HeadMismatch.to_string().contains("head"));
        assert!(ChainFault::Sequence {
            expected: 1,
            found: 2
        }
        .to_string()
        .contains("expected"));
        assert!(ChainFault::Previous { seq: 4 }.to_string().contains('4'));
        assert!(ChainFault::Truncated {
            expected_records: 2,
            found_records: 1
        }
        .to_string()
        .contains("2 were recorded"));
    }
}
