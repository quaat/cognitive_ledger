//! Graph, tenant, and principal identity types (ADR-0010, ADR-0011).
//!
//! These are validated, opaque tokens. The ledger never parses their structure; it only
//! bounds and canonicalizes them because several are identity-bearing in commit v2
//! (ADR-0009). The caps below are frozen together with the v2 envelope.

use crate::LedgerError;
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// Maximum UTF-8 byte length of a principal, activity, evidence, or source-system token.
pub const MAX_IDENTIFIER_BYTES: usize = 512;
/// Maximum byte length of a `graph_id`.
pub const MAX_GRAPH_ID_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a commit message.
pub const MAX_MESSAGE_BYTES: usize = 4096;
/// Maximum number of distinct evidence references in one commit.
pub const MAX_EVIDENCE_REFS: usize = 64;

/// A generic identity-bearing token: non-empty, bounded, and free of control characters
/// (Unicode general category `Cc`).
pub(crate) fn validate_token(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), LedgerError> {
    if value.is_empty() {
        return Err(LedgerError::InvalidIdentifier {
            field,
            reason: "must not be empty".into(),
        });
    }
    if value.len() > max_bytes {
        return Err(LedgerError::InvalidIdentifier {
            field,
            reason: format!("exceeds {max_bytes} bytes"),
        });
    }
    if value.chars().any(char::is_control) {
        return Err(LedgerError::InvalidIdentifier {
            field,
            reason: "must not contain control characters".into(),
        });
    }
    Ok(())
}

/// `graph_id` is ledger-generated and opaque: ASCII `[A-Za-z0-9._:-]`, 1..=128 bytes.
/// UUIDs and URN-like tokens both fit; the ledger never derives meaning from it.
pub(crate) fn validate_graph_id(value: &str) -> Result<(), LedgerError> {
    validate_token("graph_id", value, MAX_GRAPH_ID_BYTES)?;
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
    {
        return Err(LedgerError::InvalidIdentifier {
            field: "graph_id",
            reason: "must match [A-Za-z0-9._:-]".into(),
        });
    }
    Ok(())
}

fn validate_tenant_id(value: &str) -> Result<(), LedgerError> {
    validate_token("tenant_id", value, MAX_IDENTIFIER_BYTES)
}

fn validate_principal_id(value: &str) -> Result<(), LedgerError> {
    validate_token("principal_id", value, MAX_IDENTIFIER_BYTES)
}

macro_rules! validated_token {
    ($(#[$meta:meta])* $name:ident, $validate:path) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, LedgerError> {
                let value = value.into();
                $validate(&value)?;
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
        impl FromStr for $name {
            type Err = LedgerError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
        impl TryFrom<String> for $name {
            type Error = LedgerError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

validated_token!(
    /// Stable, ledger-owned identity of a `LedgerGraph` (ADR-0010). Identity-bearing in
    /// commit v2; survives KB renames; many graphs may reference one Sculpin KB.
    GraphId,
    validate_graph_id
);
validated_token!(
    /// Tenant scope for every graph, ref, and authorization check (ADR-0010/0011).
    /// Deliberately *not* part of the commit envelope: a graph belongs to one tenant.
    TenantId,
    validate_tenant_id
);
validated_token!(
    /// Authenticated principal identity (ADR-0011); never taken from the request body.
    PrincipalId,
    validate_principal_id
);

/// Kind of verified identity; derived from the trust source, never caller-claimed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalType {
    Human,
    Agent,
    Service,
}

impl PrincipalType {
    /// Single-byte wire encoding used by the commit v2 canonical form.
    pub const fn wire_byte(self) -> u8 {
        match self {
            Self::Human => 0,
            Self::Agent => 1,
            Self::Service => 2,
        }
    }
    pub fn from_wire_byte(byte: u8) -> Result<Self, LedgerError> {
        match byte {
            0 => Ok(Self::Human),
            1 => Ok(Self::Agent),
            2 => Ok(Self::Service),
            other => Err(LedgerError::InvalidCommit(format!(
                "unknown principal_type byte {other}"
            ))),
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::Service => "service",
        }
    }
}

impl fmt::Display for PrincipalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The actor recorded in a commit v2 envelope (ADR-0009). Populated only from an
/// [`AuthenticatedPrincipal`]; the HTTP body can never set these fields (ADR-0011).
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub principal_id: PrincipalId,
    pub principal_type: PrincipalType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<PrincipalId>,
}

/// The result of authentication at the service boundary (ADR-0011). Everything the
/// commit envelope trusts about "who" comes from here.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct AuthenticatedPrincipal {
    pub principal_id: PrincipalId,
    pub principal_type: PrincipalType,
    pub tenant_id: TenantId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<PrincipalId>,
}

impl AuthenticatedPrincipal {
    /// Project the identity-bearing actor fields for a commit envelope.
    pub fn actor(&self) -> Actor {
        Actor {
            principal_id: self.principal_id.clone(),
            principal_type: self.principal_type,
            on_behalf_of: self.on_behalf_of.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_id_is_a_bounded_ascii_token() {
        assert!(GraphId::new("0b0a2a1c-2e3d-4f50-8a61-72b384c5d6e7").is_ok());
        assert!(GraphId::new("urn:sculpin:graph:team.alpha").is_ok());
        assert!(GraphId::new("").is_err());
        assert!(GraphId::new("has space").is_err());
        assert!(GraphId::new("unicode-é").is_err());
        assert!(GraphId::new("x".repeat(MAX_GRAPH_ID_BYTES)).is_ok());
        assert!(GraphId::new("x".repeat(MAX_GRAPH_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn principal_tokens_reject_empty_and_control_characters() {
        assert!(PrincipalId::new("urn:sculpin:agent:curator").is_ok());
        assert!(PrincipalId::new("").is_err());
        assert!(PrincipalId::new("line\nbreak").is_err());
        assert!(PrincipalId::new("del\u{7f}").is_err());
        assert!(PrincipalId::new("x".repeat(MAX_IDENTIFIER_BYTES + 1)).is_err());
        assert!(TenantId::new("tenant-1").is_ok());
    }

    #[test]
    fn serde_cannot_bypass_token_validation() {
        assert!(serde_json::from_str::<GraphId>("\"\"").is_err());
        assert!(serde_json::from_str::<PrincipalId>("\"ok\"").is_ok());
        let empty_on_behalf_of =
            r#"{"principal_id":"p","principal_type":"agent","on_behalf_of":""}"#;
        assert!(serde_json::from_str::<Actor>(empty_on_behalf_of).is_err());
        let absent = r#"{"principal_id":"p","principal_type":"agent"}"#;
        assert_eq!(
            serde_json::from_str::<Actor>(absent).unwrap().on_behalf_of,
            None
        );
    }

    #[test]
    fn principal_type_wire_bytes_round_trip_and_fail_closed() {
        for kind in [
            PrincipalType::Human,
            PrincipalType::Agent,
            PrincipalType::Service,
        ] {
            assert_eq!(
                PrincipalType::from_wire_byte(kind.wire_byte()).unwrap(),
                kind
            );
        }
        assert!(PrincipalType::from_wire_byte(3).is_err());
        assert_eq!(
            serde_json::to_string(&PrincipalType::Service).unwrap(),
            "\"service\""
        );
    }
}
