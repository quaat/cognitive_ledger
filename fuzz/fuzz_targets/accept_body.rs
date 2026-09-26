#![no_main]
//! Accept request body and its request identity: never panics, deterministic digest.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(body) = serde_json::from_slice::<ledger_api::AcceptBody>(data) else {
        return;
    };
    let graph = ledger_core::GraphId::new("fuzz-graph").unwrap();
    let candidate: ledger_core::CommitId =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();
    if let Ok(canonical) = ledger_api::canonical_accept(&graph, &candidate, &body, "fuzz") {
        let a = canonical.canonical_bytes();
        assert_eq!(canonical.canonical_bytes(), a);
        assert_eq!(canonical.digest(), canonical.digest());
    }
});
