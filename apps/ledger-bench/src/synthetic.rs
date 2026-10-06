//! `synthetic-ledger-*`: a deterministic, seeded generator of a Cognitive Ledger history
//! **and** the independent oracle of that history. The history covers:
//! - linear, forked, diamond, merge-heavy and criss-cross shapes;
//! - additions, deletions and replacements;
//! - designed structural conflicts of five shapes;
//! - convergent changes, a merge base reachable only through a second parent, and both
//!   `NO_CHANGE` directions.
//!
//! The oracle never calls the ledger's crates. It knows every state because it generated
//! every change, and merge outcomes by construction:
//! - **States.** `state(child) = state(parent) − deletes + adds`, as set algebra over
//!   generated statements.
//! - **Ancestry, merge base and ahead/behind.** A brute-force reference over the symbolic
//!   DAG: ancestor sets, and the maximal elements of their intersection. This is the ADR-0024
//!   definition, not `ledger-dag`'s traversal. Several maximal elements mean an ambiguous
//!   base.
//! - **Merges** (ADR-0023/0024 `structural-slot/v1`). Every scenario is *designed*: target
//!   and source touch disjoint `(graph, subject, predicate)` slots, except the slots it puts
//!   in conflict on purpose and those it makes convergent on purpose.
//!   - Outside the designed conflicts, the merged state is the three-way set formula
//!     `(T − (B − S)) ∪ (S − B)`.
//!   - Inside them, the strategy's definition applies: `T|k`, `S|k` or `T|k ∪ S|k`.
//!
//!   The premise is checked whenever a merge is generated: any slot changed differently on
//!   both sides that was not designed is a generator bug and panics. It is never folded
//!   into the expectation.
//! - **`NO_CHANGE`.** The merged state equals T **and** the source has no net change from B.
//!
//! The five conflict shapes, cycled over the designed slots, are chosen so that a ledger
//! implementing anything other than ADR-0024's slot rules is caught:
//! 1. replace / replace;
//! 2. delete (target) / modify (source);
//! 3. modify (target) / delete (source);
//! 4. keep-and-add (target) / delete (source). `union` must *keep* the statement the
//!    source deleted, so a delete-honouring merge fails;
//! 5. add / add of different values to a slot both sides keep (multi-valued).
//!
//! Determinism: one SplitMix64 stream from the seed; only ordered collections; no clocks.

use crate::workload::{
    ApplyStep, CommitStep, Expected, Label, MergeStep, PreviewExpect, Provenance, State, Step,
    Workload, oracle_digest,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    sync::Arc,
};

/// Bumped whenever generation changes; recorded with the dataset checksum in its manifest.
pub const GENERATOR_VERSION: &str = "synthetic-ledger-gen/3";

/// Size and shape parameters of a synthetic profile.
#[derive(Clone, Debug, Serialize)]
pub struct Params {
    /// Recorded as a hex string at the manifest's top level (a JSON number would exceed
    /// 2^53).
    #[serde(skip)]
    pub seed: u64,
    /// Entities present at genesis (each with `predicates` statements).
    pub entities: u32,
    pub predicates: u32,
    /// Entities that also carry a statement in the named graph `<urn:bench:g:1>`.
    pub meta_entities: u32,
    /// `main` commits before the feature branches fork, after, and after all merges.
    pub main_pre: u32,
    pub main_mid: u32,
    pub main_post: u32,
    /// Commits on each of `feature/a` and `feature/b` (forked together: a diamond).
    pub feature: u32,
    /// Add-only history (growing state) and replace-only history (constant state).
    pub growth: u32,
    pub churn: u32,
    /// Commits on `feature/e` (merged as a fast-forward class).
    pub fast: u32,
    /// Commits on each of `feature/c` and `feature/d` (merged with designed conflicts).
    pub contested: u32,
    /// Designed conflicting slots, cycling through the five shapes (module docs).
    pub conflicts: u32,
    /// Slots both sides change identically (not conflicts).
    pub convergent: u32,
    /// Retain the full expected state of every n-th commit of each branch (diffs, samples).
    pub sample_every: u32,
    /// Reconstruct every commit again at the end (else only retained ones).
    pub verify_all_history: bool,
}

impl Params {
    /// The PR-CI profile (Plan 0010 targets: ~1,000 entities, ~6,000 initial statements,
    /// ~200 commits, 4–8 work branches, ≥ 10 applied merges, ≥ 5 designed conflicts).
    pub fn ci() -> Self {
        Self {
            seed: 0x5eed_0001_c1ed_6e00,
            entities: 1_000,
            predicates: 6,
            meta_entities: 100,
            main_pre: 20,
            main_mid: 20,
            main_post: 10,
            feature: 15,
            growth: 35,
            churn: 35,
            fast: 6,
            contested: 10,
            conflicts: 10,
            convergent: 2,
            sample_every: 10,
            verify_all_history: true,
        }
    }

    /// A deeper local profile for baselines (≈1,500 commits; parent-0 depth up to ≈600).
    pub fn local() -> Self {
        Self {
            seed: 0x5eed_0001_10ca_1000,
            entities: 2_000,
            predicates: 6,
            meta_entities: 200,
            main_pre: 200,
            main_mid: 200,
            main_post: 200,
            feature: 100,
            growth: 300,
            churn: 300,
            fast: 20,
            contested: 40,
            conflicts: 20,
            convergent: 4,
            sample_every: 50,
            verify_all_history: false,
        }
    }
}

// ---- deterministic randomness --------------------------------------------------------

struct Rng(u64);

impl Rng {
    /// SplitMix64 (public-domain reference constants).
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0);
        self.next() % n
    }
}

// ---- compact statements ----------------------------------------------------------------
//
// A statement is a u64: graph (2 bits) | subject (22) | predicate (6) | object kind (1) |
// object (33). Its slot is the top 30 bits (graph, subject, predicate). Rendered as
// canonical N-Quads with plain-string literals and `urn:` IRIs, the only forms used, so
// the rendering is the canonical form the ledger returns.

type Stmt = u64;

const MAX_ENTITY: u64 = 1 << 22;
const MAX_VALUE: u64 = 1 << 33;
/// Statements per bulk-load commit (well below the API's 10,000 operations per request;
/// limits are never raised for benchmarks).
const BULK_ADDS: usize = 5_000;

