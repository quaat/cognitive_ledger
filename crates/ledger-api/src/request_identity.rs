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
//! `field` = u32 big-endian length + UTF-8 bytes; `opt` = 0x00 absent-or-empty | 0x01 + non-empty
//! field. Vectors live in `fixtures/golden/requests/` and are cross-checked by
//! `scripts/golden/request_v1_reference.py`.

use ledger_core::{CommitId, ContentId, GraphId, LedgerTimestamp, PatchId};

pub const REQUEST_IDENTITY_HEADER: &[u8] = b"sculpin-ledger-request/v1\0";

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
        let mut out = REQUEST_IDENTITY_HEADER.to_vec();
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
