//! Golden vectors for `sculpin-semantic-context/v1` and `sculpin-validation-record/v1`
//! (ADR-0018). `.input` files are logical values (JSON), `.hex` the canonical bytes and
//! `.sha256` the identity, produced by the independent Python reference
//! `scripts/golden/validation_v1_reference.py`; the Rust encoders are verified against them
//! here and never rewrite them.

use ledger_core::ContentId;
use ledger_validation_protocol::{SemanticExecutionContext, ValidationRecord};

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../../fixtures/golden/validation/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn bytes_and_id(stem: &str) -> (Vec<u8>, String) {
    (
        hex::decode(fixture(&format!("{stem}.hex")).trim()).expect("golden hex"),
        fixture(&format!("{stem}.sha256")).trim().to_owned(),
    )
}

const CONTEXTS: [&str; 4] = [
    "context-v1-basic",
    "context-v1-virtual-reordered",
    "context-v1-no-ontology",
    "context-v1-external-v42",
];
const RECORDS: [&str; 3] = [
    "record-v1-conforms",
    "record-v1-violations",
    "record-v1-violations-reordered",
];

#[test]
fn context_vectors_encode_decode_and_hash_stably() {
    for stem in CONTEXTS {
        let context: SemanticExecutionContext =
            serde_json::from_str(&fixture(&format!("{stem}.input")))
                .unwrap_or_else(|e| panic!("{stem}.input: {e}"));
        let (bytes, id) = bytes_and_id(stem);
        assert_eq!(context.canonical_bytes().unwrap(), bytes, "{stem}: bytes");
        assert_eq!(context.id().unwrap().to_string(), id, "{stem}: id");
        assert_eq!(ContentId::for_bytes(&bytes).to_string(), id);
        let decoded = SemanticExecutionContext::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(
            decoded.canonical_bytes().unwrap(),
            bytes,
            "{stem}: decode exact"
        );
    }
}

#[test]
fn virtual_context_order_and_duplicates_do_not_change_identity_but_versions_do() {
    let (basic, basic_id) = bytes_and_id("context-v1-basic");
    let (reordered, reordered_id) = bytes_and_id("context-v1-virtual-reordered");
    assert_eq!(basic, reordered);
    assert_eq!(basic_id, reordered_id);
    let (external, external_id) = bytes_and_id("context-v1-external-v42");
    assert_ne!(basic, external);
    assert_ne!(
        basic_id, external_id,
        "another external source version is another context"
    );
    let decoded = SemanticExecutionContext::from_canonical_bytes(&basic).unwrap();
    assert_eq!(
        decoded.virtual_contexts[0].dataset_id,
        "urn:sculpin:datasource:lab-a"
    );
    assert_eq!(
        decoded.virtual_contexts[1].object_refs,
        vec!["s3://lab-b/run-1.parquet@v3", "s3://lab-b/run-2.parquet@v3"]
    );
    let (no_ontology, _) = bytes_and_id("context-v1-no-ontology");
    assert!(
        SemanticExecutionContext::from_canonical_bytes(&no_ontology)
            .unwrap()
            .ontology
            .is_none()
    );
}

#[test]
fn record_vectors_encode_decode_and_hash_stably() {
    for stem in RECORDS {
        let record: ValidationRecord = serde_json::from_str(&fixture(&format!("{stem}.input")))
            .unwrap_or_else(|e| panic!("{stem}.input: {e}"));
        let (bytes, id) = bytes_and_id(stem);
        assert_eq!(record.canonical_bytes().unwrap(), bytes, "{stem}: bytes");
        assert_eq!(record.id().unwrap().to_string(), id, "{stem}: id");
        let decoded = ValidationRecord::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(
            decoded.canonical_bytes().unwrap(),
            bytes,
            "{stem}: decode exact"
        );
    }
    let (violations, violations_id) = bytes_and_id("record-v1-violations");
    let (reordered, reordered_id) = bytes_and_id("record-v1-violations-reordered");
    assert_eq!(violations, reordered);
    assert_eq!(violations_id, reordered_id);
    let decoded = ValidationRecord::from_canonical_bytes(&violations).unwrap();
    assert_eq!(decoded.outcome.violation_count, 3);
    assert_eq!(decoded.outcome.violations.len(), 2);
    assert_eq!(
        decoded.recorded_at.canonical(),
        "2026-09-27T14:03:07.250000Z",
        "offset converted to UTC, fraction padded"
    );
    let (conforms, _) = bytes_and_id("record-v1-conforms");
    let conforms = ValidationRecord::from_canonical_bytes(&conforms).unwrap();
    assert!(conforms.outcome.is_conforming());
    assert_eq!(conforms.report_reference, None);
}

#[test]
fn negative_vectors_are_rejected_by_the_strict_decoders() {
    for stem in [
        "record-v1-invalid-unsorted-summary",
        "record-v1-invalid-duplicate-summary",
        "record-v1-invalid-noncanonical-time",
        "record-v1-invalid-trailing-bytes",
        "record-v1-invalid-conforms-with-violations",
    ] {
        let bytes = hex::decode(fixture(&format!("{stem}.hex")).trim()).unwrap();
        assert!(
            ValidationRecord::from_canonical_bytes(&bytes).is_err(),
            "{stem} must be rejected"
        );
    }
    for stem in [
        "context-v1-invalid-trailing-bytes",
        "context-v1-invalid-unknown-version",
        "context-v1-invalid-unsorted-virtual-contexts",
        "context-v1-invalid-empty-ontology",
    ] {
        let bytes = hex::decode(fixture(&format!("{stem}.hex")).trim()).unwrap();
        assert!(
            SemanticExecutionContext::from_canonical_bytes(&bytes).is_err(),
            "{stem} must be rejected"
        );
    }
}
