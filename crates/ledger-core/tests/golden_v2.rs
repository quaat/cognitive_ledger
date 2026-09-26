//! Golden vectors for `sculpin-cognitive-commit/v2` (ADR-0009). The `.input` files are
//! logical commits (JSON), the `.hex` files are the canonical bytes, and `.sha256` the
//! `CommitId`. They were produced by the independent Python reference encoder
//! (`scripts/golden/commit_v2_reference.py`) and are verified here by the Rust encoder;
//! tests never rewrite them.

use ledger_core::{AnyCommit, Commit, CommitV2, ContentId, LedgerError, LedgerTimestamp};

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../../fixtures/golden/commits/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn vector(stem: &str) -> (CommitV2, Vec<u8>, String) {
    let commit: CommitV2 = serde_json::from_str(&fixture(&format!("{stem}.input")))
        .unwrap_or_else(|e| panic!("{stem}.input is a valid logical commit: {e}"));
    let bytes = hex::decode(fixture(&format!("{stem}.hex")).trim()).expect("golden hex");
    let id = fixture(&format!("{stem}.sha256")).trim().to_owned();
    (commit, bytes, id)
}

const POSITIVE: [&str; 5] = [
    "v2-genesis",
    "v2-linear",
    "v2-evidence-reordered",
    "v2-merge",
    "v2-normalized-time",
];

#[test]
fn v2_vectors_encode_decode_and_hash_stably() {
    for stem in POSITIVE {
        let (commit, bytes, id) = vector(stem);
        assert_eq!(
            commit.canonical_bytes().unwrap(),
            bytes,
            "{stem}: canonical bytes"
        );
        assert_eq!(commit.id().unwrap().to_string(), id, "{stem}: commit id");
        assert_eq!(
            ContentId::for_bytes(&bytes).to_string(),
            id,
            "{stem}: id is sha256 of bytes"
        );
        let decoded = CommitV2::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(
            decoded.canonical_bytes().unwrap(),
            bytes,
            "{stem}: decode is exact"
        );
        match AnyCommit::from_canonical_bytes(&bytes).unwrap() {
            AnyCommit::V2(any) => assert_eq!(any, decoded, "{stem}: dual read yields v2"),
            AnyCommit::V1(_) => panic!("{stem}: v2 bytes read as v1"),
        }
    }
}

#[test]
fn evidence_order_and_duplicates_do_not_change_identity() {
    let (linear, linear_bytes, linear_id) = vector("v2-linear");
    let (reordered, reordered_bytes, reordered_id) = vector("v2-evidence-reordered");
    assert_ne!(
        linear.evidence_refs, reordered.evidence_refs,
        "inputs differ in order/dups"
    );
    assert_eq!(linear_bytes, reordered_bytes);
    assert_eq!(linear_id, reordered_id);
    assert_eq!(
        CommitV2::from_canonical_bytes(&linear_bytes)
            .unwrap()
            .evidence_refs,
        vec![
            "https://example.org/evidence/report-17",
            "urn:sculpin:evidence:feedback:0042",
            "urn:sculpin:evidence:feedback:9001",
        ]
    );
}

#[test]
fn parent_order_is_pinned_by_the_merge_vector() {
    let (merge, bytes, _) = vector("v2-merge");
    assert_eq!(merge.parents.len(), 2);
    assert_eq!(
        merge.event_time, None,
        "merge vector pins an absent event_time"
    );
    let mut reversed = merge.clone();
    reversed.parents.reverse();
    assert_ne!(reversed.canonical_bytes().unwrap(), bytes);
}

#[test]
fn timestamps_are_normalized_before_hashing() {
    let (commit, _, _) = vector("v2-normalized-time");
    assert_eq!(
        commit.event_time.unwrap().canonical(),
        "2026-09-24T10:30:15.123456Z",
        "offset converted to UTC, fraction truncated to microseconds"
    );
    assert_eq!(
        commit.recorded_at.canonical(),
        "2026-09-24T10:30:15.999999Z"
    );
    assert_eq!(
        commit.message, "",
        "an empty message is valid (it is not an optional field)"
    );
    let (genesis, _, _) = vector("v2-genesis");
    assert_eq!(
        genesis.event_time,
        Some(LedgerTimestamp::parse_rfc3339("2026-09-24T10:00:00Z").unwrap())
    );
}

