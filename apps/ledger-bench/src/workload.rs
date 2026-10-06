//! The dataset-independent workload a dataset adapter produces and the runner executes
//! against the ledger: an ordered list of steps (commits, branch creations, merges) plus
//! the oracle's expectations, keyed by symbolic labels. Commit ids are content-addressed
//! and only known once the ledger creates them; the runner maps labels to ids as it goes.
//!
//! Statements are canonical N-Quads lines (the form the ledger returns), so any dataset —
//! generated or extracted (BEAR versions, temporal events rendered as quads) — fits the
//! same representation. Nothing here knows how the ledger computes anything.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub type Label = String;
pub type State = Arc<BTreeSet<String>>;

/// Provenance the dataset attaches to a generated change; the runner checks that the
/// persisted commit carries exactly this (persisted-state assertion).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Provenance {
    pub activity: String,
    pub message: String,
    pub evidence_refs: Vec<String>,
    pub source_system: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CommitStep {
    pub label: Label,
    pub branch: String,
    /// `None` only for the genesis commit of `main`.
    pub parent: Option<Label>,
    pub adds: Vec<String>,
    pub deletes: Vec<String>,
    pub provenance: Provenance,
}

/// Expectations of one merge preview (one strategy).
#[derive(Clone, Debug, Serialize)]
pub struct PreviewExpect {
    pub strategy: &'static str,
    /// An explicit merge base sent with the request (it must be a best common ancestor).
    pub explicit_base: Option<Label>,
    pub classification: &'static str,
    /// For `ambiguous_merge_base`: every best common ancestor.
    pub base_candidates: Vec<Label>,
    pub merge_base: Option<Label>,
    pub ahead: usize,
    pub behind: usize,
    pub conflict_count: usize,
    /// `(graph, subject, predicate)` of every designed conflict, ascending.
    pub conflict_keys: Vec<(Option<String>, String, String)>,
    /// `(adds, deletes, affected keys)` of base → target and base → source.
    pub target_delta: Option<(usize, usize, usize)>,
    pub source_delta: Option<(usize, usize, usize)>,
    /// The merged state when a candidate results (its protocol digest is compared with the
    /// preview's `merged_state_digest`).
    #[serde(skip)]
    pub merged: Option<State>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MergeStep {
    pub id: String,
    pub source: String,
    pub target: String,
    /// Previews to run, in order; each must match its expectation.
    pub previews: Vec<PreviewExpect>,
    /// Apply the preview with this strategy (must be a candidate class); the integration
    /// commit gets `label`.
    pub apply: Option<ApplyStep>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyStep {
    pub strategy: &'static str,
    /// The explicit base of the applied preview, if any.
    pub explicit_base: Option<Label>,
    pub label: Label,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, Serialize)]
pub enum Step {
    Commit(CommitStep),
    /// Create `name` at the commit `from` (a historical branch point when it is not the
    /// source head).
    Branch {
        name: String,
        source: String,
        from: Label,
    },
    Merge(MergeStep),
}

/// What the oracle knows about one commit.
#[derive(Clone, Debug)]
pub struct Expected {
    pub parents: Vec<Label>,
    /// Parent-0 chain length from genesis (genesis = 0): what reconstruction folds.
    pub depth: u32,
    /// Patch operations (adds + deletes) along the parent-0 chain up to and including this
    /// commit: the logical work a reconstruction folds.
    pub fold_ops: u64,
    pub quads: usize,
    /// [`oracle_digest`] of the state.
    pub digest: String,
    /// The full state, retained for selected commits (diffs, merge inputs, samples).
    pub state: Option<State>,
    /// Reporting class of the history the commit is on (`main`, `feature`, `growth`, ...).
    pub kind: &'static str,
    pub provenance: Provenance,
}

/// A statement's membership at version boundaries (appearing, disappearing, reappearing),
/// checked against ledger-materialized states.
#[derive(Clone, Debug, Serialize)]
pub struct HistoryFact {
    pub quad: String,
    pub present: Vec<Label>,
    pub absent: Vec<Label>,
}

/// A complete dataset workload.
#[derive(Clone, Debug)]
pub struct Workload {
    pub steps: Vec<Step>,
    pub expected: BTreeMap<Label, Expected>,
    /// Branch heads after the last step.
    pub final_heads: BTreeMap<String, Label>,
    /// Pairs `(a, b)` whose ledger diff is compared with the oracle (both retained).
    pub diff_pairs: Vec<(Label, Label)>,
    /// Reconstruct every commit again after the run (otherwise only retained ones).
    pub verify_all_history: bool,
    pub history_facts: Vec<HistoryFact>,
}

impl Workload {
    pub fn commit_count(&self) -> usize {
        self.expected.len()
    }
}

/// The oracle's state fingerprint: SHA-256 over the sorted lines, each followed by `\n`.
/// Deliberately not the ledger's `sculpin-rdf-state/v1` digest, so this comparison does not
/// depend on the ledger's own digest code.
pub fn oracle_digest<'a>(sorted_lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut h = Sha256::new();
    for line in sorted_lines {
        h.update(line.as_bytes());
        h.update(b"\n");
    }
    hex::encode(h.finalize())
}

/// A deterministic fingerprint of a whole workload (the dataset checksum recorded in its
/// manifest): every step and every expectation in order, as canonical JSON lines.
pub fn workload_checksum(w: &Workload) -> String {
    let mut h = Sha256::new();
    for step in &w.steps {
        h.update(serde_json::to_vec(step).expect("serializable step"));
        h.update(b"\n");
        if let Step::Merge(m) = step {
            for p in &m.previews {
                if let Some(merged) = &p.merged {
                    h.update(oracle_digest(merged.iter().map(String::as_str)).as_bytes());
                }
                h.update(b"\n");
            }
        }
    }
    for (label, e) in &w.expected {
        h.update(
            format!(
                "{label}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                e.parents.join(","),
                e.depth,
                e.fold_ops,
                e.quads,
                e.digest,
                e.kind
            )
            .as_bytes(),
        );
        h.update(serde_json::to_vec(&e.provenance).expect("serializable provenance"));
        h.update(b"\n");
    }
    for (b, l) in &w.final_heads {
        h.update(format!("head {b} {l}\n").as_bytes());
    }
    for (a, b) in &w.diff_pairs {
        h.update(format!("diff {a} {b}\n").as_bytes());
    }
    for f in &w.history_facts {
        h.update(serde_json::to_vec(f).expect("serializable history fact"));
        h.update(b"\n");
    }
    format!("sha256:{}", hex::encode(h.finalize()))
}

/// The parent-0 chain from `head` (head first), as the oracle knows it.
pub fn first_parent_chain(w: &Workload, head: &Label) -> Vec<Label> {
    let mut out = vec![head.clone()];
    let mut cur = head;
    while let Some(p) = w.expected.get(cur).and_then(|e| e.parents.first()) {
        out.push(p.clone());
        cur = p;
    }
    out
}
