#![no_main]
//! Commit v1/v2 strict decoders: never panic; accepted bytes re-encode identically and
//! yield a stable id (content identity is a fixed point of decode∘encode).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(commit) = ledger_core::AnyCommit::from_canonical_bytes(data) {
        let bytes = commit.canonical_bytes().expect("decoded commit re-encodes");
        assert_eq!(bytes, data, "decoder accepted non-canonical bytes");
        let id = commit.id().expect("decoded commit has an id");
        let again = ledger_core::AnyCommit::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(again.id().unwrap(), id);
    }
});