#[test]
fn absent_optionals_are_encoded_distinctly_from_present_ones() {
    let (genesis, genesis_bytes, _) = vector("v2-genesis");
    assert_eq!(genesis.actor.on_behalf_of, None);
    assert_eq!(genesis.source_system, None);
    let (linear, _, _) = vector("v2-linear");
    assert!(linear.actor.on_behalf_of.is_some());
    assert!(linear.source_system.is_some());
    // A genesis with the linear commit's optionals filled in must not collide.
    let mut filled = genesis.clone();
    filled.actor.on_behalf_of = linear.actor.on_behalf_of.clone();
    filled.source_system = linear.source_system.clone();
    assert_ne!(filled.canonical_bytes().unwrap(), genesis_bytes);
}

#[test]
fn v2_rejects_empty_optional_values_in_logical_input() {
    let linear = fixture("v2-linear.input");
    let empty_delegate = linear.replace("\"urn:sculpin:human:reviewer-42\"", "\"\"");
    assert!(serde_json::from_str::<CommitV2>(&empty_delegate).is_err());
    let empty_source = linear.replace("\"sculpin-agent-api\"", "\"\"");
    assert!(serde_json::from_str::<CommitV2>(&empty_source).is_err());
    let correlation = linear.replace("\"message\"", "\"correlation_id\": \"c-1\",\n  \"message\"");
    assert!(serde_json::from_str::<CommitV2>(&correlation).is_err());
}

/// Which decoder rule a negative vector must trip, so a broken fixture cannot pass for the
/// wrong reason.
enum Expect {
    Commit,
    Timestamp,
}

#[test]
fn negative_vectors_are_rejected_by_the_strict_decoder() {
    let cases = [
        ("v2-invalid-unsorted-evidence", Expect::Commit),
        ("v2-invalid-duplicate-evidence", Expect::Commit),
        ("v2-invalid-too-many-evidence", Expect::Commit),
        ("v2-invalid-empty-on-behalf-of", Expect::Commit),
        ("v2-invalid-empty-event-time", Expect::Commit),
        ("v2-invalid-empty-source-system", Expect::Commit),
        ("v2-invalid-noncanonical-time", Expect::Timestamp),
        ("v2-invalid-noncanonical-event-time", Expect::Timestamp),
        ("v2-invalid-principal-type", Expect::Commit),
        ("v2-invalid-duplicate-parents", Expect::Commit),
        ("v2-invalid-three-parents", Expect::Commit),
        ("v2-invalid-trailing-bytes", Expect::Commit),
    ];
    for (stem, expect) in cases {
        let bytes = hex::decode(fixture(&format!("{stem}.hex")).trim()).expect("hex");
        let error = CommitV2::from_canonical_bytes(&bytes)
            .err()
            .unwrap_or_else(|| panic!("{stem} must be rejected"));
        let kind_matches = match expect {
            Expect::Commit => matches!(error, LedgerError::InvalidCommit(_)),
            Expect::Timestamp => matches!(error, LedgerError::InvalidTimestamp(_)),
        };
        assert!(kind_matches, "{stem}: unexpected error {error}");
        assert!(
            AnyCommit::from_canonical_bytes(&bytes).is_err(),
            "{stem} via dual read"
        );
    }
}

#[test]
fn unknown_envelope_versions_fail_closed() {
    let bytes = hex::decode(fixture("v2-invalid-unknown-version.hex").trim()).expect("hex");
    assert!(matches!(
        AnyCommit::from_canonical_bytes(&bytes),
        Err(LedgerError::UnknownCommitVersion(_))
    ));
}

#[test]
fn v1_vectors_remain_readable_through_dual_read() {
    for (stem, parents) in [("basic-v1", 0), ("linear-v1", 1), ("merge-v1", 2)] {
        let bytes = hex::decode(fixture(&format!("{stem}.hex")).trim()).expect("hex");
        let expected = fixture(&format!("{stem}.sha256")).trim().to_owned();
        let any = AnyCommit::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(any.parents().len(), parents);
        assert_eq!(any.id().unwrap().to_string(), expected);
        assert_eq!(any.canonical_bytes().unwrap(), bytes);
        assert!(matches!(any, AnyCommit::V1(_)));
        let direct = Commit::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(direct.id().unwrap().to_string(), expected);
    }
    // v2 commits may descend from v1 commits: the linear vector's parent is basic-v1.
    let (linear, _, _) = vector("v2-linear");
    assert_eq!(
        linear.parents[0].to_string(),
        fixture("basic-v1.sha256").trim()
    );
}
