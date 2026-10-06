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
/// Default byte budget of the detailed conflict report (2 MiB; see [`ReportLimits`]).
pub const DEFAULT_CONFLICT_REPORT_BYTES: usize = 2 * 1024 * 1024;
/// Smallest accepted byte budget: one conflict entry of ordinary terms with a few quads per
/// side fits; below this a report could not show even that.
pub const MIN_CONFLICT_REPORT_BYTES: usize = 1024;
/// Largest accepted byte budget (64 MiB, the default state-export cap).
pub const MAX_CONFLICT_REPORT_BYTES: usize = 64 * 1024 * 1024;
/// Bytes charged per listed conflict for its JSON framing: field names, braces, separators
/// and the framing of its three sides. At least the real framing (153 bytes with a `graph`
/// field and both `truncated` flags `false`), so the charge never undercounts.
pub const CONFLICT_ENTRY_FRAMING_BYTES: usize = 160;

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
    /// The detailed conflict report: a prefix, in ascending key order, bounded by the
    /// [`ReportLimits`] the merge ran with.
    pub conflicts: Vec<Conflict>,
    /// Total number of conflicting keys (reported or not); never affected by the limits.
    pub conflict_count: usize,
    /// The report-level limits (listed conflicts or bytes) left conflicts or quads out of
    /// `conflicts`. A side's own `truncated` flag also covers the per-side quad cap.
    pub conflicts_truncated: bool,
}

/// Bounds of the detailed conflict report: an operational setting, **never** part of the
/// merge. The merged state, `conflict_count`, the preview token and the candidate are the
/// same under any limits; only how much of the conflicts is listed differs.
///
/// The byte budget charges what the report costs as JSON (the API response shape): every
/// listed key term and quad as an escaped JSON string ([`json_string_len`]) plus a separator,
/// and [`CONFLICT_ENTRY_FRAMING_BYTES`] per listed conflict. The serialized `conflicts`
/// array is therefore at most the budget plus its two brackets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReportLimits {
    max_conflicts: usize,
    max_quads_per_side: usize,
    max_bytes: usize,
}

/// A byte budget outside `MIN_CONFLICT_REPORT_BYTES..=MAX_CONFLICT_REPORT_BYTES`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidReportLimit(pub usize);

impl std::fmt::Display for InvalidReportLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "conflict report budget {} is outside {MIN_CONFLICT_REPORT_BYTES}..={MAX_CONFLICT_REPORT_BYTES} bytes",
            self.0
        )
    }
}

impl std::error::Error for InvalidReportLimit {}

impl ReportLimits {
    /// 1 000 conflicts, 64 quads per side, [`DEFAULT_CONFLICT_REPORT_BYTES`].
    pub const DEFAULT: Self = Self {
        max_conflicts: MAX_REPORTED_CONFLICTS,
        max_quads_per_side: MAX_REPORTED_QUADS_PER_SIDE,
        max_bytes: DEFAULT_CONFLICT_REPORT_BYTES,
    };

    /// The default counts with the smallest byte budget (for callers that need only the
    /// merge and the count, never the details).
    pub const SMALLEST: Self = Self {
        max_conflicts: MAX_REPORTED_CONFLICTS,
        max_quads_per_side: MAX_REPORTED_QUADS_PER_SIDE,
        max_bytes: MIN_CONFLICT_REPORT_BYTES,
    };

