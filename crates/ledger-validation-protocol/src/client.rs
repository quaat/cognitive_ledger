//! The validator boundary (ADR-0014): what the ledger sends a validation service, what it
//! receives, and the `ValidationClient` trait an HTTP adapter or a deterministic test
//! validator implements. Sculpin reconstructs the effective semantic state from references
//! and digests; the ledger only ships the immutable candidate it owns.

use crate::{
    BaseKb, Ontology, ProtocolError, Reasoning, SemanticExecutionContext, ShapeSet,
    ValidationInvocationId, ValidationOutcome, ValidatorIdentity, VirtualContextRef,
    encoding::{TAG_ABSENT, TAG_PRESENT, field, opt},
};
use ledger_core::{CommitId, ContentId, GraphId, MAX_IDENTIFIER_BYTES, validate_token};
use serde::{Deserialize, Serialize};

pub const VALIDATION_REQUEST_PROTOCOL: &str = "sculpin-validation-request/v1";
pub const VALIDATION_RESPONSE_PROTOCOL: &str = "sculpin-validation-response/v1";
/// Protocol cap on candidate quads shipped inline (deployments bound bytes far lower).
pub const MAX_CANDIDATE_QUADS_INLINE: usize = 1_000_000;

fn token(field_name: &'static str, value: &str) -> Result<(), ProtocolError> {
    validate_token(field_name, value, MAX_IDENTIFIER_BYTES).map_err(ProtocolError::from)
}

/// What the requester asks the validator to pin, if anything. Everything is optional: an
/// absent hint lets Sculpin choose its current ontology, shapes, base KB or reasoning; a
/// present one is a request Sculpin may honour or refuse. Hints are request identity
/// (ADR-0015 v2), so the same key with different hints is `IDEMPOTENCY_CONFLICT`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestedContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_kb: Option<BaseKb>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ontology: Option<Ontology>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shapes: Option<ShapeSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_profile: Option<String>,
    /// The external-source catalog revision the validation must run under. (Individual
    /// source-version pins are deliberately not a hint: a pinned run could report the
    /// current catalog revision and alias the current environment.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources_revision: Option<String>,
}

impl RequestedContext {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if let Some(kb) = &self.base_kb {
            token("base_kb.kb_id", &kb.kb_id)?;
            token("base_kb.revision", &kb.revision)?;
        }
        if let Some(ontology) = &self.ontology {
            token("ontology.id", &ontology.id)?;
            token("ontology.version", &ontology.version)?;
        }
        if let Some(shapes) = &self.shapes {
            token("shapes.id", &shapes.id)?;
            token("shapes.version", &shapes.version)?;
        }
        if let Some(profile) = &self.reasoning_profile {
            token("reasoning_profile", profile)?;
        }
        if let Some(revision) = &self.sources_revision {
            token("sources_revision", revision)?;
        }
        Ok(())
    }

    /// A present hint the validator did not honour, if any: the effective context must use
    /// exactly the hinted base KB, ontology, shapes, reasoning profile and sources revision.
    /// Opaque comparison only.
    pub fn unmet_by(&self, effective: &EffectiveContext) -> Option<&'static str> {
        if self
            .base_kb
            .as_ref()
            .is_some_and(|kb| kb != &effective.base_kb)
        {
            return Some("base_kb");
        }
        if self.ontology.is_some() && self.ontology != effective.ontology {
            return Some("ontology");
        }
        if self.shapes.as_ref().is_some_and(|s| s != &effective.shapes) {
            return Some("shapes");
        }
        if let Some(profile) = &self.reasoning_profile
            && effective.reasoning.as_ref().map(|r| &r.profile) != Some(profile)
        {
            return Some("reasoning_profile");
        }
        if self.sources_revision.is_some() && self.sources_revision != effective.sources_revision {
            return Some("sources_revision");
        }
        None
    }

    /// Append the hint encoding used by request identity v2 (ADR-0015 amendment):
    /// tagged pairs for base KB, ontology and shapes, `opt` reasoning profile, `opt`
    /// sources revision.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), ProtocolError> {
        self.validate()?;
        let pair = |a: Option<(&str, &str)>, out: &mut Vec<u8>| -> Result<(), ProtocolError> {
            match a {
                None => out.push(TAG_ABSENT),
                Some((x, y)) => {
                    out.push(TAG_PRESENT);
                    field(out, x)?;
                    field(out, y)?;
                }
            }
            Ok(())
        };
        pair(
            self.base_kb
                .as_ref()
                .map(|kb| (kb.kb_id.as_str(), kb.revision.as_str())),
            out,
        )?;
        pair(
            self.ontology
                .as_ref()
                .map(|o| (o.id.as_str(), o.version.as_str())),
            out,
        )?;
        pair(
            self.shapes
                .as_ref()
                .map(|s| (s.id.as_str(), s.version.as_str())),
            out,
        )?;
        opt(out, self.reasoning_profile.as_deref())?;
        opt(out, self.sources_revision.as_deref())?;
        Ok(())
    }
}

