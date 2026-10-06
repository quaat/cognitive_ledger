//! Deterministic three-way structural RDF merge and the merge preview-token v1 identity
//! (ADR-0023 / ADR-0024).
//!
//! Inputs are reconstructed states (`BTreeSet<Quad>`): base `B`, target `T`, source `S`.
//! Quads are partitioned by their structural key `(graph, subject, predicate)`; for each key
//! `k` (writing `X|k` for the quads of `X` with key `k`):
//!
//! - `T|k = B|k`  → `S|k` (only the source changed the slot, or nobody did);
//! - `S|k = B|k`  → `T|k` (only the target changed it);
//! - `T|k = S|k`  → `T|k` (both made the same change: convergent, not a conflict);
//! - otherwise the key **conflicts** and only the strategy decides: `abort` (no merged
//!   state), `take-target` (`T|k`), `take-source` (`S|k`), `union` (`T|k ∪ S|k`, RDF set
//!   union of both sides' results — set semantics only, never "semantically acceptable").
//!
//! The merged state is the union over all keys. Everything is a pure function of the three
//! sets: ordered collections only, no hash iteration, no clocks, no I/O. Semantic adequacy is
//! Sculpin's job; this crate never resolves anything by time, confidence, ontology or AI.
//!
//! The crate is infrastructure-free and synchronous: the store reconstructs states and runs
//! ancestry (`ledger-dag`); this crate only computes.

use ledger_core::{CommitId, GraphId};
use ledger_rdf::{Quad, StructuralKey, diff, state_digest};
use std::collections::{BTreeMap, BTreeSet};

/// The merge semantics implemented here; recorded with every merge (merge row and preview
/// token) so a later explanation can recompute exactly the same per-key comparison.
pub const MERGE_ALGORITHM_V1: &str = "structural-slot/v1";

/// At most this many conflicting keys are reported in detail (the total is always given).
pub const MAX_REPORTED_CONFLICTS: usize = 1_000;
/// At most this many quads per side of a reported conflict (each side flags truncation).
pub const MAX_REPORTED_QUADS_PER_SIDE: usize = 64;

/// Conflict resolution for structurally conflicting keys.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Strategy {
    Abort,
    TakeTarget,
    TakeSource,
    Union,
}

impl Strategy {
    /// Canonical byte for the preview token (ADR-0024).
    pub fn code(self) -> u8 {
        match self {
            Self::Abort => 0,
            Self::TakeTarget => 1,
            Self::TakeSource => 2,
            Self::Union => 3,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::TakeTarget => "take-target",
            Self::TakeSource => "take-source",
            Self::Union => "union",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "abort" => Self::Abort,
            "take-target" => Self::TakeTarget,
            "take-source" => Self::TakeSource,
            "union" => Self::Union,
            _ => return None,
        })
    }
}

/// One side of a reported conflict, bounded.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Side {
    pub quads: Vec<Quad>,
    /// More quads exist than are listed.
    pub truncated: bool,
}

fn side(set: &BTreeSet<Quad>) -> Side {
    Side {
        quads: set
            .iter()
            .take(MAX_REPORTED_QUADS_PER_SIDE)
            .cloned()
            .collect(),
        truncated: set.len() > MAX_REPORTED_QUADS_PER_SIDE,
    }
}

/// A structurally conflicting key: both sides changed the slot, differently.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Conflict {
    pub key: StructuralKey,
    pub base: Side,
    pub target: Side,
    pub source: Side,
}

/// The three-way merge result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreeWay {
    /// The merged state; `None` only for `abort` with at least one conflict.
    pub merged: Option<BTreeSet<Quad>>,
    /// Conflicting keys in ascending key order, at most [`MAX_REPORTED_CONFLICTS`].
    pub conflicts: Vec<Conflict>,
    /// Total number of conflicting keys (reported or not).
    pub conflict_count: usize,
}

#[derive(Default)]
struct Slot {
    base: BTreeSet<Quad>,
    target: BTreeSet<Quad>,
    source: BTreeSet<Quad>,
}

