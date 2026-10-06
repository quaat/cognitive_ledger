//! Canonical HTTP request identity `sculpin-ledger-request/v1`.
//!
//! The API computes the request digest that scopes idempotency (ADR-0013); clients never
//! supply it. Requests are parsed and normalized first, then typed fields are encoded in
//! a fixed order with the same length-prefixed layout the commit envelopes use, so JSON
//! key order, whitespace and evidence order cannot change the digest while any
//! semantically different request does. Excluded by design: `Idempotency-Key`,
//! `correlation_id`, `recorded_at`, the authenticated actor (it is part of the idempotency
//! scope) and transport headers.
//!
//! ```text
//! "sculpin-ledger-request/v1\0"
//! field  operation              prepare | accept | reject
//! field  graph_id
//! field  branch
//! opt    expected_head          (commit id)
//! -- prepare --
//! field  requested patch id     (PatchId of the canonical requested patch)
//! field  activity
//! opt    event_time             (canonical LedgerTimestamp)
//! u32    evidence_count; field × n   (sorted bytewise, unique — a set)
//! opt    source_system
//! field  message
//! -- accept --
//! field  candidate
//! opt    reason
//! field  validation policy      ("no-validation" in P1.3/P1.4)
//! -- reject --
//! field  candidate
//! field  reason
//! ```
//! Frozen by ADR-0015.
//!
//! `sculpin-ledger-request/v2` (ADR-0015 amendment, Plan 0006) covers the validation-aware
//! operations and is selected by request shape: `validate`, and `accept`/`reject` that name
//! a `validation_id`. Legacy accept/reject shapes keep their v1 bytes.
//!
//! ```text
//! "sculpin-ledger-request/v2\0"
//! field operation · field graph_id
//! -- accept --   field branch · opt expected_head · field candidate · opt reason
//!                field "validated" · field validation_id · field semantic_environment_id
//! -- reject --   field branch · field candidate · field reason · field validation_id
//! -- validate -- field candidate · RequestedContext hints (tagged base_kb / ontology /
//!                shapes pairs, opt reasoning_profile, source-pin set)
//! ```
//! `sculpin-ledger-branch-request/v1` (ADR-0022, Plan 0008) covers the branch lifecycle
//! operations; a separate domain, so no v1/v2 byte changes:
//!
//! ```text
//! "sculpin-ledger-branch-request/v1\0"
//! field operation  branch_create | branch_delete | branch_restore
//! field graph_id · field name
//! -- branch_create --   field source · opt from_commit · u8 protected
//!                       · u8 require_validation · u8 require_distinct_reviewer   (0x00 | 0x01)
//! -- branch_delete / branch_restore --   opt reason
//! ```
//! `sculpin-ledger-merge-request/v1` (ADR-0024, Plan 0009) covers merge propose and apply;
//! a separate domain again (merge preview is a read and has no request identity):
//!
//! ```text
//! "sculpin-ledger-merge-request/v1\0"
//! field operation  merge_propose | merge_apply
//! field graph_id
//! -- merge_propose --  field source · field target · field strategy · opt base
//!                      · field preview_token · opt message
//!                      · u32 evidence_count · field evidence_ref × count (sorted, unique)
//! -- merge_apply --    field proposal_id (decimal) · field preview_token
//!                      · opt validation_id · opt semantic_environment_id · opt reason
//! ```
//! `field` = u32 big-endian length + UTF-8 bytes; `opt` = 0x00 absent-or-empty | 0x01 + non-empty
//! field. Vectors live in `fixtures/golden/requests/` and are cross-checked by
//! `scripts/golden/request_v1_reference.py`.

use ledger_core::{CommitId, ContentId, GraphId, LedgerTimestamp, PatchId};
use ledger_validation_protocol::{RequestedContext, SemanticEnvironmentId, ValidationId};

