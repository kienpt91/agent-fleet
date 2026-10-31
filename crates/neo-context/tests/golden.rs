//! Golden vectors: the digests of fixed inputs, written down. They are real SHA-256 values of the
//! canonical text, so they are the same on every platform and run; a change here means the rendered
//! form, the manifest or the layer content changed, which is a new pack version on purpose. The
//! adapters check the same values with their own hash (`fleet-infra`), so the two implementations
//! of SHA-256 agree with each other and with these numbers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use neo_context::handoff::HandoffTrigger;
use neo_context::input::*;
use neo_context::layer::UnverifiedPolicy;
use neo_context::pack::{read_document, Manifest};
use neo_context::testing::{compiler, sample_handoff, sample_input, Sha256Ref};

pub const SAMPLE_PACK: &str = "edc8af0fd4eadee87af23aa25f836ab092437c643984897bedd78399189fb612";
pub const SAMPLE_STABLE: &str = "460042244d8c1a5e5ca60d77e9e33a766c33349e4212837122a94651ca1a4a7e";
pub const SAMPLE_LAYERS: [&str; 8] = [
    "61ddecdb95efe2693057d1e9827969680c6c3eddcd493f80114b493d461f2cc2",
    "a668c44bd1640ca6c35ac9549ce6b4439d1952cd993226fa80f97431da2b6590",
    "311df98459c38973cc527caaf0aa11a8d4bc9ffb6aaafc227279d03d8355f5f7",
    "05cbe0bbe3306b15d7149f8341cb57fcbbeff3139b990db22787c46f1e5ccfed",
    "b929985f453f9cd4cf2e8abf1f487e0f02fa65c1dd0ea377fff887dd85769231",
    "787237f6eb2df38a674bc262a3a06ace971e66fc85d2f379730af622c2cf5e2b",
    "341ed30f8c8c3e3566dd4dea7985596dc7e49e0b4e8b8facfa8502feee9f02a6",
    "f704b5213d0529f9eaaa6424cccc49eef41a6a5832f7f45692de0604d0b03a29",
];
pub const SAMPLE_MANIFEST_BYTES: usize = 2244;
pub const SAMPLE_HANDOFF: &str = "06866e5d219d97c68b93ece51e5803061b6f802480e3d2250c3390c551dba622";
pub const SAMPLE_WITH_HANDOFF: &str =
    "e021fa86276a41cf0bf7bacdee77130cd5e86a0a76e9c7fbf212c9d92d310bc1";
pub const SAMPLE_EXCLUDED: &str =
    "d7343809c467b85d57b49892002eebd3215fb2485811107ca541496d0c816db6";

#[test]
fn the_sample_pack_has_the_digests_written_down() {
    let c = compiler(8192, UnverifiedPolicy::Label);
    let p = c.compile(&sample_input(), &Sha256Ref).unwrap();
    assert_eq!(p.digest(), SAMPLE_PACK);
    assert_eq!(p.stable_digest(), SAMPLE_STABLE);
    let layers: Vec<&str> = p
        .manifest()
        .layers
        .iter()
        .map(|l| l.digest.as_str())
        .collect();
    assert_eq!(layers, SAMPLE_LAYERS);
    assert_eq!(p.manifest().canonical().len(), SAMPLE_MANIFEST_BYTES);
}

#[test]
fn a_handoff_and_a_policy_change_the_digest_and_only_as_documented() {
    let c = compiler(8192, UnverifiedPolicy::Label);
    let h = sample_handoff(&c, HandoffTrigger::Retry, Some("summary"));
    assert_eq!(h.digest(), SAMPLE_HANDOFF);
    let mut i = sample_input();
    i.handoff = Some(h);
    let with = c.compile(&i, &Sha256Ref).unwrap();
    assert_eq!(with.digest(), SAMPLE_WITH_HANDOFF);
    assert_eq!(
        with.stable_digest(),
        SAMPLE_STABLE,
        "a handoff does not touch layers 1 to 5"
    );
    let excluded = compiler(8192, UnverifiedPolicy::Exclude)
        .compile(&sample_input(), &Sha256Ref)
        .unwrap();
    assert_eq!(excluded.digest(), SAMPLE_EXCLUDED);
    assert_eq!(excluded.stable_digest(), SAMPLE_STABLE);
}

#[test]
fn the_digest_does_not_depend_on_how_the_input_was_built() {
    // the same facts reached by another route: cloned, rebuilt field by field, strings rebuilt
    let a = sample_input();
    let b = PackInput {
        subject: Subject {
            ..a.subject.clone()
        },
        charter: CharterInput {
            objective: a.charter.objective.chars().collect(),
            constraints: a
                .charter
                .constraints
                .iter()
                .map(|s| s.chars().collect())
                .collect(),
            glossary: a.charter.glossary.clone(),
        },
        ..a.clone()
    };
    let c = compiler(8192, UnverifiedPolicy::Label);
    assert_eq!(
        c.compile(&a, &Sha256Ref).unwrap().digest(),
        c.compile(&b, &Sha256Ref).unwrap().digest()
    );
}

#[test]
fn the_stored_document_reads_back_to_the_same_manifest() {
    let c = compiler(8192, UnverifiedPolicy::Label);
    let p = c.compile(&sample_input(), &Sha256Ref).unwrap();
    let json = p.document().canonical();
    // the document is canonical JSON: a stable text of its own
    assert!(json.starts_with("{\"digest\":\"edc8af0f"));
    assert!(json.contains("\"schema\":\"neo-context/1\""));
    let (digest, manifest) = read_document(&p.document()).unwrap();
    assert_eq!(digest, SAMPLE_PACK);
    assert_eq!(&manifest, p.manifest());
    assert_eq!(
        Manifest::from_value(&manifest.to_value()).unwrap(),
        manifest
    );
}
