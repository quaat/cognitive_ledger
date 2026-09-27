#![no_main]
//! Phase 2 strict decoders (ADR-0018): semantic context, semantic environment and validation
//! record never panic; accepted bytes re-encode identically (content identity is a fixed
//! point of decode∘encode), and a context's environment is always encodable. The validator's
//! JSON response (the network-facing untrusted input) never panics through strict decoding,
//! summary normalization and conversion into a context, and every context it yields is
//! canonical and decodes back to itself.
use ledger_core::GraphId;
use ledger_validation_protocol::{
    RequestedContext, SemanticEnvironment, SemanticExecutionContext, ValidationRecord,
    ValidatorResponse,
};
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
    if let Ok(response) = serde_json::from_slice::<ValidatorResponse>(data) {
        let response = response.with_bounded_summary();
        assert!(response.outcome.violations.len() <= ledger_validation_protocol::MAX_VIOLATION_SUMMARY);
        let graph = GraphId::new("fuzz-graph").unwrap();
        if let Ok(context) = response.into_context(
            &graph,
            &response.candidate_commit,
            &response.candidate_state_digest,
            "urn:fuzz:validator",
            &RequestedContext::default(),
        ) {
            // Bound to what the ledger asked about and to the ledger's service identity.
            assert_eq!(context.graph_id, graph);
            assert_eq!(context.candidate_commit, response.candidate_commit);
            assert_eq!(context.candidate_state_digest, response.candidate_state_digest);
            assert_eq!(context.validator.service_id, "urn:fuzz:validator");
            let bytes = context.canonical_bytes().expect("a converted context encodes");
            let decoded = SemanticExecutionContext::from_canonical_bytes(&bytes).unwrap();
            assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
            assert_eq!(decoded.id().unwrap(), context.id().unwrap());
            // An answer about another candidate or state is never accepted.
            let other = ledger_core::ContentId::for_bytes(&bytes);
            assert!(response
                .into_context(
                    &graph,
                    &ledger_core::CommitId(other.clone()),
                    &response.candidate_state_digest,
                    "urn:fuzz:validator",
                    &RequestedContext::default(),
                )
                .is_err());
            assert!(response
                .into_context(
                    &graph,
                    &response.candidate_commit,
                    &other,
                    "urn:fuzz:validator",
                    &RequestedContext::default(),
                )
                .is_err());
        }
    }
    if let Ok(record) = ValidationRecord::from_canonical_bytes(data) {
        assert_eq!(record.canonical_bytes().unwrap(), data);
        assert_eq!(record.id().unwrap(), ValidationRecord::from_canonical_bytes(data).unwrap().id().unwrap());
    }
});
