#![no_main]
//! N-Quads single-quad parser (`Quad::from_str`): never panics; an accepted quad
//! re-serializes to text that parses again to the identical canonical form.
use libfuzzer_sys::fuzz_target;
use std::str::FromStr;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(quad) = ledger_rdf::Quad::from_str(text) {
        let again =
            ledger_rdf::Quad::from_str(&quad.to_string()).expect("canonical quad text must parse");
        assert_eq!(
            again.to_string(),
            quad.to_string(),
            "canonical form must be a fixed point"
        );
    }
});
