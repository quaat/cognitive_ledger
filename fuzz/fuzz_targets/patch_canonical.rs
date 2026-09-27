#![no_main]
//! Patch canonical decoder: never panics; accepted bytes are exactly the canonical
//! re-encoding (so two different byte strings can never decode to one patch identity).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(patch) = ledger_rdf::Patch::from_canonical_bytes(data) {
        assert_eq!(
            patch.canonical_bytes(),
            data,
            "decoder accepted non-canonical bytes"
        );
        let id = patch.id();
        let again = ledger_rdf::Patch::from_canonical_bytes(&patch.canonical_bytes()).unwrap();
        assert_eq!(again.id(), id);
    }
});
