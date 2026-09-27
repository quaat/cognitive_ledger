//! Golden vectors for `sculpin-semantic-context/v1`, `sculpin-semantic-environment/v1`,
//! `sculpin-validation-record/v1` (ADR-0018) and `sculpin-validation-invocation/v1`
//! (ADR-0019 amendment). `.input` files are logical values (JSON),
//! `.hex` the canonical bytes and `.sha256` the identity, produced by the independent Python
//! reference `scripts/golden/validation_v1_reference.py`; the Rust encoders are verified
//! against them here and never rewrite them.

use ledger_core::ContentId;
use ledger_validation_protocol::{
    SemanticEnvironment, SemanticExecutionContext, ValidationInvocation, ValidationRecord,
};

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

const CONTEXTS: [&str; 5] = [
    "context-v1-basic",
    "context-v1-virtual-reordered",
    "context-v1-shacl-only",
    "context-v1-external-v42",
    "context-v1-other-candidate-same-environment",
];
const ENVIRONMENTS: [(&str, &str); 3] = [
    ("environment-v1-basic", "context-v1-basic"),
    ("environment-v1-shacl-only", "context-v1-shacl-only"),
    ("environment-v1-external-v42", "context-v1-external-v42"),
];
const RECORDS: [&str; 4] = [
    "record-v1-conforms",
    "record-v1-conforms-with-warnings",
    "record-v1-violations",
    "record-v1-violations-reordered",
];

fn context(stem: &str) -> SemanticExecutionContext {
    serde_json::from_str(&fixture(&format!("{stem}.input")))
        .unwrap_or_else(|e| panic!("{stem}.input: {e}"))
}

