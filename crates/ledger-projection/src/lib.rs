//! Accepted-state projection protocol `sculpin-ledger-projection/v1` (ADR-0020).
//!
//! The ledger is authoritative; a projection target (Fuseki) is a derived view of the
//! **accepted** state of one ref. This crate is infrastructure-free: it defines the cognitive
//! graph identity derived from a knowledge-base id, the projection marker a target carries,
//! the state that is projected, the decision table that says what a projector must do after
//! observing a target, the [`ProjectionClient`] boundary an adapter implements, and the error
//! taxonomy. It never talks HTTP or SQL and never interprets RDF semantics.

mod error;
mod marker;
mod plan;
mod target;

pub use error::{ErrorClass, ProjectionError, ProjectionErrorCode};
pub use marker::{MarkerRead, MarkerTerm, ProjectionMarker, marker_predicates};
pub use plan::{LedgerView, Plan, RebuildReason, plan, write_mode};
pub use target::{
    CognitiveGraph, MARKER_GRAPH, PROBE_GRAPH, TARGET_SUBJECT, pct_decode, pct_encode,
};

use ledger_core::ContentId;
use ledger_rdf::Quad;
use std::collections::BTreeSet;

/// The protocol identifier written into every marker.
pub const PROJECTION_PROTOCOL: &str = "sculpin-ledger-projection/v1";
/// Namespace of the marker predicates (`lp:`).
pub const LP_NAMESPACE: &str = "urn:sculpin:ledger-projection:v1#";
pub const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
pub const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// The accepted state as it is written into the cognitive graph: the default-graph triples
/// of the reconstructed ledger state, in canonical order, with the state digest and size the
/// marker records. v1 refuses named-graph quads (ADR-0020).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedState {
    triples: Vec<String>,
    digest: ContentId,
}

impl ProjectedState {
    pub fn from_state(state: &BTreeSet<Quad>) -> Result<Self, ProjectionError> {
        let mut triples = Vec::with_capacity(state.len());
        for quad in state {
            let Some(triple) = quad.default_graph_triple() else {
                return Err(ProjectionError::permanent(
                    ProjectionErrorCode::NamedGraphUnsupported,
                    "the accepted state contains quads in a named graph; projection v1 \
                     projects the default graph only (ADR-0020)",
                ));
            };
            triples.push(triple);
        }
        Ok(Self {
            triples,
            digest: ledger_rdf::state_digest(state),
        })
    }

    /// N-Triples statements (`<s> <p> <o> .`), canonical order, no duplicates.
    pub fn triples(&self) -> &[String] {
        &self.triples
    }

    /// `sculpin-rdf-state/v1` digest of the ledger state (ADR-0018).
    pub fn digest(&self) -> &ContentId {
        &self.digest
    }

    pub fn triple_count(&self) -> u64 {
        self.triples.len() as u64
    }

    /// Total bytes of the triple statements (for request-size limits).
    pub fn byte_len(&self) -> usize {
        self.triples.iter().map(|t| t.len() + 1).sum()
    }
}

/// What a projector learns from one consistent read of the target: the marker for the
/// cognitive graph and how many triples the graph holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub marker: MarkerRead,
    pub triple_count: u64,
    /// The highest `lp:refVersion` integer found on the marker subject, even when the
    /// marker is malformed (the ceiling of a guarded replacement, ADR-0020).
    pub max_ref_version: Option<i64>,
}

/// How a write treats the existing target content (ADR-0020). Both modes are guarded in
/// the target transaction, so a stale worker can never move the marker backwards past
/// something newer than it observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteMode {
    /// Applies only while the target's marker is absent or names only older ref versions:
    /// a duplicate or stale write is a no-op.
    Conditional,
    /// Recovery: replaces graph and marker whatever they hold, unless the marker names a
    /// ref version above `ceiling` (the highest version the planner observed, or the
    /// version being written, whichever is larger).
    Replace { ceiling: i64 },
}

/// The target boundary. Implementations are adapters (HTTP/Fuseki); they must run each
/// method as one target transaction and classify every failure.
#[async_trait::async_trait]
pub trait ProjectionClient: Send + Sync {
    /// Read the marker for `graph` and the graph's triple count in one read transaction.
    async fn observe(&self, graph: &CognitiveGraph) -> Result<Observation, ProjectionError>;
    /// Write `state` and `marker` into `graph` as one target transaction.
    async fn write(
        &self,
        graph: &CognitiveGraph,
        state: &ProjectedState,
        marker: &ProjectionMarker,
        mode: WriteMode,
    ) -> Result<(), ProjectionError>;
    /// The complete content of `graph` as default-graph quads, as the target returns them
    /// (the target may canonicalize literal lexical forms; diagnostics and tests).
    async fn read_graph(&self, graph: &CognitiveGraph) -> Result<BTreeSet<Quad>, ProjectionError>;
    /// Whether every triple of `state` is in `graph`, compared by the target's own term
    /// equality (so its literal canonicalization cannot cause a false mismatch).
    async fn contains_all(
        &self,
        graph: &CognitiveGraph,
        state: &ProjectedState,
    ) -> Result<bool, ProjectionError>;
    /// Bind the dataset to `target_id` (first use) or verify it is bound to it: one dataset
    /// never answers to two target ids (ADR-0020).
    async fn bind_target(&self, target_id: &str) -> Result<(), ProjectionError>;
    /// Prove the target rolls a failed multi-operation update back completely; refuse to run
    /// against a target that does not.
    async fn probe_transactional(&self) -> Result<(), ProjectionError>;
    /// Human-readable description for logs (never credentials).
    fn describe(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projected_state_keeps_default_graph_triples_in_canonical_order() {
        let state: BTreeSet<Quad> = ["<urn:b> <urn:p> \"2\" .", "<urn:a> <urn:p> \"1\"@en ."]
            .iter()
            .map(|q| q.parse().unwrap())
            .collect();
        let projected = ProjectedState::from_state(&state).unwrap();
        assert_eq!(
            projected.triples(),
            ["<urn:a> <urn:p> \"1\"@en .", "<urn:b> <urn:p> \"2\" ."]
        );
        assert_eq!(projected.triple_count(), 2);
        assert_eq!(projected.digest(), &ledger_rdf::state_digest(&state));
        let empty = ProjectedState::from_state(&BTreeSet::new()).unwrap();
        assert_eq!(empty.triple_count(), 0);
    }

    #[test]
    fn named_graph_quads_are_refused_permanently() {
        let state: BTreeSet<Quad> = ["<urn:a> <urn:p> <urn:o> <urn:g> ."]
            .iter()
            .map(|q| q.parse().unwrap())
            .collect();
        let error = ProjectedState::from_state(&state).unwrap_err();
        assert_eq!(error.code(), ProjectionErrorCode::NamedGraphUnsupported);
        assert_eq!(error.class(), ErrorClass::Permanent);
    }
}
