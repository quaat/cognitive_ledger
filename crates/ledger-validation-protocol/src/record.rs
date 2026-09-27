//! `sculpin-validation-record/v1` (ADR-0018): the immutable outcome of one validation of
//! one candidate under one semantic execution context. Never part of hashed commit bytes;
//! a candidate may carry any number of records.
//!
//! ```text
//! "sculpin-validation-record-v1\0"
//! field graph_id · field candidate_commit · field candidate_state_digest
//! field semantic_execution_context_id
//! field validator.service_id · field validator.service_version · field validator.configuration_version
//! u8    outcome (0 conforms | 1 violations) · u32 violation_count
//! u32   summary_count (<= 64, <= violation_count), entries bytewise ascending + unique:
//!         field severity (<= 64 bytes) · field code · field message (<= 1024 bytes, may be empty)
//! field recorded_at (canonical, server-assigned) · field report_digest · opt report_reference
//! ```

use crate::{
    ProtocolError, SemanticContextId, ValidatorIdentity,
    encoding::{Cursor, canonical_set, field, opt, u32be},
    typed_id,
};
use ledger_core::{
    CommitId, ContentId, GraphId, LedgerTimestamp, MAX_IDENTIFIER_BYTES, validate_token,
};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

pub const VALIDATION_RECORD_V1_HEADER: &[u8] = b"sculpin-validation-record-v1\0";
/// Maximum number of summarized violations stored in a record (the full report lives
/// behind `report_digest`/`report_reference`).
pub const MAX_VIOLATION_SUMMARY: usize = 64;
pub const MAX_SEVERITY_BYTES: usize = 64;
pub const MAX_VIOLATION_MESSAGE_BYTES: usize = 1024;
pub const MAX_REPORT_REFERENCE_BYTES: usize = 2048;

typed_id!(
    /// Content identity of a canonical `ValidationRecord`.
    ValidationId
);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeKind {
    Conforms,
    Violations,
}

impl OutcomeKind {
    const fn wire_byte(self) -> u8 {
        match self {
            Self::Conforms => 0,
            Self::Violations => 1,
        }
    }
    fn from_wire_byte(byte: u8) -> Result<Self, ProtocolError> {
        match byte {
            0 => Ok(Self::Conforms),
            1 => Ok(Self::Violations),
            other => Err(ProtocolError::Invalid(format!(
                "validation record: unknown outcome byte {other}"
            ))),
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Conforms => "conforms",
            Self::Violations => "violations",
        }
    }
}

/// One summarized violation. The ledger stores and bounds these; it never evaluates them.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViolationSummary {
    pub severity: String,
    pub code: String,
    #[serde(default)]
    pub message: String,
}

impl ViolationSummary {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_token("violation.severity", &self.severity, MAX_SEVERITY_BYTES)?;
        validate_token("violation.code", &self.code, MAX_IDENTIFIER_BYTES)?;
        if self.message.len() > MAX_VIOLATION_MESSAGE_BYTES {
            return Err(ProtocolError::Invalid(format!(
                "violation.message exceeds {MAX_VIOLATION_MESSAGE_BYTES} bytes"
            )));
        }
        if self.message.chars().any(char::is_control) {
            return Err(ProtocolError::Invalid(
                "violation.message must not contain control characters".into(),
            ));
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        let mut out = Vec::new();
        field(&mut out, &self.severity)?;
        field(&mut out, &self.code)?;
        field(&mut out, &self.message)?;
        Ok(out)
    }

    fn decode(cursor: &mut Cursor<'_>) -> Result<Self, ProtocolError> {
        let value = Self {
            severity: cursor.field("violation.severity")?,
            code: cursor.field("violation.code")?,
            message: cursor.field("violation.message")?,
        };
        value.validate()?;
        Ok(value)
    }
}

/// `conforms` carries no violations; `violations` carries the validator's total count and
/// a bounded summary set (`violations.len() <= violation_count`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationOutcome {
    pub kind: OutcomeKind,
    #[serde(default)]
    pub violation_count: u32,
    #[serde(default)]
    pub violations: Vec<ViolationSummary>,
}