#[test]
fn context_vectors_encode_decode_and_hash_stably() {
    for stem in CONTEXTS {
        let context = context(stem);
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
fn environment_vectors_match_and_are_the_projection_of_their_contexts() {
    for (stem, from) in ENVIRONMENTS {
        let environment: SemanticEnvironment =
            serde_json::from_str(&fixture(&format!("{stem}.input"))).unwrap();
        let (bytes, id) = bytes_and_id(stem);
        assert_eq!(
            environment.canonical_bytes().unwrap(),
            bytes,
            "{stem}: bytes"
        );
        assert_eq!(environment.id().unwrap().to_string(), id, "{stem}: id");
        let decoded = SemanticEnvironment::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
        assert_eq!(
            context(from).environment_id().unwrap().to_string(),
            id,
            "{stem} is the environment of {from}"
        );
    }
    // Another candidate (other state, other object refs, other deployment service id) in the
    // same environment shares the environment id — what Sculpin publishes as current.
    let (_, basic_env) = bytes_and_id("environment-v1-basic");
    let other = context("context-v1-other-candidate-same-environment");
    assert_eq!(other.environment_id().unwrap().to_string(), basic_env);
    assert_ne!(
        other.id().unwrap().to_string(),
        bytes_and_id("context-v1-basic").1
    );
    let (_, external_env) = bytes_and_id("environment-v1-external-v42");
    assert_ne!(
        external_env, basic_env,
        "another source version is another environment"
    );
}

#[test]
fn virtual_context_order_and_duplicates_do_not_change_identity_but_versions_do() {
    let (basic, basic_id) = bytes_and_id("context-v1-basic");
    let (reordered, reordered_id) = bytes_and_id("context-v1-virtual-reordered");
    assert_eq!(basic, reordered);
    assert_eq!(basic_id, reordered_id);
    let (external, external_id) = bytes_and_id("context-v1-external-v42");
    assert_ne!(basic, external);
    assert_ne!(basic_id, external_id);
    let decoded = SemanticExecutionContext::from_canonical_bytes(&basic).unwrap();
    assert_eq!(
        decoded.virtual_contexts[0].dataset_id,
        "urn:sculpin:datasource:lab-a"
    );
    // Encoding order: the shorter reference first (length prefix), duplicates removed.
    assert_eq!(
        decoded.virtual_contexts[1].object_refs,
        vec![
            "s3://lab-b/run-9.parquet@v3",
            "s3://lab-b/run-10.parquet@v3"
        ]
    );
    let (shacl_only, _) = bytes_and_id("context-v1-shacl-only");
    let decoded = SemanticExecutionContext::from_canonical_bytes(&shacl_only).unwrap();
    assert!(decoded.ontology.is_none() && decoded.reasoning.is_none());
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
    assert!(
        decoded
            .outcome
            .violations
            .iter()
            .any(|v| v.message.contains("ünïcode"))
    );
    assert_eq!(
        decoded.recorded_at.canonical(),
        "2026-09-27T14:03:07.250000Z",
        "offset converted to UTC, fraction padded"
    );
    let (warnings, _) = bytes_and_id("record-v1-conforms-with-warnings");
    let warnings = ValidationRecord::from_canonical_bytes(&warnings).unwrap();
    assert!(warnings.outcome.is_conforming());
    assert_eq!(warnings.outcome.violations.len(), 1);
    let (conforms, _) = bytes_and_id("record-v1-conforms");
    let conforms = ValidationRecord::from_canonical_bytes(&conforms).unwrap();
    assert!(conforms.outcome.is_conforming());
    assert_eq!(conforms.report_reference, None);
}

/// Each negative vector must be refused for its specific reason (so a broken fixture cannot
/// pass by failing some other rule).
#[test]
fn negative_vectors_are_rejected_by_the_strict_decoders_for_the_right_reason() {
    let record_cases = [
        ("record-v1-invalid-unsorted-summary", "strictly ascending"),
        ("record-v1-invalid-duplicate-summary", "strictly ascending"),
        ("record-v1-invalid-noncanonical-time", "canonical"),
        ("record-v1-invalid-trailing-bytes", "trailing bytes"),
        (
            "record-v1-invalid-summary-exceeds-count",
            "exceed violation_count",
        ),
        (
            "record-v1-invalid-violations-without-count",
            "violation_count >= 1",
        ),
        ("record-v1-invalid-outcome-byte", "unknown outcome byte"),
    ];
    for (stem, reason) in record_cases {
        let bytes = hex::decode(fixture(&format!("{stem}.hex")).trim()).unwrap();
        let error = ValidationRecord::from_canonical_bytes(&bytes)
            .err()
            .unwrap_or_else(|| panic!("{stem} must be rejected"))
            .to_string();
        assert!(error.contains(reason), "{stem}: {error}");
    }
    let context_cases = [
        ("context-v1-invalid-trailing-bytes", "trailing bytes"),
        ("context-v1-invalid-unknown-version", "unknown header"),
        (
            "context-v1-invalid-unsorted-virtual-contexts",
            "strictly ascending",
        ),
        (
            "context-v1-invalid-duplicate-virtual-contexts",
            "strictly ascending",
        ),
        (
            "context-v1-invalid-unsorted-object-refs",
            "strictly ascending",
        ),
        ("context-v1-invalid-empty-ontology", "must not be empty"),
        ("context-v1-invalid-ontology-tag", "unknown ontology tag"),
        (
            "context-v1-invalid-hydrated-without-sources-revision",
            "requires sources_revision",
        ),
    ];
    for (stem, reason) in context_cases {
        let bytes = hex::decode(fixture(&format!("{stem}.hex")).trim()).unwrap();
        let error = SemanticExecutionContext::from_canonical_bytes(&bytes)
            .err()
            .unwrap_or_else(|| panic!("{stem} must be rejected"))
            .to_string();
        assert!(error.contains(reason), "{stem}: {error}");
    }
    let bytes = hex::decode(fixture("environment-v1-invalid-trailing-bytes.hex").trim()).unwrap();
    assert!(
        SemanticEnvironment::from_canonical_bytes(&bytes)
            .err()
            .unwrap()
            .to_string()
            .contains("trailing bytes")
    );
}

const INVOCATIONS: [&str; 3] = [
    "invocation-v1-basic",
    "invocation-v1-delegated",
    "invocation-v1-other-key",
];

#[test]
fn invocation_vectors_encode_and_hash_stably_and_differ_per_scope() {
    let mut ids = std::collections::HashSet::new();
    for stem in INVOCATIONS {
        let invocation: ValidationInvocation =
            serde_json::from_str(&fixture(&format!("{stem}.input")))
                .unwrap_or_else(|e| panic!("{stem}.input: {e}"));
        let (bytes, id) = bytes_and_id(stem);
        assert_eq!(
            invocation.canonical_bytes().unwrap(),
            bytes,
            "{stem}: bytes"
        );
        assert_eq!(invocation.id().unwrap().to_string(), id, "{stem}: id");
        assert!(
            ids.insert(id),
            "{stem}: distinct scopes share an invocation id"
        );
    }
}