pub const REQUEST_IDENTITY_HEADER: &[u8] = b"sculpin-ledger-request/v1\0";
pub const REQUEST_IDENTITY_V2_HEADER: &[u8] = b"sculpin-ledger-request/v2\0";
pub const BRANCH_REQUEST_IDENTITY_HEADER: &[u8] = b"sculpin-ledger-branch-request/v1\0";
pub const MERGE_REQUEST_IDENTITY_HEADER: &[u8] = b"sculpin-ledger-merge-request/v1\0";

/// The normalized, typed content of a mutation request — everything the digest covers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanonicalRequest {
    Prepare {
        graph: GraphId,
        branch: String,
        expected_head: Option<CommitId>,
        requested_patch: PatchId,
        activity: String,
        event_time: Option<LedgerTimestamp>,
        evidence_refs: Vec<String>,
        source_system: Option<String>,
        message: String,
    },
    Accept {
        graph: GraphId,
        branch: String,
        expected_head: Option<CommitId>,
        candidate: CommitId,
        reason: Option<String>,
        validation_policy: String,
    },
    Reject {
        graph: GraphId,
        branch: String,
        candidate: CommitId,
        reason: String,
    },
    /// v2: acceptance under a named validation and semantic context (ADR-0019).
    AcceptValidated {
        graph: GraphId,
        branch: String,
        expected_head: Option<CommitId>,
        candidate: CommitId,
        reason: Option<String>,
        validation_id: ValidationId,
        semantic_environment_id: SemanticEnvironmentId,
    },
    /// v2: rejection citing a validation record.
    RejectValidated {
        graph: GraphId,
        branch: String,
        candidate: CommitId,
        reason: String,
        validation_id: ValidationId,
    },
    /// v2: request a validation of a prepared candidate.
    Validate {
        graph: GraphId,
        candidate: CommitId,
        requested: RequestedContext,
    },
    BranchCreate {
        graph: GraphId,
        name: String,
        source: String,
        from_commit: Option<CommitId>,
        protected: bool,
        require_validation: bool,
        require_distinct_reviewer: bool,
    },
    BranchDelete {
        graph: GraphId,
        name: String,
        reason: Option<String>,
    },
    BranchRestore {
        graph: GraphId,
        name: String,
        reason: Option<String>,
    },
    MergePropose {
        graph: GraphId,
        source: String,
        target: String,
        /// `abort` | `take-target` | `take-source` | `union`.
        strategy: String,
        base: Option<CommitId>,
        preview_token: String,
        message: Option<String>,
        evidence_refs: Vec<String>,
    },
    MergeApply {
        graph: GraphId,
        proposal_id: i64,
        preview_token: String,
        validation_id: Option<ValidationId>,
        semantic_environment_id: Option<SemanticEnvironmentId>,
        reason: Option<String>,
    },
}

fn field(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(
        &u32::try_from(value.len())
            .expect("bounded field")
            .to_be_bytes(),
    );
    out.extend_from_slice(value.as_bytes());
}

/// Optional field: `0x00` when absent **or empty** (an empty string carries no meaning and
/// is normalized to absence, in the API handlers, here, and in the Python reference alike),
/// otherwise `0x01` + field.
fn opt(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        None | Some("") => out.push(0),
        Some(v) => {
            out.push(1);
            field(out, v);
        }
    }
}

