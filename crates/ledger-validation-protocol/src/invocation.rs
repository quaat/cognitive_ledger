//! `sculpin-validation-invocation/v1` (ADR-0019 amendment): the identity of one *logical*
//! outbound validation. Every physical delivery of the same ledger `validate` request — a
//! concurrent duplicate under the same `Idempotency-Key`, or a retry after a lost response or
//! a crash between the validator's answer and the ledger's record — carries the same
//! invocation id, so the validation service can resolve them to one logical validation and
//! one effective semantic environment (at-least-once delivery, exactly-once logical identity).
//!
//! It is an invocation/idempotency identity, not semantic identity: it never enters a
//! `CommitId`, `SemanticContextId`, `SemanticEnvironmentId` or `ValidationId`, is never stored,
//! and is never decoded — the validator treats it as an opaque token. It is derived only from
//! the authenticated idempotency scope and the canonical request digest; correlation ids,
//! clocks, arrival order and server instance never reach it.
//!
//! ```text
//! "sculpin-validation-invocation-v1\0"
//! field tenant_id · u8 principal_type (commit-v2 wire byte) · field principal_id
//! opt   on_behalf_of · field graph_id · field operation ("validate")
//! field idempotency_key (1..=256 bytes, no control characters) · field request_digest
//! ```
//!
//! The id is `sha256:` + hex SHA-256 of those bytes. The request digest is the
//! `sculpin-ledger-request/v2` digest of the validate request (graph, candidate, hints), so
//! the same key with another body yields another invocation — and `IDEMPOTENCY_CONFLICT` in
//! the ledger.

use crate::{
    ProtocolError,
    encoding::{field, opt},
    typed_id,
};
use ledger_core::{
    AuthenticatedPrincipal, ContentId, GraphId, PrincipalId, PrincipalType, TenantId,
};
use serde::{Deserialize, Serialize};

pub const VALIDATION_INVOCATION_V1_HEADER: &[u8] = b"sculpin-validation-invocation-v1\0";
/// The only operation an invocation names (domain separation beside the header).
pub const VALIDATION_INVOCATION_OPERATION: &str = "validate";
/// Same bound as the ledger's `Idempotency-Key`.
pub const MAX_INVOCATION_KEY_BYTES: usize = 256;

typed_id!(
    /// Opaque identity of one logical outbound validation (sent as `invocation_id` and as
    /// the `Idempotency-Key` header of the validator call).
    ValidationInvocationId
);

/// The fields an invocation id is derived from: the authenticated idempotency scope of a
/// ledger `validate` request and its canonical request digest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationInvocation {
    pub tenant_id: TenantId,
    pub principal_type: PrincipalType,
    pub principal_id: PrincipalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<PrincipalId>,
    pub graph_id: GraphId,
    pub idempotency_key: String,
    pub request_digest: ContentId,
}

impl ValidationInvocation {
    /// The invocation of a `validate` request by `principal` on `graph` under `key`.
    pub fn for_request(
        principal: &AuthenticatedPrincipal,
        graph: &GraphId,
        idempotency_key: &str,
        request_digest: &ContentId,
    ) -> Self {
        Self {
            tenant_id: principal.tenant_id.clone(),
            principal_type: principal.principal_type,
            principal_id: principal.principal_id.clone(),
            on_behalf_of: principal.on_behalf_of.clone(),
            graph_id: graph.clone(),
            idempotency_key: idempotency_key.to_owned(),
            request_digest: request_digest.clone(),
        }
    }

    fn validate(&self) -> Result<(), ProtocolError> {
        let key = &self.idempotency_key;
        if key.is_empty()
            || key.len() > MAX_INVOCATION_KEY_BYTES
            || key.chars().any(char::is_control)
        {
            return Err(ProtocolError::Invalid(format!(
                "validation invocation: idempotency key must be 1..={MAX_INVOCATION_KEY_BYTES} \
                 bytes without control characters"
            )));
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        let mut out = VALIDATION_INVOCATION_V1_HEADER.to_vec();
        field(&mut out, self.tenant_id.as_str())?;
        out.push(self.principal_type.wire_byte());
        field(&mut out, self.principal_id.as_str())?;
        opt(
            &mut out,
            self.on_behalf_of.as_ref().map(PrincipalId::as_str),
        )?;
        field(&mut out, self.graph_id.as_str())?;
        field(&mut out, VALIDATION_INVOCATION_OPERATION)?;
        field(&mut out, &self.idempotency_key)?;
        field(&mut out, &self.request_digest.to_string())?;
        Ok(out)
    }

    pub fn id(&self) -> Result<ValidationInvocationId, ProtocolError> {
        Ok(ValidationInvocationId(ContentId::for_bytes(
            &self.canonical_bytes()?,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ValidationInvocation {
        ValidationInvocation {
            tenant_id: TenantId::new("tenant-a").unwrap(),
            principal_type: PrincipalType::Service,
            principal_id: PrincipalId::new("orchestrator").unwrap(),
            on_behalf_of: None,
            graph_id: GraphId::new("materials").unwrap(),
            idempotency_key: "validate-1".into(),
            request_digest: ContentId::for_bytes(b"request"),
        }
    }

    #[test]
    fn every_scope_field_and_the_request_digest_separate_invocations() {
        let id = base().id().unwrap();
        assert_eq!(base().id().unwrap(), id, "deterministic");
        type Change = Box<dyn Fn(&mut ValidationInvocation)>;
        let variants: Vec<Change> = vec![
            Box::new(|i| i.tenant_id = TenantId::new("tenant-b").unwrap()),
            Box::new(|i| i.principal_type = PrincipalType::Agent),
            Box::new(|i| i.principal_id = PrincipalId::new("other").unwrap()),
            Box::new(|i| i.on_behalf_of = Some(PrincipalId::new("orchestrator").unwrap())),
            Box::new(|i| i.graph_id = GraphId::new("materials-2").unwrap()),
            Box::new(|i| i.idempotency_key = "validate-2".into()),
            Box::new(|i| i.request_digest = ContentId::for_bytes(b"other request")),
        ];
        let mut seen = std::collections::HashSet::from([id]);
        for change in variants {
            let mut changed = base();
            change(&mut changed);
            assert!(seen.insert(changed.id().unwrap()), "{changed:?}");
        }
    }

    #[test]
    fn keys_are_bounded_like_the_ledger_idempotency_key() {
        for bad in [
            String::new(),
            "k\n".into(),
            "k".repeat(MAX_INVOCATION_KEY_BYTES + 1),
        ] {
            let mut i = base();
            i.idempotency_key = bad;
            assert!(i.id().is_err());
        }
        let mut i = base();
        i.idempotency_key = "k".repeat(MAX_INVOCATION_KEY_BYTES);
        assert!(i.id().is_ok());
    }
}
