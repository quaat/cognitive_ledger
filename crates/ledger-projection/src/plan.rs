//! The ADR-0020 decision table: what a projector does after observing a target. Pure: the
//! inputs are ledger facts and one observation; timestamps never participate.

use crate::{MarkerRead, Observation, ProjectionError, ProjectionErrorCode, WriteMode};
use ledger_core::{CommitId, GraphId};

/// The authoritative ledger facts for one stream at planning time.
#[derive(Clone, Debug)]
pub struct LedgerView {
    pub graph_id: GraphId,
    pub branch: String,
    /// The latest accepted outbox event beyond the recorded projection.
    pub target_commit: CommitId,
    pub target_version: i64,
    /// The ref's accepted head version (>= `target_version`).
    pub head_version: i64,
    /// The ref's head commit at the marker's version, when a well-formed marker for this
    /// stream names a version `<= head_version` (looked up by the caller).
    pub commit_at_marker_version: Option<CommitId>,
    /// The version the ledger records as projected for this stream, if any.
    pub recorded_version: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebuildReason {
    MarkerMalformed,
    MarkerCommitMismatch,
    TripleCountMismatch,
    UnmarkedContent,
    /// Marker and graph are gone although the ledger recorded a projection (the target lost
    /// its data).
    TargetLost,
}

impl RebuildReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MarkerMalformed => "marker_malformed",
            Self::MarkerCommitMismatch => "marker_commit_mismatch",
            Self::TripleCountMismatch => "triple_count_mismatch",
            Self::UnmarkedContent => "unmarked_content",
            Self::TargetLost => "target_lost",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Plan {
    /// The target already represents the target event: acknowledge, write nothing.
    AlreadyProjected,
    /// The target represents a later accepted version of this ref (a previous worker got
    /// there first): acknowledge up to that version, write nothing.
    AcknowledgeBeyond { commit: CommitId, version: i64 },
    /// Write the target event's state: conditionally (first projection or a predecessor
    /// marker), or as a guarded replacement with a reason (recovery).
    Write { rebuild: Option<RebuildReason> },
    /// Never regress automatically: an operator rebuild decides.
    RecoveryRequired(ProjectionError),
}

pub fn plan(view: &LedgerView, observation: &Observation) -> Plan {
    let rebuild = |reason| Plan::Write {
        rebuild: Some(reason),
    };
    let marker = match &observation.marker {
        MarkerRead::Absent if observation.triple_count == 0 => {
            // An empty target where the ledger recorded a projection lost its data.
            return match view.recorded_version {
                Some(_) => rebuild(RebuildReason::TargetLost),
                None => Plan::Write { rebuild: None },
            };
        }
        MarkerRead::Absent => return rebuild(RebuildReason::UnmarkedContent),
        // A malformed marker naming a version beyond the ledger head may be a newer
        // protocol or a newer ledger's marker: like a well-formed one, never regress it
        // automatically.
        MarkerRead::Malformed(_)
            if observation
                .max_ref_version
                .is_some_and(|v| v > view.head_version) =>
        {
            return Plan::RecoveryRequired(ProjectionError::permanent(
                ProjectionErrorCode::MarkerAhead,
                "the target's (malformed) marker names a ref version beyond the ledger head; \
                 an operator rebuild must decide (ADR-0020)",
            ));
        }
        MarkerRead::Malformed(_) => return rebuild(RebuildReason::MarkerMalformed),
        MarkerRead::Present(marker) => marker,
    };
    if marker.graph_id != view.graph_id || marker.branch != view.branch {
        // Another stream's projection lives in this cognitive graph (two deployments sharing
        // a dataset, or a switched feed graph): never overwrite it automatically.
        return Plan::RecoveryRequired(ProjectionError::permanent(
            ProjectionErrorCode::TargetConflict,
            "the cognitive graph holds another stream's projection; an operator rebuild must \
             decide (ADR-0020)",
        ));
    }
    if marker.ref_version > view.head_version {
        return Plan::RecoveryRequired(ProjectionError::permanent(
            ProjectionErrorCode::MarkerAhead,
            format!(
                "the target marker names ref version {} beyond the ledger head {}; an operator \
                 rebuild must decide (ADR-0020)",
                marker.ref_version, view.head_version
            ),
        ));
    }
    // A marker for this stream within the ledger's history must name the ledger's commit
    // at that version (otherwise the target is not a state of this ref: rebuild from the
    // ledger, which is authoritative).
    if view.commit_at_marker_version.as_ref() != Some(&marker.commit) {
        return rebuild(RebuildReason::MarkerCommitMismatch);
    }
    // The marker's count is the target's own count at write time; a difference means the
    // graph was edited out of band.
    if marker.triple_count != observation.triple_count {
        return rebuild(RebuildReason::TripleCountMismatch);
    }
    if marker.ref_version < view.target_version {
        Plan::Write { rebuild: None }
    } else if marker.ref_version == view.target_version {
        Plan::AlreadyProjected
    } else {
        Plan::AcknowledgeBeyond {
            commit: marker.commit.clone(),
            version: marker.ref_version,
        }
    }
}

/// The write mode for a plan's write (ADR-0020): conditional for the normal path, a
/// replacement of exactly the observed marker for recovery.
pub fn write_mode(rebuild: Option<RebuildReason>) -> WriteMode {
    match rebuild {
        None => WriteMode::Conditional,
        Some(_) => WriteMode::Replace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectionMarker;
    use ledger_core::ContentId;

    fn commit(n: i64) -> CommitId {
        CommitId(ContentId::for_bytes(format!("c{n}").as_bytes()))
    }

    fn view(target: i64, head: i64, at_marker: Option<i64>) -> LedgerView {
        LedgerView {
            graph_id: GraphId::new("g").unwrap(),
            branch: "main".into(),
            target_commit: commit(target),
            target_version: target,
            head_version: head,
            commit_at_marker_version: at_marker.map(commit),
            recorded_version: None,
        }
    }

    fn marker(version: i64, count: u64) -> MarkerRead {
        MarkerRead::Present(ProjectionMarker {
            graph_id: GraphId::new("g").unwrap(),
            branch: "main".into(),
            commit: commit(version),
            ref_version: version,
            state_digest: ContentId::for_bytes(b"s"),
            triple_count: count,
            write_id: format!("w{version}"),
        })
    }

    fn obs(marker: MarkerRead, count: u64) -> Observation {
        let max_ref_version = match &marker {
            MarkerRead::Present(m) => Some(m.ref_version),
            _ => None,
        };
        Observation {
            marker,
            triple_count: count,
            max_ref_version,
            terms: Vec::new(),
        }
    }

    const CONDITIONAL: Plan = Plan::Write { rebuild: None };

    fn rebuild(reason: RebuildReason) -> Plan {
        Plan::Write {
            rebuild: Some(reason),
        }
    }

    #[test]
    fn the_adr_0020_decision_table() {
        // first projection into an empty graph
        assert_eq!(
            plan(&view(1, 1, None), &obs(MarkerRead::Absent, 0)),
            CONDITIONAL
        );
        // the target lost everything although the ledger recorded a projection → rebuild
        let mut recorded = view(3, 3, None);
        recorded.recorded_version = Some(2);
        assert_eq!(
            plan(&recorded, &obs(MarkerRead::Absent, 0)),
            rebuild(RebuildReason::TargetLost)
        );
        // unmarked content → rebuild
        assert_eq!(
            plan(&view(1, 1, None), &obs(MarkerRead::Absent, 3)),
            rebuild(RebuildReason::UnmarkedContent)
        );
        // malformed → rebuild
        assert_eq!(
            plan(
                &view(2, 2, None),
                &obs(MarkerRead::Malformed("x".into()), 3)
            ),
            rebuild(RebuildReason::MarkerMalformed)
        );
        // exact → already projected
        assert_eq!(
            plan(&view(2, 2, Some(2)), &obs(marker(2, 5), 5)),
            Plan::AlreadyProjected
        );
        // exact version but the graph was edited out of band → rebuild
        assert_eq!(
            plan(&view(2, 2, Some(2)), &obs(marker(2, 5), 6)),
            rebuild(RebuildReason::TripleCountMismatch)
        );
        // predecessor (even several versions behind) → conditional write
        assert_eq!(
            plan(&view(5, 5, Some(1)), &obs(marker(1, 9), 9)),
            CONDITIONAL
        );
        // predecessor whose graph was edited → counted rebuild (same full write, logged)
        assert_eq!(
            plan(&view(5, 5, Some(4)), &obs(marker(4, 9), 2)),
            rebuild(RebuildReason::TripleCountMismatch)
        );
        // target beyond the event but within history → acknowledge beyond
        assert_eq!(
            plan(&view(3, 6, Some(5)), &obs(marker(5, 4), 4)),
            Plan::AcknowledgeBeyond {
                commit: commit(5),
                version: 5
            }
        );
        // marker ahead of the ledger head → recovery, never regress
        match plan(&view(3, 3, None), &obs(marker(9, 1), 1)) {
            Plan::RecoveryRequired(e) => assert_eq!(e.code(), ProjectionErrorCode::MarkerAhead),
            other => panic!("{other:?}"),
        }
        // marker names another commit at its version → rebuild
        assert_eq!(
            plan(&view(3, 3, Some(99)), &obs(marker(2, 1), 1)),
            rebuild(RebuildReason::MarkerCommitMismatch)
        );
        // marker for another stream → recovery (never overwrite another stream)
        let mut other = view(3, 3, Some(2));
        other.branch = "dev".into();
        match plan(&other, &obs(marker(2, 1), 1)) {
            Plan::RecoveryRequired(e) => {
                assert_eq!(e.code(), ProjectionErrorCode::TargetConflict)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn recovery_replaces_and_malformed_markers_ahead_of_the_head_are_not_regressed() {
        assert_eq!(write_mode(None), WriteMode::Conditional);
        assert_eq!(
            write_mode(Some(RebuildReason::MarkerMalformed)),
            WriteMode::Replace
        );
        let mut malformed = obs(MarkerRead::Malformed("two versions".into()), 3);
        malformed.max_ref_version = Some(9);
        match plan(&view(5, 5, None), &malformed) {
            Plan::RecoveryRequired(e) => assert_eq!(e.code(), ProjectionErrorCode::MarkerAhead),
            other => panic!("{other:?}"),
        }
        malformed.max_ref_version = Some(5);
        assert_eq!(
            plan(&view(5, 5, None), &malformed),
            rebuild(RebuildReason::MarkerMalformed)
        );
    }
}