/// The immutable candidate the validator is asked about: references plus, bounded, the
/// reconstructed quads themselves (the ledger already holds them for the digest; shipping
/// them avoids a callback while `state_href` lets a validator re-fetch or verify).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateDescriptor {
    pub graph_id: GraphId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_base_id: Option<String>,
    pub commit: CommitId,
    /// `sculpin-rdf-state/v1` digest of `quads`.
    pub state_digest: ContentId,
    /// Ledger read URL of the candidate state (graph-scoped; the validator needs `read`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_href: Option<String>,
    /// Canonical N-Quads lines of the candidate state, bytewise sorted.
    pub quads: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationRequest {
    pub protocol: String,
    /// The logical invocation (`sculpin-validation-invocation/v1`): identical for every
    /// delivery of the same ledger request (concurrent duplicate, retry, crash recovery). The
    /// validator must resolve equal ids to one logical validation; the HTTP adapter also
    /// sends it as `Idempotency-Key`.
    pub invocation_id: ValidationInvocationId,
    pub candidate: CandidateDescriptor,
    #[serde(default)]
    pub requested: RequestedContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl ValidationRequest {
    pub fn new(
        invocation_id: ValidationInvocationId,
        candidate: CandidateDescriptor,
        requested: RequestedContext,
    ) -> Self {
        Self {
            protocol: VALIDATION_REQUEST_PROTOCOL.into(),
            invocation_id,
            candidate,
            requested,
            correlation_id: None,
        }
    }
}

/// Versions the validator declares about itself; the service identity comes from the
/// ledger's configuration, never from the response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorVersions {
    pub service_version: String,
    pub configuration_version: String,
}

