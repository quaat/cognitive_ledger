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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebuildReason {
    MarkerMalformed,
    MarkerForAnotherStream,
    MarkerCommitMismatch,
    TripleCountMismatch,
    UnmarkedContent,
}

impl RebuildReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MarkerMalformed => "marker_malformed",
            Self::MarkerForAnotherStream => "marker_for_another_stream",
            Self::MarkerCommitMismatch => "marker_commit_mismatch",
            Self::TripleCountMismatch => "triple_count_mismatch",
            Self::UnmarkedContent => "unmarked_content",
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
    /// Write the target event's state. `Conditional` for the normal path (first projection
    /// or a predecessor marker); `Replace` with a reason for recovery.
    Write {
        mode: WriteMode,
        rebuild: Option<RebuildReason>,
    },
    /// Never regress automatically: an operator rebuild decides.
    RecoveryRequired(ProjectionError),
}

pub fn plan(view: &LedgerView, observation: &Observation) -> Plan {
    let rebuild = |reason| Plan::Write {
        mode: WriteMode::Replace,
        rebuild: Some(reason),
    };
    let marker = match &observation.marker {
        MarkerRead::Absent if observation.triple_count == 0 => {
            return Plan::Write {
                mode: WriteMode::Conditional,
                rebuild: None,
            };
        }
        MarkerRead::Absent => return rebuild(RebuildReason::UnmarkedContent),
        MarkerRead::Malformed(_) => return rebuild(RebuildReason::MarkerMalformed),
        MarkerRead::Present(marker) => marker,
    };
    if marker.graph_id != view.graph_id || marker.branch != view.branch {
        return rebuild(RebuildReason::MarkerForAnotherStream);
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
    // at that version.
    if view.commit_at_marker_version.as_ref() != Some(&marker.commit) {
        return rebuild(RebuildReason::MarkerCommitMismatch);
    }
    let counts_agree = marker.triple_count == observation.triple_count;
    if marker.ref_version < view.target_version {
        // Predecessor: the conditional write replaces the whole graph whatever it holds.
        return Plan::Write {
            mode: WriteMode::Conditional,
            rebuild: None,
        };
    }
    if !counts_agree {
        return rebuild(RebuildReason::TripleCountMismatch);
    }
    if marker.ref_version == view.target_version {
        Plan::AlreadyProjected
    } else {
        Plan::AcknowledgeBeyond {
            commit: marker.commit.clone(),
            version: marker.ref_version,
        }
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
        })
    }

    fn obs(marker: MarkerRead, count: u64) -> Observation {
        Observation {
            marker,
            triple_count: count,
        }
    }

    const CONDITIONAL: Plan = Plan::Write {
        mode: WriteMode::Conditional,
        rebuild: None,
    };

    fn rebuild(reason: RebuildReason) -> Plan {
        Plan::Write {
            mode: WriteMode::Replace,
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
        assert_eq!(
            plan(&view(5, 5, Some(4)), &obs(marker(4, 9), 2)),
            CONDITIONAL
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
        // marker for another stream → rebuild
        let mut other = view(3, 3, Some(2));
        other.branch = "dev".into();
        assert_eq!(
            plan(&other, &obs(marker(2, 1), 1)),
            rebuild(RebuildReason::MarkerForAnotherStream)
        );
    }
}