impl CanonicalRequest {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = match self {
            Self::Prepare { .. } | Self::Accept { .. } | Self::Reject { .. } => {
                REQUEST_IDENTITY_HEADER.to_vec()
            }
            Self::AcceptValidated { .. } | Self::RejectValidated { .. } | Self::Validate { .. } => {
                REQUEST_IDENTITY_V2_HEADER.to_vec()
            }
            Self::BranchCreate { .. } | Self::BranchDelete { .. } | Self::BranchRestore { .. } => {
                BRANCH_REQUEST_IDENTITY_HEADER.to_vec()
            }
            Self::MergePropose { .. } | Self::MergeApply { .. } => {
                MERGE_REQUEST_IDENTITY_HEADER.to_vec()
            }
        };
        match self {
            Self::Prepare {
                graph,
                branch,
                expected_head,
                requested_patch,
                activity,
                event_time,
                evidence_refs,
                source_system,
                message,
            } => {
                field(&mut out, "prepare");
                field(&mut out, graph.as_str());
                field(&mut out, branch);
                opt(
                    &mut out,
                    expected_head.as_ref().map(ToString::to_string).as_deref(),
                );
                field(&mut out, &requested_patch.to_string());
                field(&mut out, activity);
                opt(&mut out, event_time.map(|t| t.canonical()).as_deref());
                let mut evidence = evidence_refs.clone();
                evidence.sort_unstable();
                evidence.dedup();
                out.extend_from_slice(
                    &u32::try_from(evidence.len())
                        .expect("bounded")
                        .to_be_bytes(),
                );
                for e in &evidence {
                    field(&mut out, e);
                }
                opt(&mut out, source_system.as_deref());
                field(&mut out, message);
            }
            Self::Accept {
                graph,
                branch,
                expected_head,
                candidate,
                reason,
                validation_policy,
            } => {
                field(&mut out, "accept");
                field(&mut out, graph.as_str());
                field(&mut out, branch);
                opt(
                    &mut out,
                    expected_head.as_ref().map(ToString::to_string).as_deref(),
                );
                field(&mut out, &candidate.to_string());
                opt(&mut out, reason.as_deref());
                field(&mut out, validation_policy);
            }
            Self::Reject {
                graph,
                branch,
                candidate,
                reason,
            } => {
                field(&mut out, "reject");
                field(&mut out, graph.as_str());
                field(&mut out, branch);
                field(&mut out, &candidate.to_string());
                field(&mut out, reason);
            }
            Self::AcceptValidated {
                graph,
                branch,
                expected_head,
                candidate,
                reason,
                validation_id,
                semantic_environment_id,
            } => {
                field(&mut out, "accept");
                field(&mut out, graph.as_str());
                field(&mut out, branch);
                opt(
                    &mut out,
                    expected_head.as_ref().map(ToString::to_string).as_deref(),
                );
                field(&mut out, &candidate.to_string());
                opt(&mut out, reason.as_deref());
                field(&mut out, "validated");
                field(&mut out, &validation_id.to_string());
                field(&mut out, &semantic_environment_id.to_string());
            }
            Self::RejectValidated {
                graph,
                branch,
                candidate,
                reason,
                validation_id,
            } => {
                field(&mut out, "reject");
                field(&mut out, graph.as_str());
                field(&mut out, branch);
                field(&mut out, &candidate.to_string());
                field(&mut out, reason);
                field(&mut out, &validation_id.to_string());
            }
            Self::Validate {
                graph,
                candidate,
                requested,
            } => {
                field(&mut out, "validate");
                field(&mut out, graph.as_str());
                field(&mut out, &candidate.to_string());
                // Validated by the handler before the digest is computed; the encoder is
                // total over valid hints.
                requested
                    .encode_into(&mut out)
                    .expect("requested context was validated by the handler");
            }
            Self::BranchCreate {
                graph,
                name,
                source,
                from_commit,
                protected,
                require_validation,
                require_distinct_reviewer,
            } => {
                field(&mut out, "branch_create");
                field(&mut out, graph.as_str());
                field(&mut out, name);
                field(&mut out, source);
                opt(
                    &mut out,
                    from_commit.as_ref().map(ToString::to_string).as_deref(),
                );
                out.push(u8::from(*protected));
                out.push(u8::from(*require_validation));
                out.push(u8::from(*require_distinct_reviewer));
            }
            Self::BranchDelete {
                graph,
                name,
                reason,
            }
            | Self::BranchRestore {
                graph,
                name,
                reason,
            } => {
                field(
                    &mut out,
                    if matches!(self, Self::BranchDelete { .. }) {
                        "branch_delete"
                    } else {
                        "branch_restore"
                    },
                );
                field(&mut out, graph.as_str());
                field(&mut out, name);
                opt(&mut out, reason.as_deref());
            }
            Self::MergePropose {
                graph,
                source,
                target,
                strategy,
                base,
                preview_token,
                message,
                evidence_refs,
            } => {
                field(&mut out, "merge_propose");
                field(&mut out, graph.as_str());
                field(&mut out, source);
                field(&mut out, target);
                field(&mut out, strategy);
                opt(&mut out, base.as_ref().map(ToString::to_string).as_deref());
                field(&mut out, preview_token);
                opt(&mut out, message.as_deref());
                let mut evidence = evidence_refs.clone();
                evidence.sort_unstable();
                evidence.dedup();
                out.extend_from_slice(
                    &u32::try_from(evidence.len())
                        .expect("bounded")
                        .to_be_bytes(),
                );
                for e in &evidence {
                    field(&mut out, e);
                }
            }
            Self::MergeApply {
                graph,
                proposal_id,
                preview_token,
                validation_id,
                semantic_environment_id,
                reason,
            } => {
                field(&mut out, "merge_apply");
                field(&mut out, graph.as_str());
                field(&mut out, &proposal_id.to_string());
                field(&mut out, preview_token);
                opt(
                    &mut out,
                    validation_id.as_ref().map(ToString::to_string).as_deref(),
                );
                opt(
                    &mut out,
                    semantic_environment_id
                        .as_ref()
                        .map(ToString::to_string)
                        .as_deref(),
                );
                opt(&mut out, reason.as_deref());
            }
        }
        out
    }

    /// The request digest handed to `WorkflowRepository` as `RequestScope.request_digest`.
    pub fn digest(&self) -> ContentId {
        ContentId::for_bytes(&self.canonical_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> GraphId {
        GraphId::new("0b0a2a1c-2e3d-4f50-8a61-72b384c5d6e7").unwrap()
    }

    #[test]
    fn evidence_order_and_duplicates_do_not_change_the_digest() {
        let a = CanonicalRequest::Prepare {
            graph: graph(),
            branch: "main".into(),
            expected_head: None,
            requested_patch: PatchId(ContentId::for_bytes(b"p")),
            activity: "cognitive-correction".into(),
            event_time: None,
            evidence_refs: vec!["urn:e:2".into(), "urn:e:1".into()],
            source_system: None,
            message: "m".into(),
        };
        let CanonicalRequest::Prepare { evidence_refs, .. } = &a else {
            unreachable!()
        };
        let mut b = a.clone();
        if let CanonicalRequest::Prepare {
            evidence_refs: e, ..
        } = &mut b
        {
            *e = vec!["urn:e:1".into(), "urn:e:2".into(), "urn:e:1".into()];
        }
        assert_ne!(
            evidence_refs,
            &vec!["urn:e:1".to_string(), "urn:e:2".to_string()]
        );
        assert_eq!(a.digest(), b.digest());
        let mut c = a.clone();
        if let CanonicalRequest::Prepare { message, .. } = &mut c {
            *message = "m2".into();
        }
        assert_ne!(a.digest(), c.digest());
    }

    #[test]
    fn absent_and_present_optionals_differ_and_operations_differ() {
        let base = CanonicalRequest::Accept {
            graph: graph(),
            branch: "main".into(),
            expected_head: None,
            candidate: CommitId(ContentId::for_bytes(b"c")),
            reason: None,
            validation_policy: "no-validation".into(),
        };
        let mut with_reason = base.clone();
        if let CanonicalRequest::Accept { reason, .. } = &mut with_reason {
            *reason = Some("because".into());
        }
        assert_ne!(base.digest(), with_reason.digest());
        let reject = CanonicalRequest::Reject {
            graph: graph(),
            branch: "main".into(),
            candidate: CommitId(ContentId::for_bytes(b"c")),
            reason: "because".into(),
        };
        assert_ne!(with_reason.digest(), reject.digest());
    }
}