fn stmt(graph: u64, subject: u64, predicate: u64, literal: bool, object: u64) -> Stmt {
    assert!(graph < 4 && subject < MAX_ENTITY && predicate < 64 && object < MAX_VALUE);
    graph << 62 | subject << 40 | predicate << 34 | u64::from(literal) << 33 | object
}

fn slot(s: Stmt) -> u64 {
    s >> 34
}

fn graph_of(s: Stmt) -> u64 {
    s >> 62
}

fn subject_of(s: Stmt) -> u64 {
    (s >> 40) & (MAX_ENTITY - 1)
}

fn predicate_of(s: Stmt) -> u64 {
    (s >> 34) & 63
}

fn render(s: Stmt) -> String {
    let object = s & (MAX_VALUE - 1);
    let o = if (s >> 33) & 1 == 1 {
        format!("\"v{object}\"")
    } else {
        format!("<urn:bench:e:{object}>")
    };
    let g = match graph_of(s) {
        0 => String::new(),
        g => format!("<urn:bench:g:{g}> "),
    };
    format!(
        "<urn:bench:e:{}> <urn:bench:p:{}> {o} {g}.",
        subject_of(s),
        predicate_of(s)
    )
}

fn render_slot(k: u64) -> (Option<String>, String, String) {
    let s = k << 34;
    (
        match graph_of(s) {
            0 => None,
            g => Some(format!("<urn:bench:g:{g}>")),
        },
        format!("<urn:bench:e:{}>", subject_of(s)),
        format!("<urn:bench:p:{}>", predicate_of(s)),
    )
}

fn render_state(state: &BTreeSet<Stmt>) -> BTreeSet<String> {
    state.iter().map(|s| render(*s)).collect()
}

fn slots(state: &BTreeSet<Stmt>) -> BTreeMap<u64, BTreeSet<Stmt>> {
    let mut out: BTreeMap<u64, BTreeSet<Stmt>> = BTreeMap::new();
    for s in state {
        out.entry(slot(*s)).or_default().insert(*s);
    }
    out
}

/// `(adds, deletes, affected slots)` of `from → to`.
fn summary(from: &BTreeSet<Stmt>, to: &BTreeSet<Stmt>) -> (usize, usize, usize) {
    let adds: Vec<&Stmt> = to.difference(from).collect();
    let deletes: Vec<&Stmt> = from.difference(to).collect();
    let keys: BTreeSet<u64> = adds.iter().chain(&deletes).map(|s| slot(**s)).collect();
    (adds.len(), deletes.len(), keys.len())
}

// ---- generator ----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Strategy {
    Abort,
    TakeTarget,
    TakeSource,
    Union,
}

impl Strategy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::TakeTarget => "take-target",
            Self::TakeSource => "take-source",
            Self::Union => "union",
        }
    }
}

struct Gen {
    p: Params,
    rng: Rng,
    next_value: u64,
    next_entity: u64,
    next_change: u64,
    heads: BTreeMap<String, (Label, BTreeSet<Stmt>)>,
    counters: BTreeMap<String, u32>,
    parents: BTreeMap<Label, Vec<Label>>,
    depth: BTreeMap<Label, u32>,
    fold_ops: BTreeMap<Label, u64>,
    retained: BTreeMap<Label, Arc<BTreeSet<Stmt>>>,
    plan_retain: BTreeSet<Label>,
    ancestors: BTreeMap<Label, Arc<BTreeSet<Label>>>,
    steps: Vec<Step>,
    expected: BTreeMap<Label, Expected>,
    merges: u32,
}

fn kind_of(branch: &str) -> &'static str {
    match branch {
        "main" => "main",
        "growth" => "growth",
        "churn" => "churn",
        _ => "feature",
    }
}

/// Named subject pools (each branch only touches its own; scenario slots have their own).
const POOLS: [&str; 10] = [
    "main",
    "feature/a",
    "feature/b",
    "feature/c",
    "feature/d",
    "contested",
    "feature/e",
    "churn",
    "resolve",
    "netzero",
];

impl Gen {
    fn new(p: Params) -> Self {
        let g = Self {
            rng: Rng(p.seed),
            p,
            next_value: 0,
            next_entity: 0,
            next_change: 0,
            heads: BTreeMap::new(),
            counters: BTreeMap::new(),
            parents: BTreeMap::new(),
            depth: BTreeMap::new(),
            fold_ops: BTreeMap::new(),
            retained: BTreeMap::new(),
            plan_retain: BTreeSet::new(),
            ancestors: BTreeMap::new(),
            steps: Vec::new(),
            expected: BTreeMap::new(),
            merges: 0,
        };
        // The pools must be non-empty, within the genesis entities and pairwise disjoint,
        // or the "branches touch disjoint slots" premise would not hold by construction.
        let ranges: Vec<Range<u64>> = POOLS.iter().map(|n| g.pool(n)).collect();
        for (i, a) in ranges.iter().enumerate() {
            assert!(
                a.start < a.end && a.end <= u64::from(g.p.entities),
                "pool {}",
                POOLS[i]
            );
            for (j, b) in ranges.iter().enumerate().skip(i + 1) {
                assert!(
                    a.end <= b.start || b.end <= a.start,
                    "pools {} and {} overlap",
                    POOLS[i],
                    POOLS[j]
                );
            }
        }
        g
    }

    fn fresh(&mut self) -> u64 {
        self.next_value += 1;
        self.next_value
    }

    fn pool(&self, name: &str) -> Range<u64> {
        let e = u64::from(self.p.entities);
        let contested = u64::from(self.p.conflicts + self.p.convergent);
        let at = |f: u64| e * f / 100;
        match name {
            "main" => 0..at(20),
            "feature/a" => at(20)..at(30),
            "feature/b" => at(30)..at(40),
            "feature/c" => at(40)..at(45),
            "feature/d" => at(45)..at(50),
            "contested" => at(50)..at(50) + contested,
            "feature/e" => at(52)..at(56),
            "churn" => at(56)..at(70),
            "resolve" => at(70)..at(70) + 2,
            "netzero" => at(72)..at(72) + 2,
            other => panic!("no pool for {other}"),
        }
    }

