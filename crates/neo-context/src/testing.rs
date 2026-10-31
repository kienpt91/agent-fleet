//! Test support: a plain SHA-256 (the domain, like every domain crate, has no hashing dependency;
//! the golden digests in the tests are real SHA-256 values and the adapters' own hash must agree
//! with them) and fixtures shared by the tests of this crate and of the layers above it. Checked
//! against the FIPS 180-4 test vectors below.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::needless_range_loop
)]

use crate::compile::Compiler;
use crate::estimate::Profile;
use crate::handoff::{HandoffPacket, HandoffParts, HandoffTrigger};
use crate::input::*;
use crate::layer::UnverifiedPolicy;
use fleet_domain::approval::Digest;
use fleet_domain::redact::Redactor;
use std::sync::Arc;

/// Sixty-four lowercase hex digits for a `char`, for building ids in tests.
pub fn hex64(c: char) -> String {
    c.to_string().repeat(64)
}

/// A compiler with the standard redactor and the given profile size.
pub fn compiler(max_tokens: u32, policy: UnverifiedPolicy) -> Compiler {
    let profile = Profile::new("test", max_tokens, 4, 125).unwrap();
    Compiler::new(profile, Arc::new(Redactor::builtin()), policy)
}

/// A complete, ordinary input: every layer has something to show.
pub fn sample_input() -> PackInput {
    let base = "a".repeat(40);
    PackInput {
        subject: Subject {
            run: "r1".to_string(),
            task: "api".to_string(),
            attempt: 1,
            base: base.clone(),
            head: None,
        },
        charter: CharterInput {
            objective: "Ship the billing API\nwith tests".to_string(),
            constraints: vec![
                "No new dependencies".to_string(),
                "Keep the public API source compatible".to_string(),
            ],
            glossary: vec![GlossaryEntry {
                term: "Invoice".to_string(),
                meaning: "a bill sent to a customer".to_string(),
            }],
        },
        plan: PlanInput {
            goals: vec![GoalLine {
                id: "billing".to_string(),
                title: "Billing works".to_string(),
                status: "in_progress".to_string(),
                intent: "Invoices can be created and listed".to_string(),
                ancestors: vec![("product".to_string(), "The product".to_string())],
            }],
            done: vec![TaskLine {
                id: "schema".to_string(),
                title: "Define the schema".to_string(),
            }],
            remaining: vec![
                TaskLine {
                    id: "api".to_string(),
                    title: "Write the API".to_string(),
                },
                TaskLine {
                    id: "docs".to_string(),
                    title: "Document it".to_string(),
                },
            ],
            dependencies: vec![DependencyLine {
                id: "schema".to_string(),
                title: "Define the schema".to_string(),
                done: true,
            }],
        },
        task: TaskInput {
            id: "api".to_string(),
            title: "Write the API".to_string(),
            spec: "Add `create_invoice` and `list_invoices` to src/api.rs.\nBoth return Result."
                .to_string(),
            contracts: vec![ContractLines {
                goal: "billing".to_string(),
                contract: "0123456789ab".to_string(),
                checks: vec![
                    CheckLine {
                        id: "tests".to_string(),
                        mandatory: true,
                        kind: "command".to_string(),
                        what: "/usr/bin/make test (expect exit 0)".to_string(),
                    },
                    CheckLine {
                        id: "scope".to_string(),
                        mandatory: false,
                        kind: "diff".to_string(),
                        what: "only src/** and tests/**".to_string(),
                    },
                ],
            }],
            verification: vec![VerificationLine {
                goal: "billing".to_string(),
                check: "tests".to_string(),
                mandatory: true,
                state: CheckState::NotRun,
                commit: None,
                evidence: None,
            }],
        },
        knowledge: vec![Fact {
            id: "k1".to_string(),
            claim: "Amounts are integer cents".to_string(),
            source: "docs/money.md at commit 0123456".to_string(),
            freshness: "checked by a human".to_string(),
        }],
        working_set: WorkingSetInput {
            files: vec![Excerpt {
                path: "src/api.rs".to_string(),
                blob: "b".repeat(40),
                size: 39,
                bytes: b"pub fn existing() {}\n// api lives here\n".to_vec(),
                selection: Selection::Allowed,
            }],
        },
        events: vec![
            EventLine {
                seq: 4,
                text: "r1/api ready".to_string(),
            },
            EventLine {
                seq: 5,
                text: "r1/api dispatched (attempt 1)".to_string(),
            },
        ],
        unverified: UnverifiedInput {
            notes: vec![NoteInput {
                text: "Operator says: use cents".to_string(),
            }],
            dependency_summaries: vec![DependencySummary {
                task: "schema".to_string(),
                title: "Define the schema".to_string(),
                summary: "added tables".to_string(),
            }],
            ..UnverifiedInput::default()
        },
        handoff: None,
    }
}

/// A handoff for [`sample_input`]'s task, made from a pack compiled with `compiler`.
pub fn sample_handoff(
    c: &Compiler,
    trigger: HandoffTrigger,
    summary: Option<&str>,
) -> HandoffPacket {
    let pack = c.compile(&sample_input(), &Sha256Ref).unwrap();
    HandoffPacket::new(
        HandoffParts {
            run: "r1".to_string(),
            task: "api".to_string(),
            attempt: 1,
            trigger,
            verification: sample_input().task.verification,
            evidence: vec!["e1".to_string()],
            summary: summary.map(str::to_string),
        },
        &pack,
        c.hygiene(),
        &Sha256Ref,
    )
}

/// A plain SHA-256 for tests.
pub struct Sha256Ref;

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Digest for Sha256Ref {
    fn sha256_hex(&self, data: &[u8]) -> String {
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut msg = data.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
        for block in msg.chunks(64) {
            let mut w = [0u32; 64];
            for (i, word) in block.chunks(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
                *slot = slot.wrapping_add(v);
            }
        }
        h.iter().map(|w| format!("{w:08x}")).collect()
    }
}

#[test]
fn matches_the_fips_vectors() {
    assert_eq!(
        Sha256Ref.sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        Sha256Ref.sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        Sha256Ref.sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    assert_eq!(
        Sha256Ref.sha256_hex(&vec![b'a'; 1_000_000]),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
}