/// The three-way structural merge of `source` into `target` from `base` (module docs).
pub fn three_way(
    base: &BTreeSet<Quad>,
    target: &BTreeSet<Quad>,
    source: &BTreeSet<Quad>,
    strategy: Strategy,
) -> ThreeWay {
    let mut slots: BTreeMap<StructuralKey, Slot> = BTreeMap::new();
    for q in base {
        slots
            .entry(q.structural_key())
            .or_default()
            .base
            .insert(q.clone());
    }
    for q in target {
        slots
            .entry(q.structural_key())
            .or_default()
            .target
            .insert(q.clone());
    }
    for q in source {
        slots
            .entry(q.structural_key())
            .or_default()
            .source
            .insert(q.clone());
    }
    let mut merged = BTreeSet::new();
    let mut conflicts = Vec::new();
    let mut conflict_count = 0;
    for (key, slot) in slots {
        let result = if slot.target == slot.base || slot.target == slot.source {
            slot.source.clone()
        } else if slot.source == slot.base {
            slot.target.clone()
        } else {
            conflict_count += 1;
            if conflicts.len() < MAX_REPORTED_CONFLICTS {
                conflicts.push(Conflict {
                    key: key.clone(),
                    base: side(&slot.base),
                    target: side(&slot.target),
                    source: side(&slot.source),
                });
            }
            match strategy {
                Strategy::Abort => BTreeSet::new(),
                Strategy::TakeTarget => slot.target.clone(),
                Strategy::TakeSource => slot.source.clone(),
                Strategy::Union => slot.target.union(&slot.source).cloned().collect(),
            }
        };
        merged.extend(result);
    }
    ThreeWay {
        merged: (strategy != Strategy::Abort || conflict_count == 0).then_some(merged),
        conflicts,
        conflict_count,
    }
}

/// Merge classification for the preview token (ADR-0023). `AlreadyEqual`,
/// `AlreadyContained` and `NoChange` produce nothing and have no token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Classification {
    FastForward,
    Divergent,
}

impl Classification {
    pub fn code(self) -> u8 {
        match self {
            Self::FastForward => 1,
            Self::Divergent => 2,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FastForward => "fast_forward",
            Self::Divergent => "divergent",
        }
    }
}

/// Everything a merge preview binds (ADR-0024 "Preview token v1").
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewIdentity {
    pub graph: GraphId,
    pub source_branch: String,
    pub source_head: CommitId,
    pub target_branch: String,
    pub target_head: CommitId,
    pub merge_base: CommitId,
    pub classification: Classification,
    /// Normalized: a fast-forward has no conflicts, so its strategy is always `abort`.
    pub strategy: Strategy,
    pub merged_state_digest: ledger_core::ContentId,
}

/// Header of the canonical preview-token bytes.
pub const PREVIEW_TOKEN_V1_HEADER: &[u8] = b"sculpin-ledger-merge-preview/v1\0";

fn field(out: &mut Vec<u8>, value: &str) {
    let len = u32::try_from(value.len()).expect("bounded identifiers");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

impl PreviewIdentity {
    /// The canonical bytes (ADR-0024): header, then length-prefixed fields and `u8` enums in
    /// a fixed order. The merge algorithm id is bound so a token can never confirm a merge
    /// computed under other semantics.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = PREVIEW_TOKEN_V1_HEADER.to_vec();
        field(&mut out, self.graph.as_str());
        field(&mut out, &self.source_branch);
        field(&mut out, &self.source_head.to_string());
        field(&mut out, &self.target_branch);
        field(&mut out, &self.target_head.to_string());
        field(&mut out, &self.merge_base.to_string());
        out.push(self.classification.code());
        let strategy = match self.classification {
            Classification::FastForward => Strategy::Abort,
            Classification::Divergent => self.strategy,
        };
        out.push(strategy.code());
        field(&mut out, MERGE_ALGORITHM_V1);
        field(&mut out, &self.merged_state_digest.to_string());
        out
    }

    /// `sha256:<hex>` over the canonical bytes: a confirmation digest of what the client
    /// previewed (recomputable from persisted rows on any replica), not an authenticator.
    pub fn token(&self) -> String {
        ledger_core::ContentId::for_bytes(&self.canonical_bytes()).to_string()
    }
}

/// The merged state's digest (`sculpin-rdf-state/v1`, ADR-0018).
pub fn merged_state_digest(merged: &BTreeSet<Quad>) -> ledger_core::ContentId {
    state_digest(merged)
}

/// Whether integrating `merged` into a target whose state is `target` changes nothing
/// (ADR-0023 amendment: such a merge creates no commit, which keeps two-way syncing from
/// producing endless empty integrations and respects ADR-0008's no-empty-commit rule).
pub fn is_no_change(target: &BTreeSet<Quad>, merged: &BTreeSet<Quad>) -> bool {
    diff(target, merged).is_empty()
}

#[cfg(test)]
mod tests;