    fn provenance(&mut self, label: &str) -> Provenance {
        self.next_change += 1;
        Provenance {
            activity: "benchmark-synthetic".into(),
            message: label.to_owned(),
            evidence_refs: vec![format!("urn:bench:change:{}", self.next_change)],
            source_system: Some("ledger-bench".into()),
        }
    }

    /// Record the expectation of a new commit whose patch has `ops` operations (and retain
    /// its full state if planned).
    fn record(
        &mut self,
        label: &Label,
        parents: Vec<Label>,
        state: &BTreeSet<Stmt>,
        ops: u64,
        kind: &'static str,
        provenance: Provenance,
    ) {
        let depth = parents.first().map_or(0, |p| self.depth[p] + 1);
        let fold_ops = parents.first().map_or(0, |p| self.fold_ops[p]) + ops;
        let rendered = render_state(state);
        let digest = oracle_digest(rendered.iter().map(String::as_str));
        self.depth.insert(label.clone(), depth);
        self.fold_ops.insert(label.clone(), fold_ops);
        self.parents.insert(label.clone(), parents.clone());
        self.expected.insert(
            label.clone(),
            Expected {
                parents,
                depth,
                fold_ops,
                quads: rendered.len(),
                digest,
                state: None,
                kind,
                provenance,
            },
        );
        if self.plan_retain.contains(label) {
            self.retain_rendered(label, state, rendered);
        }
    }

    fn retain(&mut self, label: &Label, state: &BTreeSet<Stmt>) {
        if self.retained.contains_key(label) {
            return;
        }
        let rendered = render_state(state);
        self.retain_rendered(label, state, rendered);
    }

    fn retain_head(&mut self, branch: &str) -> Label {
        let (label, state) = self.heads[branch].clone();
        self.retain(&label, &state);
        label
    }

    fn retain_rendered(
        &mut self,
        label: &Label,
        state: &BTreeSet<Stmt>,
        rendered: BTreeSet<String>,
    ) {
        self.retained.insert(label.clone(), Arc::new(state.clone()));
        self.expected.get_mut(label).expect("recorded").state = Some(Arc::new(rendered));
    }

    /// The initial load, split into bulk commits of at most `BULK_ADDS` statements labelled
    /// `main@0.1`, `main@0.2`, …; the last one is `main@0`, the loaded state.
    fn genesis(&mut self) -> Label {
        let mut state = BTreeSet::new();
        for s in 0..u64::from(self.p.entities) {
            for p in 0..u64::from(self.p.predicates) {
                let v = self.fresh();
                state.insert(stmt(0, s, p, true, v));
            }
        }
        for s in 0..u64::from(self.p.meta_entities) {
            let v = self.fresh();
            state.insert(stmt(1, s, u64::from(self.p.predicates), true, v));
        }
        self.next_entity = u64::from(self.p.entities);
        let all: Vec<Stmt> = state.iter().copied().collect();
        let chunks: Vec<&[Stmt]> = all.chunks(BULK_ADDS).collect();
        let mut loaded = BTreeSet::new();
        let mut parent: Option<Label> = None;
        for (i, chunk) in chunks.iter().enumerate() {
            let label: Label = if i + 1 == chunks.len() {
                "main@0".into()
            } else {
                format!("main@0.{}", i + 1)
            };
            let adds: BTreeSet<Stmt> = chunk.iter().copied().collect();
            loaded.extend(adds.iter().copied());
            let provenance = self.provenance(&label);
            self.steps.push(Step::Commit(CommitStep {
                label: label.clone(),
                branch: "main".into(),
                parent: parent.clone(),
                adds: render_state(&adds).into_iter().collect(),
                deletes: Vec::new(),
                provenance: provenance.clone(),
            }));
            self.plan_retain.insert(label.clone());
            let ops = adds.len() as u64;
            self.record(
                &label,
                parent.iter().cloned().collect(),
                &loaded,
                ops,
                "bulk",
                provenance,
            );
            parent = Some(label);
        }
        let label = parent.expect("at least one bulk commit");
        self.counters.insert("main".into(), 0);
        self.heads.insert("main".into(), (label.clone(), state));
        label
    }

    fn commit(&mut self, branch: &str, adds: BTreeSet<Stmt>, deletes: BTreeSet<Stmt>) -> Label {
        let (parent, state) = self.heads[branch].clone();
        assert!(!adds.is_empty() || !deletes.is_empty(), "empty commit");
        assert!(deletes.is_subset(&state), "deletes must exist");
        assert!(adds.is_disjoint(&state), "adds must be new");
        let mut next = state;
        for d in &deletes {
            next.remove(d);
        }
        next.extend(adds.iter().copied());
        let n = self.counters[branch] + 1;
        self.counters.insert(branch.to_owned(), n);
        let label = format!("{branch}@{n}");
        if n % self.p.sample_every == 0 {
            self.plan_retain.insert(label.clone());
        }
        let provenance = self.provenance(&label);
        self.steps.push(Step::Commit(CommitStep {
            label: label.clone(),
            branch: branch.to_owned(),
            parent: Some(parent.clone()),
            adds: render_state(&adds).into_iter().collect(),
            deletes: render_state(&deletes).into_iter().collect(),
            provenance: provenance.clone(),
        }));
        let ops = (adds.len() + deletes.len()) as u64;
        self.record(
            &label,
            vec![parent],
            &next,
            ops,
            kind_of(branch),
            provenance,
        );
        self.heads.insert(branch.to_owned(), (label.clone(), next));
        label
    }

    /// Existing statements whose subject lies in `pool`, in either graph, not in `except`.
    fn existing(
        &self,
        state: &BTreeSet<Stmt>,
        pool: &Range<u64>,
        except: &BTreeSet<Stmt>,
    ) -> Vec<Stmt> {
        let mut out = Vec::new();
        for g in 0..2u64 {
            let lo = stmt(g, pool.start, 0, false, 0);
            let hi = stmt(g, pool.end.min(MAX_ENTITY - 1), 0, false, 0);
            out.extend(state.range(lo..hi).filter(|s| !except.contains(s)));
        }
        out
    }