/// The context the validator actually used (the effective one, which may differ from the
/// hints the requester sent).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveContext {
    pub base_kb: BaseKb,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ontology: Option<Ontology>,
    pub shapes: ShapeSet,
    /// Absent when no reasoning ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// Sculpin's external-source catalog revision in force (absent without external sources).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources_revision: Option<String>,
    #[serde(default)]
    pub virtual_contexts: Vec<VirtualContextRef>,
    pub validator: ValidatorVersions,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportReference {
    pub digest: ContentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorResponse {
    pub protocol: String,
    pub candidate_commit: CommitId,
    pub candidate_state_digest: ContentId,
    pub context: EffectiveContext,
    pub outcome: ValidationOutcome,
    pub report: ReportReference,
}

impl ValidatorResponse {
    /// Bound the result summary deterministically before it becomes a record: control
    /// characters in messages become spaces (SHACL engines emit multi-line messages), messages
    /// are cut to the protocol cap at a character boundary, and the summary keeps the first
    /// `MAX_VIOLATION_SUMMARY` entries in canonical order. `violation_count` (the validator's
    /// total) is untouched. Formatting only — the ledger never evaluates the results.
    pub fn with_bounded_summary(mut self) -> Self {
        for v in &mut self.outcome.violations {
            let cleaned: String = v
                .message
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            let mut cut = cleaned.len().min(crate::MAX_VIOLATION_MESSAGE_BYTES);
            while !cleaned.is_char_boundary(cut) {
                cut -= 1;
            }
            v.message = cleaned[..cut].to_owned();
        }
        let mut encoded: Vec<(Vec<u8>, crate::ViolationSummary)> = self
            .outcome
            .violations
            .drain(..)
            .map(|v| (v.canonical_bytes().unwrap_or_default(), v))
            .collect();
        encoded.sort_by(|a, b| a.0.cmp(&b.0));
        encoded.dedup_by(|a, b| a.0 == b.0);
        self.outcome.violations = encoded
            .into_iter()
            .take(crate::MAX_VIOLATION_SUMMARY)
            .map(|(_, v)| v)
            .collect();
        self
    }

    /// Turn the response into the context the ledger records, refusing a response that
    /// names another protocol version, candidate or state than the one asked about, or that
    /// ignored a hint the requester set. `service_id` is the ledger's configured identity of
    /// the validator it called.
    pub fn into_context(
        &self,
        graph_id: &GraphId,
        candidate: &CommitId,
        state_digest: &ContentId,
        service_id: &str,
        requested: &RequestedContext,
    ) -> Result<SemanticExecutionContext, ProtocolError> {
        if self.protocol != VALIDATION_RESPONSE_PROTOCOL {
            return Err(ProtocolError::Invalid(format!(
                "validator response protocol {:?} is not {VALIDATION_RESPONSE_PROTOCOL}",
                bounded(&self.protocol)
            )));
        }
        if &self.candidate_commit != candidate {
            return Err(ProtocolError::Invalid(
                "validator response names another candidate commit".into(),
            ));
        }
        if &self.candidate_state_digest != state_digest {
            return Err(ProtocolError::Invalid(
                "validator response names another candidate state digest".into(),
            ));
        }
        if let Some(hint) = requested.unmet_by(&self.context) {
            return Err(ProtocolError::Invalid(format!(
                "validator response did not honour the requested {hint}"
            )));
        }
        let context = SemanticExecutionContext {
            graph_id: graph_id.clone(),
            candidate_commit: candidate.clone(),
            candidate_state_digest: state_digest.clone(),
            base_kb: self.context.base_kb.clone(),
            ontology: self.context.ontology.clone(),
            shapes: self.context.shapes.clone(),
            reasoning: self.context.reasoning.clone(),
            sources_revision: self.context.sources_revision.clone(),
            virtual_contexts: self.context.virtual_contexts.clone(),
            validator: ValidatorIdentity {
                service_id: service_id.to_owned(),
                service_version: self.context.validator.service_version.clone(),
                configuration_version: self.context.validator.configuration_version.clone(),
            },
        };
        context.validate()?;
        self.outcome.validate()?;
        if let Some(reference) = &self.report.reference {
            validate_token(
                "report_reference",
                reference,
                crate::MAX_REPORT_REFERENCE_BYTES,
            )?;
        }
        Ok(context)
    }
}

fn bounded(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// Why a validator call produced no usable response. `Unavailable` is retryable (nothing
/// was recorded; the client may retry with the same idempotency key); `Rejected` means the
/// validator answered but the answer cannot become a record (refused request, malformed or
/// oversized body, wrong content type, mismatching candidate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationClientError {
    Unavailable(String),
    Rejected(String),
}

impl From<ValidationClientError> for ledger_core::LedgerError {
    fn from(error: ValidationClientError) -> Self {
        match error {
            ValidationClientError::Unavailable(reason) => Self::ValidatorUnavailable(reason),
            ValidationClientError::Rejected(reason) => Self::ValidatorError(reason),
        }
    }
}

/// The boundary the ledger calls a semantic validation service through. Implementations
/// are an HTTP adapter (production) or a deterministic fake (tests); neither interprets
/// semantics.
#[async_trait::async_trait]
pub trait ValidationClient: Send + Sync {
    async fn validate(
        &self,
        request: &ValidationRequest,
    ) -> Result<ValidatorResponse, ValidationClientError>;
    /// Human-readable description for logs (never credentials).
    fn describe(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(s: &str) -> ContentId {
        ContentId::for_bytes(s.as_bytes())
    }

    fn response() -> ValidatorResponse {
        ValidatorResponse {
            protocol: VALIDATION_RESPONSE_PROTOCOL.into(),
            candidate_commit: CommitId(digest("c")),
            candidate_state_digest: digest("s"),
            context: EffectiveContext {
                base_kb: BaseKb {
                    kb_id: "kb".into(),
                    revision: "r1".into(),
                },
                ontology: None,
                shapes: ShapeSet {
                    id: "shapes".into(),
                    version: "1".into(),
                },
                reasoning: None,
                sources_revision: None,
                virtual_contexts: vec![],
                validator: ValidatorVersions {
                    service_version: "1".into(),
                    configuration_version: "1".into(),
                },
            },
            outcome: ValidationOutcome::conforms(),
            report: ReportReference {
                digest: digest("report"),
                reference: None,
            },
        }
    }

    #[test]
    fn response_must_name_the_asked_candidate_and_state() {
        let graph = GraphId::new("g").unwrap();
        let r = response();
        let context = r
            .into_context(
                &graph,
                &CommitId(digest("c")),
                &digest("s"),
                "urn:svc",
                &RequestedContext::default(),
            )
            .unwrap();
        assert_eq!(context.validator.service_id, "urn:svc");
        assert!(
            r.into_context(
                &graph,
                &CommitId(digest("other")),
                &digest("s"),
                "urn:svc",
                &RequestedContext::default()
            )
            .is_err()
        );
        assert!(
            r.into_context(
                &graph,
                &CommitId(digest("c")),
                &digest("other"),
                "urn:svc",
                &RequestedContext::default()
            )
            .is_err()
        );
        let mut wrong_protocol = response();
        wrong_protocol.protocol = "sculpin-validation-response/v9".into();
        assert!(
            wrong_protocol
                .into_context(
                    &graph,
                    &CommitId(digest("c")),
                    &digest("s"),
                    "urn:svc",
                    &RequestedContext::default()
                )
                .is_err()
        );
    }

    #[test]
    fn unhonoured_hints_are_refused_and_messages_are_bounded() {
        let graph = GraphId::new("g").unwrap();
        let r = response();
        let hinted = RequestedContext {
            shapes: Some(ShapeSet {
                id: "shapes".into(),
                version: "2".into(),
            }),
            ..RequestedContext::default()
        };
        assert!(
            r.into_context(
                &graph,
                &CommitId(digest("c")),
                &digest("s"),
                "urn:svc",
                &hinted
            )
            .is_err()
        );
        let honoured = RequestedContext {
            shapes: Some(ShapeSet {
                id: "shapes".into(),
                version: "1".into(),
            }),
            ..RequestedContext::default()
        };
        assert!(
            r.into_context(
                &graph,
                &CommitId(digest("c")),
                &digest("s"),
                "urn:svc",
                &honoured
            )
            .is_ok()
        );
        let mut noisy = response();
        noisy.outcome = ValidationOutcome {
            kind: crate::OutcomeKind::Violations,
            violation_count: 100,
            violations: (0..80)
                .map(|i| crate::ViolationSummary {
                    severity: "Violation".into(),
                    code: format!("c{i:02}"),
                    message: format!("line one\nline two {}", "é".repeat(700)),
                })
                .collect(),
        };
        let bounded = noisy.with_bounded_summary();
        assert_eq!(
            bounded.outcome.violations.len(),
            crate::MAX_VIOLATION_SUMMARY
        );
        assert!(bounded.outcome.validate().is_ok());
        assert!(!bounded.outcome.violations[0].message.contains('\n'));
        assert!(bounded.outcome.violations[0].message.len() <= crate::MAX_VIOLATION_MESSAGE_BYTES);
    }

    #[test]
    fn requested_context_hints_encode_deterministically() {
        let a = RequestedContext {
            sources_revision: Some("rev-22".into()),
            ..RequestedContext::default()
        };
        let b = a.clone();
        let mut ea = Vec::new();
        let mut eb = Vec::new();
        a.encode_into(&mut ea).unwrap();
        b.encode_into(&mut eb).unwrap();
        assert_eq!(ea, eb);
        let mut other_revision = Vec::new();
        RequestedContext {
            sources_revision: Some("rev-23".into()),
            ..RequestedContext::default()
        }
        .encode_into(&mut other_revision)
        .unwrap();
        assert_ne!(ea, other_revision);
        let mut c = a.clone();
        c.reasoning_profile = Some("rdfs".into());
        let mut ec = Vec::new();
        c.encode_into(&mut ec).unwrap();
        assert_ne!(ea, ec);
        let mut empty = a.clone();
        empty.reasoning_profile = Some(String::new());
        assert!(empty.encode_into(&mut Vec::new()).is_err());
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<RequestedContext>(&json).unwrap(), c);
    }
}