    /// The default counts with another byte budget; a budget of zero, below the minimum or
    /// above the maximum is refused rather than silently clamped.
    pub fn with_max_bytes(max_bytes: usize) -> Result<Self, InvalidReportLimit> {
        if !(MIN_CONFLICT_REPORT_BYTES..=MAX_CONFLICT_REPORT_BYTES).contains(&max_bytes) {
            return Err(InvalidReportLimit(max_bytes));
        }
        Ok(Self {
            max_bytes,
            ..Self::DEFAULT
        })
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}

impl Default for ReportLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The length of `s` as a JSON string literal: quotes plus the escapes `serde_json` writes
/// (`\"` and `\\`, the short forms of backspace, form feed, newline, carriage return and
/// tab, `\u00XX` for every other control character; everything else verbatim).
pub fn json_string_len(s: &str) -> usize {
    2 + s
        .bytes()
        .map(|b| match b {
            b'"' | b'\\' | 0x08 | 0x0c | b'\n' | b'\r' | b'\t' => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
        .sum::<usize>()
}

/// Collects the detailed conflict report under [`ReportLimits`]. It only reads the slots the
/// merge already partitioned and stops cloning once a limit is reached, so the report costs
/// at most its budget however large the conflicting terms are. Conflicts are offered in
/// ascending key order and each side's quads in canonical order, so the report is a
/// deterministic prefix: (conflict, side base/target/source, quad).
struct Reporter {
    limits: ReportLimits,
    remaining: usize,
    conflicts: Vec<Conflict>,
    /// A report-level limit was hit; nothing further is listed.
    exhausted: bool,
}

impl Reporter {
    fn new(limits: ReportLimits) -> Self {
        Self {
            limits,
            remaining: limits.max_bytes,
            conflicts: Vec::new(),
            exhausted: false,
        }
    }

    fn charge(&mut self, cost: usize) -> bool {
        if cost > self.remaining {
            self.exhausted = true;
            return false;
        }
        self.remaining -= cost;
        true
    }

    fn offer(&mut self, key: &StructuralKey, slot: &Slot) {
        if self.exhausted {
            return;
        }
        if self.conflicts.len() == self.limits.max_conflicts {
            self.exhausted = true;
            return;
        }
        let key_cost = CONFLICT_ENTRY_FRAMING_BYTES
            + key.graph.as_deref().map_or(0, json_string_len)
            + json_string_len(&key.subject)
            + json_string_len(&key.predicate);
        if !self.charge(key_cost) {
            return;
        }
        let base = self.side(&slot.base);
        let target = self.side(&slot.target);
        let source = self.side(&slot.source);
        self.conflicts.push(Conflict {
            key: key.clone(),
            base,
            target,
            source,
        });
    }

    /// One side: whole quads while they fit (never a partial quad), then `truncated` if any
    /// quad of the side is not listed.
    fn side(&mut self, set: &BTreeSet<Quad>) -> Side {
        let mut quads = Vec::new();
        if !self.exhausted {
            for q in set.iter().take(self.limits.max_quads_per_side) {
                if !self.charge(json_string_len(q.as_str()) + 1) {
                    break;
                }
                quads.push(q.clone());
            }
        }
        Side {
            truncated: quads.len() < set.len(),
            quads,
        }
    }
}

#[derive(Default)]
struct Slot {
    base: BTreeSet<Quad>,
    target: BTreeSet<Quad>,
    source: BTreeSet<Quad>,
}

/// The three-way structural merge of `source` into `target` from `base` (module docs), with
/// the default conflict report limits.
pub fn three_way(
    base: &BTreeSet<Quad>,
    target: &BTreeSet<Quad>,
    source: &BTreeSet<Quad>,
    strategy: Strategy,
) -> ThreeWay {
    three_way_reported(base, target, source, strategy, ReportLimits::DEFAULT)
}

/// [`three_way`] with explicit conflict report limits. The limits bound only the detailed
/// report; the merged state and `conflict_count` are computed independently of them.
pub fn three_way_reported(
    base: &BTreeSet<Quad>,
    target: &BTreeSet<Quad>,
    source: &BTreeSet<Quad>,
    strategy: Strategy,
    report: ReportLimits,
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
    let mut reporter = Reporter::new(report);
    let mut conflict_count = 0;
    for (key, slot) in slots {
        let result = if slot.target == slot.base || slot.target == slot.source {
            slot.source.clone()
        } else if slot.source == slot.base {
            slot.target.clone()
        } else {
            conflict_count += 1;
            reporter.offer(&key, &slot);
            match strategy {
                Strategy::Abort => BTreeSet::new(),
                Strategy::TakeTarget => slot.target.clone(),
                Strategy::TakeSource => slot.source.clone(),
                Strategy::Union => slot.target.union(&slot.source).cloned().collect(),
            }
        };
        merged.extend(result);
    }
    // `exhausted` is set exactly when a limit left a conflict or a quad unlisted.
    ThreeWay {
        merged: (strategy != Strategy::Abort || conflict_count == 0).then_some(merged),
        conflicts_truncated: reporter.exhausted,
        conflicts: reporter.conflicts,
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
