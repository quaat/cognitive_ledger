#![no_main]
//! Accept request body and its request identity: never panics; the identity is independent
//! of JSON key order and whitespace.
use libfuzzer_sys::fuzz_target;

fn digest_of(bytes: &[u8]) -> Option<ledger_core::ContentId> {
    let body = serde_json::from_slice::<ledger_api::AcceptBody>(bytes).ok()?;
    let graph = ledger_core::GraphId::new("fuzz-graph").unwrap();
    let candidate: ledger_core::CommitId =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();
    ledger_api::canonical_accept(&graph, &candidate, &body, "fuzz")
        .ok()
        .map(|c| c.digest())
}

fuzz_target!(|data: &[u8]| {
    let Some(original) = digest_of(data) else { return };
    let value: serde_json::Value = serde_json::from_slice(data).expect("already parsed once");
    let reshaped = serde_json::to_vec_pretty(&value).unwrap();
    let again = digest_of(&reshaped).expect("a reshaped valid body must stay valid");
    assert_eq!(again, original, "request identity depends on JSON shape");
});
