//! Semantic-validation coordination contracts (ADR-0014, ADR-0018, ADR-0019).
//!
//! This crate is infrastructure-free protocol: the versioned `SemanticExecutionContext`
//! and `ValidationRecord` with their frozen canonical encodings and content identities, the
//! logical validation invocation identity, the validator request/response wire types, and the `ValidationClient` boundary the ledger
//! calls a validation service through. It contains no SHACL, reasoning, ontology or
//! Virtual A-Box logic and no HTTP or database code: Sculpin owns semantic interpretation,
//! the ledger owns immutable candidates, validation provenance, acceptance policy,
//! decisions and history.
//!
//! Every identifier crossing this boundary is an opaque, bounded token
//! (`ledger_core::validate_token`); every digest is a strict `ContentId`; every timestamp a
//! canonical `LedgerTimestamp`. Decoders are strict and never reinterpret bytes.

mod client;
mod context;
mod encoding;
mod invocation;
mod record;

pub use client::{
    CandidateDescriptor, EffectiveContext, MAX_CANDIDATE_QUADS_INLINE, ReportReference,
    RequestedContext, VALIDATION_REQUEST_PROTOCOL, VALIDATION_RESPONSE_PROTOCOL, ValidationClient,
    ValidationClientError, ValidationRequest, ValidatorResponse, ValidatorVersions,
};
pub use context::{
    BaseKb, MAX_OBJECT_REFS, MAX_VIRTUAL_CONTEXTS, Ontology, Reasoning, SEMANTIC_CONTEXT_V1_HEADER,
    SEMANTIC_ENVIRONMENT_V1_HEADER, SemanticContextId, SemanticEnvironment, SemanticEnvironmentId,
    SemanticExecutionContext, ShapeSet, ValidatorIdentity, VirtualContextRef,
};
pub use invocation::{
    MAX_INVOCATION_KEY_BYTES, VALIDATION_INVOCATION_OPERATION, VALIDATION_INVOCATION_V1_HEADER,
    ValidationInvocation, ValidationInvocationId,
};
pub use record::{
    MAX_REPORT_REFERENCE_BYTES, MAX_SEVERITY_BYTES, MAX_VIOLATION_MESSAGE_BYTES,
    MAX_VIOLATION_SUMMARY, OutcomeKind, VALIDATION_RECORD_V1_HEADER, ValidationId,
    ValidationOutcome, ValidationRecord, ViolationSummary,
};

use ledger_core::LedgerError;
use thiserror::Error;

/// A structural violation of the validation protocol (bounds, strictness, identity).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("{0}")]
    Invalid(String),
}

impl From<LedgerError> for ProtocolError {
    fn from(error: LedgerError) -> Self {
        Self::Invalid(error.to_string())
    }
}

impl From<ProtocolError> for LedgerError {
    fn from(error: ProtocolError) -> Self {
        match error {
            ProtocolError::Invalid(reason) => LedgerError::InvalidValidation(reason),
        }
    }
}

/// Typed content identities of the two protocol objects. Transparent in serde (a plain
/// `sha256:` string); strict on parse like every `ContentId`.
macro_rules! typed_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub ledger_core::ContentId);
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
        impl std::str::FromStr for $name {
            type Err = ledger_core::LedgerError;
            fn from_str(v: &str) -> Result<Self, Self::Err> {
                Ok(Self(v.parse()?))
            }
        }
    };
}
pub(crate) use typed_id;
