#![no_main]
//! RFC 3339 timestamp normalization: never panics; canonical output is a fixed point and
//! is accepted by the strict canonical parser used by the commit decoder.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(ts) = ledger_core::LedgerTimestamp::parse_rfc3339(text) {
        let canonical = ts.canonical();
        assert_eq!(canonical.len(), ledger_core::LedgerTimestamp::CANONICAL_LEN);
        let strict = ledger_core::LedgerTimestamp::parse_canonical(&canonical)
            .expect("canonical form must satisfy the strict parser");
        assert_eq!(strict.canonical(), canonical);
    }
});