    /// A mixed change (add, delete, replace) confined to `pool`, committed on `branch`.
    fn mixed_commit_in(&mut self, branch: &str, pool_name: &str) -> Label {
        let pool = self.pool(pool_name);
        let state = self.heads[branch].1.clone();
        let (mut adds, mut deletes) = (BTreeSet::new(), BTreeSet::new());
        let ops = 1 + self.rng.below(5);
        for _ in 0..ops {
            let kind = self.rng.below(3);
            if kind == 0 {
                let s = pool.start + self.rng.below(pool.end - pool.start);
                let meta = s < u64::from(self.p.meta_entities) && self.rng.below(4) == 0;
                let (g, p) = if meta {
                    (1, u64::from(self.p.predicates))
                } else {
                    (0, self.rng.below(u64::from(self.p.predicates)))
                };
                let v = self.fresh();
                adds.insert(stmt(g, s, p, true, v));
                continue;
            }
            let candidates = self.existing(&state, &pool, &deletes);
            if candidates.is_empty() {
                continue;
            }
            let victim = candidates[self.rng.below(candidates.len() as u64) as usize];
            deletes.insert(victim);
            if kind == 2 {
                let v = self.fresh();
                adds.insert(stmt(
                    graph_of(victim),
                    subject_of(victim),
                    predicate_of(victim),
                    true,
                    v,
                ));
            }
        }
        if adds.is_empty() && deletes.is_empty() {
            let s = pool.start;
            let v = self.fresh();
            adds.insert(stmt(0, s, 0, true, v));
        }
        self.commit(branch, adds, deletes)
    }

    fn mixed_commit(&mut self, branch: &str) -> Label {
        self.mixed_commit_in(branch, branch)
    }

    /// Add-only: a new entity with 2–5 statements (one links to an existing entity).
    fn growth_commit(&mut self) -> Label {
        let s = self.next_entity;
        self.next_entity += 1;
        let mut adds = BTreeSet::new();
        let n = 2 + self.rng.below(4);
        for p in 0..n {
            let v = self.fresh();
            adds.insert(stmt(0, s, p, true, v));
        }
        let target = self.rng.below(u64::from(self.p.entities));
        adds.insert(stmt(0, s, u64::from(self.p.predicates) - 1, false, target));
        self.commit("growth", adds, BTreeSet::new())
    }

    /// Replace-only: 1–3 values change; the state size stays constant.
    fn churn_commit(&mut self) -> Label {
        let pool = self.pool("churn");
        let state = self.heads["churn"].1.clone();
        let (mut adds, mut deletes) = (BTreeSet::new(), BTreeSet::new());
        for _ in 0..1 + self.rng.below(3) {
            let candidates = self.existing(&state, &pool, &deletes);
            let victim = candidates[self.rng.below(candidates.len() as u64) as usize];
            deletes.insert(victim);
            let v = self.fresh();
            adds.insert(stmt(
                graph_of(victim),
                subject_of(victim),
                predicate_of(victim),
                true,
                v,
            ));
        }
        self.commit("churn", adds, deletes)
    }

    /// Create `name` at `from`, which must be the head of `source` or reachable from it (the
    /// Phase-4 branch-point rule; asserted here so a scenario cannot ask for a refusal).
    fn branch(&mut self, name: &str, source: &str, from: &Label) {
        let head = self.heads[source].0.clone();
        assert!(
            self.ancestors_of(&head).contains(from),
            "branch point {from} is not reachable from {source}"
        );
        let state = self
            .retained
            .get(from)
            .unwrap_or_else(|| panic!("branch point {from} must be retained"))
            .as_ref()
            .clone();
        self.steps.push(Step::Branch {
            name: name.to_owned(),
            source: source.to_owned(),
            from: from.clone(),
        });
        self.counters.entry(name.to_owned()).or_insert(0);
        self.heads.insert(name.to_owned(), (from.clone(), state));
    }

    fn ancestors_of(&mut self, label: &Label) -> Arc<BTreeSet<Label>> {
        if let Some(a) = self.ancestors.get(label) {
            return a.clone();
        }
        let mut set = BTreeSet::from([label.clone()]);
        for p in self.parents[label].clone() {
            set.extend(self.ancestors_of(&p).iter().cloned());
        }
        let set = Arc::new(set);
        self.ancestors.insert(label.clone(), set.clone());
        set
    }

    /// The single statement of a genesis slot `(s, p)` in the default graph in `state`.
    fn value_at(state: &BTreeSet<Stmt>, s: u64, p: u64) -> Stmt {
        let found: Vec<&Stmt> = state
            .range(stmt(0, s, p, false, 0)..stmt(0, s, p + 1, false, 0))
            .collect();
        assert_eq!(
            found.len(),
            1,
            "slot ({s}, {p}) must hold exactly its initial value"
        );
        *found[0]
    }