impl ValidationOutcome {
    pub fn conforms() -> Self {
        Self {
            kind: OutcomeKind::Conforms,
            violation_count: 0,
            violations: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        for violation in &self.violations {
            violation.validate()?;
        }
        let summary = self.canonical_violations()?;
        match self.kind {
            OutcomeKind::Conforms => {
                if self.violation_count != 0 || !summary.is_empty() {
                    return Err(ProtocolError::Invalid(
                        "a conforming outcome carries no violations".into(),
                    ));
                }
            }
            OutcomeKind::Violations => {
                if self.violation_count == 0 {
                    return Err(ProtocolError::Invalid(
                        "a violations outcome needs violation_count >= 1".into(),
                    ));
                }
            }
        }
        if summary.len() > MAX_VIOLATION_SUMMARY {
            return Err(ProtocolError::Invalid(format!(
                "more than {MAX_VIOLATION_SUMMARY} distinct summarized violations"
            )));
        }
        if summary.len() > self.violation_count as usize {
            return Err(ProtocolError::Invalid(
                "summarized violations exceed violation_count".into(),
            ));
        }
        Ok(())
    }

    pub fn canonical_violations(&self) -> Result<Vec<Vec<u8>>, ProtocolError> {
        let encoded = self
            .violations
            .iter()
            .map(ViolationSummary::canonical_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(canonical_set(encoded))
    }

    pub fn is_conforming(&self) -> bool {
        self.kind == OutcomeKind::Conforms
    }
}

/// `Eq` is structural (caller order of the summary); identity is `id()`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ValidationRecord {
    pub graph_id: GraphId,
    pub candidate_commit: CommitId,
    pub candidate_state_digest: ContentId,
    pub semantic_execution_context_id: SemanticContextId,
    pub validator: ValidatorIdentity,
    pub outcome: ValidationOutcome,
    /// Server-assigned when the ledger records the validator's response.
    pub recorded_at: LedgerTimestamp,
    /// Digest of the validator's complete report (stored elsewhere, never inline).
    pub report_digest: ContentId,
    /// Immutable reference to the report (opaque, bounded), if the validator offers one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_reference: Option<String>,
}

impl ValidationRecord {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.validator.validate()?;
        self.outcome.validate()?;
        if let Some(reference) = &self.report_reference {
            validate_token("report_reference", reference, MAX_REPORT_REFERENCE_BYTES)?;
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        let mut out = VALIDATION_RECORD_V1_HEADER.to_vec();
        field(&mut out, self.graph_id.as_str())?;
        field(&mut out, &self.candidate_commit.to_string())?;
        field(&mut out, &self.candidate_state_digest.to_string())?;
        field(&mut out, &self.semantic_execution_context_id.to_string())?;
        field(&mut out, &self.validator.service_id)?;
        field(&mut out, &self.validator.service_version)?;
        field(&mut out, &self.validator.configuration_version)?;
        out.push(self.outcome.kind.wire_byte());
        out.extend_from_slice(&self.outcome.violation_count.to_be_bytes());
        let summary = self.outcome.canonical_violations()?;
        u32be(&mut out, summary.len())?;
        for entry in &summary {
            out.extend_from_slice(entry);
        }
        field(&mut out, &self.recorded_at.canonical())?;
        field(&mut out, &self.report_digest.to_string())?;
        opt(&mut out, self.report_reference.as_deref())?;
        Ok(out)
    }

    pub fn id(&self) -> Result<ValidationId, ProtocolError> {
        Ok(ValidationId(ContentId::for_bytes(&self.canonical_bytes()?)))
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut cursor = Cursor::new(bytes, VALIDATION_RECORD_V1_HEADER, "validation record")?;
        let graph_id = GraphId::new(cursor.field("graph_id")?)?;
        let candidate_commit: CommitId = cursor.field("candidate_commit")?.parse()?;
        let candidate_state_digest: ContentId = cursor.field("candidate_state_digest")?.parse()?;
        let semantic_execution_context_id: SemanticContextId =
            cursor.field("semantic_execution_context_id")?.parse()?;
        let validator = ValidatorIdentity {
            service_id: cursor.field("validator.service_id")?,
            service_version: cursor.field("validator.service_version")?,
            configuration_version: cursor.field("validator.configuration_version")?,
        };
        let kind = OutcomeKind::from_wire_byte(cursor.u8("outcome")?)?;
        let violation_count = cursor.u32("violation_count")?;
        let violations = cursor.set(
            "summarized violations",
            MAX_VIOLATION_SUMMARY,
            ViolationSummary::decode,
            ViolationSummary::canonical_bytes,
        )?;
        let recorded_at = LedgerTimestamp::parse_canonical(&cursor.field("recorded_at")?)?;
        let report_digest: ContentId = cursor.field("report_digest")?.parse()?;
        let report_reference = cursor.opt("report_reference")?;
        cursor.finish()?;
        let record = Self {
            graph_id,
            candidate_commit,
            candidate_state_digest,
            semantic_execution_context_id,
            validator,
            outcome: ValidationOutcome {
                kind,
                violation_count,
                violations,
            },
            recorded_at,
            report_digest,
            report_reference,
        };
        record.validate()?;
        Ok(record)
    }
}

impl<'de> Deserialize<'de> for ValidationRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            graph_id: GraphId,
            candidate_commit: CommitId,
            candidate_state_digest: ContentId,
            semantic_execution_context_id: SemanticContextId,
            validator: ValidatorIdentity,
            outcome: ValidationOutcome,
            recorded_at: LedgerTimestamp,
            report_digest: ContentId,
            #[serde(default)]
            report_reference: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let record = Self {
            graph_id: wire.graph_id,
            candidate_commit: wire.candidate_commit,
            candidate_state_digest: wire.candidate_state_digest,
            semantic_execution_context_id: wire.semantic_execution_context_id,
            validator: wire.validator,
            outcome: wire.outcome,
            recorded_at: wire.recorded_at,
            report_digest: wire.report_digest,
            report_reference: wire.report_reference,
        };
        record.validate().map_err(D::Error::custom)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(s: &str) -> ContentId {
        ContentId::for_bytes(s.as_bytes())
    }

