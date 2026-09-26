#![no_main]
//! Prepare request body: JSON deserialization (strict, unknown fields refused) followed by
//! the handler's own normalization and request-identity computation. Never panics; the
//! canonical request bytes and digest are deterministic.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(body) = serde_json::from_slice::<ledger_api::PrepareBody>(data) else {
        return;
    };
    let graph = ledger_core::GraphId::new("fuzz-graph").unwrap();
    let limits = ledger_api::ApiLimits::default();
    if let Ok(parsed) = ledger_api::canonical_prepare(&graph, body, &limits, "fuzz") {
        let a = parsed.canonical.canonical_bytes();
        let d = parsed.canonical.digest();
        assert_eq!(
            parsed.canonical.canonical_bytes(),
            a,
            "canonical bytes must be deterministic"
        );
        assert_eq!(parsed.canonical.digest(), d);
        // The requested patch is canonical by construction.
        let again = ledger_rdf::Patch::from_canonical_bytes(&parsed.requested.canonical_bytes())
            .expect("normalized patch is canonical");
        assert_eq!(again.id(), parsed.requested.id());
    }
});