    /// The oracle's expectation of one preview; also returns the merged state.
    fn expect(
        &mut self,
        target: &str,
        source: &str,
        strategy: Strategy,
        explicit_base: Option<&Label>,
        designed: &BTreeSet<u64>,
    ) -> (PreviewExpect, Option<BTreeSet<Stmt>>) {
        let (t, tstate) = self.heads[target].clone();
        let (s, sstate) = self.heads[source].clone();
        let at = self.ancestors_of(&t);
        let as_ = self.ancestors_of(&s);
        let ahead = as_.difference(&at).count();
        let behind = at.difference(&as_).count();
        let plain = |classification, candidates: Vec<Label>| PreviewExpect {
            strategy: strategy.as_str(),
            explicit_base: explicit_base.cloned(),
            classification,
            base_candidates: candidates,
            merge_base: None,
            ahead,
            behind,
            conflict_count: 0,
            conflict_keys: Vec::new(),
            target_delta: None,
            source_delta: None,
            merged: None,
        };
        if t == s {
            return (plain("already_equal", Vec::new()), None);
        }
        if at.contains(&s) {
            return (plain("already_contained", Vec::new()), None);
        }
        let ff = as_.contains(&t);
        let base = if ff {
            t.clone()
        } else {
            let common: Vec<Label> = at.intersection(&as_).cloned().collect();
            let mut best = Vec::new();
            for c in &common {
                let dominated = common
                    .iter()
                    .any(|o| o != c && self.ancestors_of(o).contains(c));
                if !dominated {
                    best.push(c.clone());
                }
            }
            assert!(!best.is_empty(), "scenario histories are related");
            match explicit_base {
                Some(b) => {
                    assert!(
                        best.contains(b),
                        "explicit base {b} must be a best common ancestor"
                    );
                    b.clone()
                }
                None if best.len() > 1 => {
                    return (plain("ambiguous_merge_base", best), None);
                }
                None => best.remove(0),
            }
        };
        let bstate = self
            .retained
            .get(&base)
            .unwrap_or_else(|| panic!("merge base {base} must be retained"))
            .as_ref()
            .clone();
        // The design premise: every slot both sides changed differently is designed.
        let (bs, ts, ss) = (slots(&bstate), slots(&tstate), slots(&sstate));
        let keys: BTreeSet<u64> = bs
            .keys()
            .chain(ts.keys())
            .chain(ss.keys())
            .copied()
            .collect();
        let empty = BTreeSet::new();
        let mut actual = BTreeSet::new();
        for k in keys {
            let (b, tk, sk) = (
                bs.get(&k).unwrap_or(&empty),
                ts.get(&k).unwrap_or(&empty),
                ss.get(&k).unwrap_or(&empty),
            );
            if tk != b && sk != b && tk != sk {
                actual.insert(k);
            }
        }
        assert_eq!(
            &actual, designed,
            "generator premise: conflicting slots must be exactly the designed ones"
        );
        let conflicted = !designed.is_empty() && strategy == Strategy::Abort;
        let merged = (!conflicted).then(|| {
            let non = |x: &BTreeSet<Stmt>| -> BTreeSet<Stmt> {
                x.iter()
                    .filter(|q| !designed.contains(&slot(**q)))
                    .copied()
                    .collect()
            };
            let (b, tn, sn) = (non(&bstate), non(&tstate), non(&sstate));
            let removed: BTreeSet<Stmt> = b.difference(&sn).copied().collect();
            let mut m: BTreeSet<Stmt> = tn.difference(&removed).copied().collect();
            m.extend(sn.difference(&b));
            for k in designed {
                let tk = ts.get(k).cloned().unwrap_or_default();
                let sk = ss.get(k).cloned().unwrap_or_default();
                match strategy {
                    Strategy::TakeTarget => m.extend(tk),
                    Strategy::TakeSource => m.extend(sk),
                    Strategy::Union => {
                        m.extend(tk);
                        m.extend(sk);
                    }
                    Strategy::Abort => {
                        unreachable!("abort without conflicts has no designed slots")
                    }
                }
            }
            m
        });
        let classification = match &merged {
            None => "conflicted",
            Some(m) if *m == tstate && bstate == sstate => "no_change",
            Some(_) if ff => "fast_forward",
            Some(_) => "divergent",
        };
        let expect = PreviewExpect {
            strategy: strategy.as_str(),
            explicit_base: explicit_base.cloned(),
            classification,
            base_candidates: Vec::new(),
            merge_base: Some(base),
            ahead,
            behind,
            conflict_count: if ff { 0 } else { designed.len() },
            conflict_keys: designed.iter().map(|k| render_slot(*k)).collect(),
            target_delta: Some(summary(&bstate, &tstate)),
            source_delta: Some(summary(&bstate, &sstate)),
            merged: merged
                .as_ref()
                .filter(|_| matches!(classification, "fast_forward" | "divergent"))
                .map(|m| Arc::new(render_state(m)) as State),
        };
        (expect, merged)
    }

    /// One merge request: previews (strategy, explicit base) in order, then optionally apply
    /// `apply`, which must yield a candidate. Returns the integration commit label.
    fn merge(
        &mut self,
        source: &str,
        target: &str,
        previews: &[(Strategy, Option<&Label>)],
        apply: Option<(Strategy, Option<&Label>)>,
        designed: &BTreeSet<u64>,
    ) -> Option<Label> {
        self.merges += 1;
        let id = format!("merge#{}", self.merges);
        let t = self.retain_head(target);
        let s = self.retain_head(source);
        let tstate = self.heads[target].1.clone();
        let mut expects = Vec::new();
        for (strategy, base) in previews {
            expects.push(self.expect(target, source, *strategy, *base, designed).0);
        }
        let mut applied = None;
        let mut integrated = None;
        if let Some((strategy, base)) = apply {
            let (e, merged) = self.expect(target, source, strategy, base, designed);
            assert!(
                matches!(e.classification, "fast_forward" | "divergent"),
                "{id}: applying a {} merge",
                e.classification
            );
            let provenance = Provenance {
                activity: "merge".into(),
                message: id.clone(),
                evidence_refs: vec![format!("urn:bench:merge:{}", self.merges)],
                source_system: None,
            };
            applied = Some(ApplyStep {
                strategy: strategy.as_str(),
                explicit_base: base.cloned(),
                label: id.clone(),
                provenance: provenance.clone(),
            });
            integrated = Some((
                id.clone(),
                merged.expect("candidate has a merged state"),
                provenance,
            ));
            if !previews.contains(&(strategy, base)) {
                expects.push(e);
            }
        }
        self.steps.push(Step::Merge(MergeStep {
            id: id.clone(),
            source: source.to_owned(),
            target: target.to_owned(),
            previews: expects,
            apply: applied,
        }));
        let (label, merged, provenance) = integrated?;
        let ops = merged.symmetric_difference(&tstate).count() as u64;
        self.plan_retain.insert(label.clone());
        self.record(&label, vec![t, s], &merged, ops, "merge", provenance);
        self.heads
            .insert(target.to_owned(), (label.clone(), merged));
        Some(label)
    }
}