    fn violation(severity: &str, code: &str, message: &str) -> ViolationSummary {
        ViolationSummary {
            severity: severity.into(),
            code: code.into(),
            message: message.into(),
        }
    }

    fn sample() -> ValidationRecord {
        ValidationRecord {
            graph_id: GraphId::new("graph-1").unwrap(),
            candidate_commit: CommitId(digest("candidate")),
            candidate_state_digest: digest("state"),
            semantic_execution_context_id: SemanticContextId(digest("context")),
            validator: ValidatorIdentity {
                service_id: "urn:sculpin:service:validator".into(),
                service_version: "2026.09".into(),
                configuration_version: "cfg-5".into(),
            },
            outcome: ValidationOutcome {
                kind: OutcomeKind::Violations,
                violation_count: 3,
                violations: vec![
                    violation(
                        "Violation",
                        "sh:MinCountConstraintComponent",
                        "missing label",
                    ),
                    violation("Violation", "reasoning:disjoint", ""),
                ],
            },
            recorded_at: LedgerTimestamp::parse_rfc3339("2026-09-27T12:00:00Z").unwrap(),
            report_digest: digest("report"),
            report_reference: Some("urn:sculpin:report:17".into()),
        }
    }

    #[test]
    fn round_trips_and_summary_order_is_irrelevant() {
        let record = sample();
        let bytes = record.canonical_bytes().unwrap();
        let decoded = ValidationRecord::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
        let mut reordered = sample();
        reordered.outcome.violations.reverse();
        reordered
            .outcome
            .violations
            .push(violation("Violation", "reasoning:disjoint", ""));
        assert_eq!(reordered.id().unwrap(), record.id().unwrap());
        assert_ne!(reordered, record, "structural equality sees caller order");
    }

    #[test]
    fn conforming_records_carry_no_violations() {
        let mut record = sample();
        record.outcome = ValidationOutcome::conforms();
        record.report_reference = None;
        let bytes = record.canonical_bytes().unwrap();
        assert_eq!(
            ValidationRecord::from_canonical_bytes(&bytes).unwrap(),
            record
        );
        let mut bad = record.clone();
        bad.outcome.violation_count = 1;
        assert!(bad.canonical_bytes().is_err());
        let mut summary_exceeds = sample();
        summary_exceeds.outcome.violation_count = 1;
        assert!(summary_exceeds.canonical_bytes().is_err());
        let mut zero = sample();
        zero.outcome.violation_count = 0;
        assert!(zero.canonical_bytes().is_err());
    }

    #[test]
    fn bounds_and_strictness() {
        let mut long = sample();
        long.outcome.violations[0].message = "m".repeat(MAX_VIOLATION_MESSAGE_BYTES + 1);
        assert!(long.canonical_bytes().is_err());
        let mut severity = sample();
        severity.outcome.violations[0].severity = "s".repeat(MAX_SEVERITY_BYTES + 1);
        assert!(severity.canonical_bytes().is_err());
        let mut reference = sample();
        reference.report_reference = Some("r".repeat(MAX_REPORT_REFERENCE_BYTES + 1));
        assert!(reference.canonical_bytes().is_err());
        let mut empty_reference = sample();
        empty_reference.report_reference = Some(String::new());
        assert!(empty_reference.canonical_bytes().is_err());
        let mut trailing = sample().canonical_bytes().unwrap();
        trailing.push(1);
        assert!(ValidationRecord::from_canonical_bytes(&trailing).is_err());
        // Non-canonical recorded_at inside the bytes is refused.
        let canonical = sample().canonical_bytes().unwrap();
        let mut noncanonical = Vec::new();
        field(&mut noncanonical, "2026-09-27T12:00:00.000000Z").unwrap();
        let mut replacement = Vec::new();
        field(&mut replacement, "2026-09-27T12:00:00Z").unwrap();
        let at = canonical
            .windows(noncanonical.len())
            .position(|w| w == noncanonical.as_slice())
            .unwrap();
        let mut bytes = canonical[..at].to_vec();
        bytes.extend_from_slice(&replacement);
        bytes.extend_from_slice(&canonical[at + noncanonical.len()..]);
        assert!(ValidationRecord::from_canonical_bytes(&bytes).is_err());
    }

    #[test]
    fn serde_round_trips_and_validates() {
        let json = serde_json::to_string(&sample()).unwrap();
        let back: ValidationRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sample());
        assert!(json.contains("\"kind\":\"violations\""));
        let bad = json.replacen("\"violation_count\":3", "\"violation_count\":0", 1);
        assert!(serde_json::from_str::<ValidationRecord>(&bad).is_err());
    }
}
