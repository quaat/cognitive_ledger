#![no_main]
//! Phase 2 strict decoders (ADR-0018): semantic context, semantic environment and validation
//! record never panic; accepted bytes re-encode identically (content identity is a fixed
//! point of decode∘encode), and a context's environment is always encodable.
use ledger_validation_protocol::{SemanticEnvironment, SemanticExecutionContext, ValidationRecord};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(context) = SemanticExecutionContext::from_canonical_bytes(data) {
        let bytes = context.canonical_bytes().expect("decoded context re-encodes");
        assert_eq!(bytes, data, "context decoder accepted non-canonical bytes");
        let environment = context.environment();
        let env_bytes = environment.canonical_bytes().expect("environment encodes");
        let again = SemanticEnvironment::from_canonical_bytes(&env_bytes).unwrap();
        assert_eq!(again.canonical_bytes().unwrap(), env_bytes);
    }
    if let Ok(environment) = SemanticEnvironment::from_canonical_bytes(data) {
        assert_eq!(environment.canonical_bytes().unwrap(), data);
    }
    if let Ok(record) = ValidationRecord::from_canonical_bytes(data) {
        assert_eq!(record.canonical_bytes().unwrap(), data);
        assert_eq!(record.id().unwrap(), ValidationRecord::from_canonical_bytes(data).unwrap().id().unwrap());
    }
});