/// Generate the workload of a synthetic profile (pure and deterministic).
pub fn generate(p: &Params) -> Workload {
    use Strategy::*;
    let mut g = Gen::new(p.clone());
    let genesis = g.genesis();
    g.plan_retain.insert("main@5".into());
    g.plan_retain.insert("main@10".into());
    let none = BTreeSet::new();
    let abort = [(Abort, None)];

    // Linear main, then a diamond (a, b) forked at a historical commit, plus a growing-state
    // and a constant-state history forked even earlier.
    for _ in 0..p.main_pre {
        g.mixed_commit("main");
    }
    let fork = "main@10".to_owned();
    let early = "main@5".to_owned();
    g.branch("feature/a", "main", &fork);
    g.branch("feature/b", "main", &fork);
    g.branch("growth", "main", &early);
    g.branch("churn", "main", &early);
    for i in 0..p.feature.max(p.growth).max(p.churn) {
        if i < p.feature {
            g.mixed_commit("feature/a");
            g.mixed_commit("feature/b");
        }
        if i < p.growth {
            g.growth_commit();
        }
        if i < p.churn {
            g.churn_commit();
        }
    }
    for _ in 0..p.main_mid {
        g.mixed_commit("main");
    }
    // Diamond: both sides of the fork integrate into the moved main (divergent).
    let m1 = g.merge("feature/a", "main", &abort, Some((Abort, None)), &none);
    g.merge("feature/b", "main", &abort, Some((Abort, None)), &none);
    // Repeated merge: contained; syncing back is a fast-forward class; then no change.
    g.merge("feature/a", "main", &abort, None, &none);
    g.merge("main", "feature/a", &abort, Some((Abort, None)), &none);
    g.merge("feature/a", "main", &abort, None, &none);
    // A branch at the head is already equal; with work it merges as a fast-forward class.
    let head = g.retain_head("main");
    g.branch("feature/e", "main", &head);
    g.merge("feature/e", "main", &abort, None, &none);
    for _ in 0..p.fast {
        g.mixed_commit("feature/e");
    }
    g.merge("feature/e", "main", &abort, Some((Abort, None)), &none);

    // Designed conflicts: c and d fork together; c integrates first (fast-forward class),
    // then d conflicts with it on `conflicts` slots (five shapes, module docs) and agrees
    // with it on `convergent` slots.
    let head = g.retain_head("main");
    g.branch("feature/c", "main", &head);
    g.branch("feature/d", "main", &head);
    let contested = g.pool("contested");
    let base_state = g.heads["main"].1.clone();
    let (mut c_adds, mut c_dels, mut d_adds, mut d_dels) = (
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    let mut designed = BTreeSet::new();
    for (i, s) in contested.clone().enumerate() {
        let old = Gen::value_at(&base_state, s, 0);
        let i = i as u32;
        let (x, y) = (g.fresh(), g.fresh());
        let (vx, vy) = (stmt(0, s, 0, true, x), stmt(0, s, 0, true, y));
        if i >= p.conflicts {
            // Convergent: both replace the value with the same new one.
            c_dels.insert(old);
            c_adds.insert(vx);
            d_dels.insert(old);
            d_adds.insert(vx);
            continue;
        }
        designed.insert(slot(old));
        match i % 5 {
            0 => {
                // replace / replace
                c_dels.insert(old);
                c_adds.insert(vx);
                d_dels.insert(old);
                d_adds.insert(vy);
            }
            1 => {
                // delete (target) / modify (source)
                c_dels.insert(old);
                d_dels.insert(old);
                d_adds.insert(vy);
            }
            2 => {
                // modify (target) / delete (source)
                c_dels.insert(old);
                c_adds.insert(vx);
                d_dels.insert(old);
            }
            3 => {
                // keep-and-add (target) / delete (source): union keeps `old`
                c_adds.insert(vx);
                d_dels.insert(old);
            }
            _ => {
                // add / add of different values to a slot both keep (multi-valued)
                c_adds.insert(vx);
                d_adds.insert(vy);
            }
        }
    }
    g.commit("feature/c", c_adds, c_dels);
    g.commit("feature/d", d_adds, d_dels);
    for _ in 1..p.contested {
        g.mixed_commit("feature/c");
        g.mixed_commit("feature/d");
    }
    let before_conflict = g.heads["main"].0.clone();
    g.merge("feature/c", "main", &abort, Some((Abort, None)), &none);
    let all = [
        (Abort, None),
        (TakeTarget, None),
        (TakeSource, None),
        (Union, None),
    ];
    let resolved = g.merge("feature/d", "main", &all, Some((Union, None)), &designed);
    g.merge("feature/d", "main", &abort, None, &none);
    g.merge("main", "feature/c", &abort, Some((Abort, None)), &none);
    g.merge("main", "feature/d", &abort, Some((Abort, None)), &none);

    // A merge base reachable only through a second parent: c (synced with main through an
    // integration commit whose parent 1 is main) and main both move on, then c → main.
    for _ in 0..3 {
        g.mixed_commit("feature/c");
    }
    for _ in 0..2 {
        g.mixed_commit("main");
    }
    g.merge("feature/c", "main", &abort, Some((Abort, None)), &none);

    // NO_CHANGE in both directions:
    // (1) the merged state equals the target but the source changed something
    //     (take-target over every differing slot) → an empty integration commit
    //     (`divergent`), never `no_change`;
    // (2) a source whose changes net to nothing while the target moved → `no_change`.
    let resolve = g.pool("resolve");
    let (mut t_adds, mut t_dels, mut s_adds, mut s_dels, mut resolve_slots) = (
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    let main_state = g.heads["main"].1.clone();
    for s in resolve {
        let old = Gen::value_at(&main_state, s, 0);
        let (x, y) = (g.fresh(), g.fresh());
        resolve_slots.insert(slot(old));
        t_dels.insert(old);
        t_adds.insert(stmt(0, s, 0, true, x));
        s_dels.insert(old);
        s_adds.insert(stmt(0, s, 0, true, y));
    }
    g.commit("feature/b", s_adds, s_dels);
    g.commit("main", t_adds, t_dels);
    g.merge(
        "feature/b",
        "main",
        &[(Abort, None), (TakeTarget, None)],
        Some((TakeTarget, None)),
        &resolve_slots,
    );
    let netzero = g.pool("netzero").start;
    let x = g.fresh();
    let temp = stmt(0, netzero, 1, true, x);
    g.commit("feature/e", BTreeSet::from([temp]), BTreeSet::new());
    g.commit("feature/e", BTreeSet::new(), BTreeSet::from([temp]));
    g.mixed_commit("main");
    g.merge("feature/e", "main", &abort, None, &none);

    // Criss-cross: a (synced long ago) and the earlier `growth`/`churn` work are not
    // involved; c and d each move on, then integrate each other's *old* heads (the second
    // through a historical branch point), leaving two best common ancestors. The ambiguous
    // preview lists both; an explicit base resolves it.
    g.mixed_commit("feature/c");
    g.mixed_commit("feature/d");
    let c_old = g.retain_head("feature/c");
    let d_old = g.retain_head("feature/d");
    g.branch("crisscross", "feature/c", &c_old);
    g.merge("feature/d", "feature/c", &abort, Some((Abort, None)), &none);
    g.merge(
        "crisscross",
        "feature/d",
        &abort,
        Some((Abort, None)),
        &none,
    );
    let candidates = [(Abort, None), (Abort, Some(&c_old)), (Abort, Some(&d_old))];
    g.merge(
        "feature/d",
        "feature/c",
        &candidates,
        Some((Abort, Some(&d_old))),
        &none,
    );

    // The growing- and constant-state histories integrate last (divergent, early base).
    let growth_head = g.retain_head("growth");
    let churn_head = g.retain_head("churn");
    g.merge("growth", "main", &abort, Some((Abort, None)), &none);
    g.merge("churn", "main", &abort, Some((Abort, None)), &none);
    for _ in 0..p.main_post {
        g.mixed_commit("main");
    }

    let final_heads: BTreeMap<String, Label> = g
        .heads
        .iter()
        .map(|(b, (l, _))| (b.clone(), l.clone()))
        .collect();
    for b in final_heads.keys().cloned().collect::<Vec<_>>() {
        g.retain_head(&b);
    }
    let main_head = final_heads["main"].clone();
    let diff_pairs = vec![
        (genesis.clone(), main_head),
        (fork, m1.expect("applied")),
        (before_conflict, resolved.expect("applied")),
        (early.clone(), growth_head),
        (early, churn_head),
        (c_old, d_old),
    ];
    Workload {
        steps: g.steps,
        expected: g.expected,
        final_heads,
        diff_pairs,
        verify_all_history: p.verify_all_history,
        history_facts: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::workload_checksum;

    fn merges(w: &Workload) -> Vec<&MergeStep> {
        w.steps
            .iter()
            .filter_map(|s| match s {
                Step::Merge(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn rendering_is_canonical_n_quads() {
        assert_eq!(
            render(stmt(0, 7, 2, true, 42)),
            "<urn:bench:e:7> <urn:bench:p:2> \"v42\" ."
        );
        assert_eq!(
            render(stmt(1, 7, 6, false, 9)),
            "<urn:bench:e:7> <urn:bench:p:6> <urn:bench:e:9> <urn:bench:g:1> ."
        );
        for line in [
            render(stmt(0, 7, 2, true, 42)),
            render(stmt(1, 7, 6, false, 9)),
        ] {
            let q: ledger_rdf::Quad = line.parse().expect("valid N-Quads");
            assert_eq!(q.as_str(), line, "already in the ledger's canonical form");
        }
        assert_eq!(slot(stmt(1, 7, 6, false, 9)), slot(stmt(1, 7, 6, true, 3)));
        assert_ne!(slot(stmt(0, 7, 6, true, 3)), slot(stmt(1, 7, 6, true, 3)));
    }

    #[test]
    fn the_ci_profile_is_deterministic_and_meets_its_shape() {
        let a = generate(&Params::ci());
        let b = generate(&Params::ci());
        assert_eq!(workload_checksum(&a), workload_checksum(&b));
        let mut other = Params::ci();
        other.seed += 1;
        assert_ne!(workload_checksum(&a), workload_checksum(&generate(&other)));

        let genesis = &a.expected["main@0"];
        assert!(
            genesis.quads >= 5_000 && genesis.quads <= 10_000,
            "{}",
            genesis.quads
        );
        assert!(
            (150..=260).contains(&a.commit_count()),
            "{}",
            a.commit_count()
        );
        // main plus the work branches (4–8 suggested) and the criss-cross helper.
        let branches = a.final_heads.len();
        assert!((5..=10).contains(&branches), "{branches}");
        let ms = merges(&a);
        assert!(ms.iter().filter(|m| m.apply.is_some()).count() >= 10);
        let conflicts = ms
            .iter()
            .flat_map(|m| &m.previews)
            .map(|p| p.conflict_count)
            .max()
            .unwrap();
        assert!(conflicts >= 5);
        let classes: BTreeSet<&str> = ms
            .iter()
            .flat_map(|m| &m.previews)
            .map(|p| p.classification)
            .collect();
        for c in [
            "already_equal",
            "already_contained",
            "no_change",
            "fast_forward",
            "divergent",
            "conflicted",
            "ambiguous_merge_base",
        ] {
            assert!(classes.contains(c), "missing {c}: {classes:?}");
        }
        // Every commit fits one API request (≤ 10,000 operations; limits are never raised).
        let commits: Vec<&CommitStep> = a
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Commit(c) => Some(c),
                _ => None,
            })
            .collect();
        assert!(
            commits
                .iter()
                .all(|c| c.adds.len() + c.deletes.len() <= 10_000)
        );
        // Additions, deletions and replacements all occur; provenance is unique per change.
        assert!(
            commits
                .iter()
                .any(|c| c.deletes.is_empty() && !c.adds.is_empty())
        );
        assert!(
            commits
                .iter()
                .any(|c| c.adds.is_empty() && !c.deletes.is_empty())
        );
        assert!(
            commits
                .iter()
                .any(|c| !c.adds.is_empty() && !c.deletes.is_empty())
        );
        let refs: BTreeSet<&String> = a
            .expected
            .values()
            .flat_map(|e| &e.provenance.evidence_refs)
            .collect();
        assert_eq!(refs.len(), a.expected.len());
        for (x, y) in &a.diff_pairs {
            assert!(a.expected[x].state.is_some() && a.expected[y].state.is_some());
        }
    }

    #[test]
    fn the_oracle_states_are_consistent_with_the_steps() {
        // Replaying the generated commit steps with plain set algebra reproduces every
        // recorded digest; integration commits are checked by the next test.
        let w = generate(&Params::ci());
        let mut states: BTreeMap<Label, BTreeSet<String>> = BTreeMap::new();
        for step in &w.steps {
            if let Step::Commit(c) = step {
                let mut s = c
                    .parent
                    .as_ref()
                    .map(|p| states[p].clone())
                    .unwrap_or_default();
                for d in &c.deletes {
                    assert!(s.remove(d), "{}: delete of an absent statement", c.label);
                }
                for a in &c.adds {
                    assert!(
                        s.insert(a.clone()),
                        "{}: add of a present statement",
                        c.label
                    );
                }
                let e = &w.expected[&c.label];
                assert_eq!(e.digest, oracle_digest(s.iter().map(String::as_str)));
                if let Some(full) = &e.state {
                    assert_eq!(full.as_ref(), &s);
                }
                states.insert(c.label.clone(), s);
            }
            if let Step::Merge(m) = step
                && let Some(a) = &m.apply
            {
                let e = &w.expected[&a.label];
                let full = e.state.as_ref().expect("integration commits are retained");
                states.insert(a.label.clone(), full.as_ref().clone());
            }
        }
    }

    /// A second, slot-by-slot reading of ADR-0024 (`structural-slot/v1`), written
    /// independently of `expect`'s set formula, over the retained states: every applied
    /// merge's expected state must agree with it.
    #[test]
    fn applied_merge_expectations_agree_with_a_slot_by_slot_reading() {
        let w = generate(&Params::ci());
        let parse = |s: &State| -> BTreeMap<(String, String, String), BTreeSet<String>> {
            let mut out: BTreeMap<_, BTreeSet<String>> = BTreeMap::new();
            for line in s.iter() {
                let parts: Vec<&str> = line.split(' ').collect();
                let graph = if parts.len() == 5 {
                    parts[3].to_owned()
                } else {
                    String::new()
                };
                out.entry((graph, parts[0].to_owned(), parts[1].to_owned()))
                    .or_default()
                    .insert(line.clone());
            }
            out
        };
        for m in merges(&w) {
            let Some(apply) = &m.apply else { continue };
            let p = m
                .previews
                .iter()
                .find(|p| p.strategy == apply.strategy && p.explicit_base == apply.explicit_base)
                .expect("the applied preview");
            let base = p.merge_base.as_ref().expect("a candidate has a base");
            let [t, s] = &w.expected[&apply.label].parents[..] else {
                panic!("two parents")
            };
            let (bs, ts, ss) = (
                parse(w.expected[base].state.as_ref().unwrap()),
                parse(w.expected[t].state.as_ref().unwrap()),
                parse(w.expected[s].state.as_ref().unwrap()),
            );
            let keys: BTreeSet<_> = bs
                .keys()
                .chain(ts.keys())
                .chain(ss.keys())
                .cloned()
                .collect();
            let mut merged = BTreeSet::new();
            let empty = BTreeSet::new();
            for k in keys {
                let (b, tk, sk) = (
                    bs.get(&k).unwrap_or(&empty),
                    ts.get(&k).unwrap_or(&empty),
                    ss.get(&k).unwrap_or(&empty),
                );
                let result: BTreeSet<String> = if tk == b || tk == sk {
                    sk.clone()
                } else if sk == b {
                    tk.clone()
                } else {
                    match apply.strategy {
                        "take-target" => tk.clone(),
                        "take-source" => sk.clone(),
                        "union" => tk.union(sk).cloned().collect(),
                        other => panic!("{}: {other} cannot resolve a conflict", m.id),
                    }
                };
                merged.extend(result);
            }
            assert_eq!(
                &merged,
                w.expected[&apply.label].state.as_ref().unwrap().as_ref(),
                "{}",
                m.id
            );
        }
    }

    #[test]
    fn designed_conflicts_resolve_per_strategy() {
        let w = generate(&Params::ci());
        let m = merges(&w)
            .into_iter()
            .find(|m| m.previews.len() == 4)
            .expect("the conflict scenario");
        let by: BTreeMap<&str, &PreviewExpect> =
            m.previews.iter().map(|p| (p.strategy, p)).collect();
        assert_eq!(by["abort"].classification, "conflicted");
        assert!(by["abort"].merged.is_none());
        assert_eq!(by["abort"].conflict_count, 10);
        let t = by["take-target"].merged.as_ref().unwrap();
        let s = by["take-source"].merged.as_ref().unwrap();
        let u = by["union"].merged.as_ref().unwrap();
        // Union keeps statements one side deleted (shape 4), so it is a strict superset of
        // both resolutions and of their intersection.
        assert!(t.is_subset(u) && s.is_subset(u) && t != s);
        assert_eq!(u.len(), t.union(s).count());
    }

    #[test]
    fn both_no_change_directions_and_the_ambiguous_base_are_generated() {
        let w = generate(&Params::ci());
        let ms = merges(&w);
        // The take-target resolution keeps the target state but is not `no_change`.
        let resolution = ms
            .iter()
            .find(|m| {
                m.apply
                    .as_ref()
                    .is_some_and(|a| a.strategy == "take-target")
            })
            .expect("take-target resolution");
        let label = &resolution.apply.as_ref().unwrap().label;
        let target = &w.expected[label].parents[0];
        assert_eq!(w.expected[label].digest, w.expected[target].digest);
        assert!(
            resolution
                .previews
                .iter()
                .any(|p| p.strategy == "take-target" && p.classification == "divergent")
        );
        assert!(
            ms.iter()
                .flat_map(|m| &m.previews)
                .filter(|p| p.classification == "no_change")
                .count()
                >= 2
        );
        let ambiguous = ms
            .iter()
            .flat_map(|m| &m.previews)
            .find(|p| p.classification == "ambiguous_merge_base")
            .expect("criss-cross");
        assert_eq!(ambiguous.base_candidates.len(), 2);
    }

    #[test]
    #[ignore = "≈90 s in a debug build; CI runs `ledger-bench validate --profile local` (release) in scripts/benchmark.sh, which regenerates this profile and checks its manifest"]
    fn the_local_profile_generates() {
        let w = generate(&Params::local());
        assert!(w.commit_count() > 1_000);
        assert!(w.steps.iter().all(|s| match s {
            Step::Commit(c) => c.adds.len() + c.deletes.len() <= 10_000,
            _ => true,
        }));
        let deepest = w.expected.values().map(|e| e.depth).max().unwrap();
        assert!(deepest >= 500, "{deepest}");
    }
}
