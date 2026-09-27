#![no_main]
//! Prepare request body: strict JSON deserialization (unknown fields refused) followed by the
//! handler's own normalization and request-identity computation. Never panics, and the
//! request identity is independent of JSON key order and whitespace and of the order of
//! `operations` and `evidence_refs` (the same canonical request must replay under one key).
use libfuzzer_sys::fuzz_target;

fn digest_of(bytes: &[u8]) -> Option<(ledger_core::ContentId, ledger_core::PatchId)> {
    let body = serde_json::from_slice::<ledger_api::PrepareBody>(bytes).ok()?;
    let graph = ledger_core::GraphId::new("fuzz-graph").unwrap();
    let limits = ledger_api::ApiLimits::default();
    let parsed = ledger_api::canonical_prepare(&graph, body, &limits, "fuzz").ok()?;
    Some((parsed.canonical.digest(), parsed.requested.id()))
}

fuzz_target!(|data: &[u8]| {
    let Some(original) = digest_of(data) else { return };
    // Re-serialize through a generic JSON value: keys sorted (serde_json's default map),
    // pretty-printed whitespace, operations and evidence reversed.
    let mut value: serde_json::Value = serde_json::from_slice(data).expect("already parsed once");
    if let Some(ops) = value.get_mut("operations").and_then(|v| v.as_array_mut()) {
        ops.reverse();
    }
    if let Some(ev) = value.get_mut("evidence_refs").and_then(|v| v.as_array_mut()) {
        ev.reverse();
    }
    let reshaped = serde_json::to_vec_pretty(&value).unwrap();
    let again = digest_of(&reshaped).expect("a reshaped valid body must stay valid");
    assert_eq!(again, original, "request identity depends on JSON shape or element order");
});
