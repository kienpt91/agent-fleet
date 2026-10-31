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

pub const SAMPLE_PACK: &str = "7a46e27d834b4402df690b7a17fdc0275159cb849725bed49db82d6d71776a0c";
pub const SAMPLE_STABLE: &str = "95caec77bf88dbcfc2de12602522db60a49ed2689cd4b5e5f4d9fcdcedf43c26";
pub const SAMPLE_LAYERS: [&str; 8] = [
    "ccd7c687028af53a9505cf07cf9f36573b06168ed3314db42c7c978a3b9a52bf",
    "a668c44bd1640ca6c35ac9549ce6b4439d1952cd993226fa80f97431da2b6590",
    "311df98459c38973cc527caaf0aa11a8d4bc9ffb6aaafc227279d03d8355f5f7",
    "05cbe0bbe3306b15d7149f8341cb57fcbbeff3139b990db22787c46f1e5ccfed",
    "b929985f453f9cd4cf2e8abf1f487e0f02fa65c1dd0ea377fff887dd85769231",
    "787237f6eb2df38a674bc262a3a06ace971e66fc85d2f379730af622c2cf5e2b",
    "341ed30f8c8c3e3566dd4dea7985596dc7e49e0b4e8b8facfa8502feee9f02a6",
    "0720b63350ce2c84cabbb5ef98b63719836bacb576c679af2f64fa333f089322",
];
pub const SAMPLE_MANIFEST_BYTES: usize = 2244;
pub const SAMPLE_HANDOFF: &str = "3354fb1fffe6fe0f5fe4b6638ab99ea1d7fadf05819c6995ad8827d4b18144d6";
pub const SAMPLE_WITH_HANDOFF: &str =
    "a901528edc1ceb6a59cbebaddfde87dd71fdc48d26b7cb3316502aafebd2a2b8";
pub const SAMPLE_EXCLUDED: &str =
    "477b282a8ec93c7d01ecf8800c1d8e0e4df80fda415155c155c0a2c0a69ac5e8";

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
    assert!(json.starts_with("{\"digest\":\"7a46e27d"));
    assert!(json.contains("\"schema\":\"neo-context/1\""));
    let (digest, manifest) = read_document(&p.document()).unwrap();
    assert_eq!(digest, SAMPLE_PACK);
    assert_eq!(&manifest, p.manifest());
    assert_eq!(
        Manifest::from_value(&manifest.to_value()).unwrap(),
        manifest
    );
}
