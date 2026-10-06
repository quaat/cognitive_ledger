//! `synthetic-ledger-*`: a deterministic, seeded generator of a Cognitive Ledger history
//! (linear, forked, diamond and merge-heavy shapes; additions, deletions and replacements;
//! designed structural conflicts, delete-vs-modify and convergent changes) **and** the
//! independent oracle of that history.
//!
//! The oracle never calls the ledger's crates. It knows every state because it generated
//! every change, and merge outcomes by construction:
//! - **States.** `state(child) = state(parent) − deletes + adds`, as set algebra over
//!   generated statements.
//! - **Ancestry, merge base, ahead/behind.** A brute-force reference over the symbolic DAG:
//!   ancestor sets, and the maximal elements of their intersection. This is the definition
//!   in ADR-0024, not `ledger-dag`'s traversal.
//! - **Merges** (ADR-0023/0024 `structural-slot/v1`). Each scenario is *designed* so that
//!   target and source touch disjoint `(graph, subject, predicate)` slots, apart from the
//!   slots the scenario deliberately puts in conflict and deliberately convergent ones.
//!   - Outside the designed conflicts, the merged state is the three-way set formula
//!     `(T − (B − S)) ∪ (S − B)`.
//!   - Inside them, it is what the strategy specifies (`T|k`, `S|k` or `T|k ∪ S|k`).
//!
//!   The design premise is checked whenever a merge is generated: a slot changed
//!   differently on both sides that was not designed as a conflict is a generator bug and
//!   panics. It is never silently folded into the expectation.
//! - **NO_CHANGE.** The merged state equals T and the source has no net change from B
//!   (ADR-0024).
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
pub const GENERATOR_VERSION: &str = "synthetic-ledger-gen/2";

/// Size and shape parameters of a synthetic profile.
#[derive(Clone, Debug, Serialize)]
pub struct Params {
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
    /// Designed conflicting slots, of which the first `delete_modify` are delete-vs-modify.
    pub conflicts: u32,
    pub delete_modify: u32,
    /// Slots both sides change identically (not conflicts).
    pub convergent: u32,
    /// Retain the full expected state of every n-th commit of each branch (diffs, samples).
    pub sample_every: u32,
    /// Reconstruct every commit again at the end (else only retained ones).
    pub verify_all_history: bool,
}

impl Params {
    /// The PR-CI profile (Plan 0010 targets: ~1,000 entities, ~6,000 initial statements,
    /// ~200 commits, 7 branches, 10 applied merges, 6 designed conflicts).
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
            conflicts: 6,
            delete_modify: 2,
            convergent: 2,
            sample_every: 10,
            verify_all_history: true,
        }
    }

    /// A deeper local profile for baselines (≈1,400 commits; parent-0 depth up to ≈650).
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
            conflicts: 12,
            delete_modify: 4,
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
/// Statements per bulk-load commit (well below the API's 10,000 operations per request).
const BULK_ADDS: usize = 5_000;
const MAX_VALUE: u64 = 1 << 33;

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

impl Gen {
    fn new(p: Params) -> Self {
        Self {
            rng: Rng(p.seed),
            p,
            next_value: 0,
            next_entity: 0,
            next_change: 0,
            heads: BTreeMap::new(),
            counters: BTreeMap::new(),
            parents: BTreeMap::new(),
            depth: BTreeMap::new(),
            retained: BTreeMap::new(),
            plan_retain: BTreeSet::new(),
            ancestors: BTreeMap::new(),
            steps: Vec::new(),
            expected: BTreeMap::new(),
            merges: 0,
        }
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

    /// Record the expectation of a new commit (and retain its full state if planned).
    fn record(
        &mut self,
        label: &Label,
        parents: Vec<Label>,
        state: &BTreeSet<Stmt>,
        kind: &'static str,
        provenance: Provenance,
    ) {
        let depth = parents.first().map_or(0, |p| self.depth[p] + 1);
        let rendered = render_state(state);
        let digest = oracle_digest(rendered.iter().map(String::as_str));
        self.depth.insert(label.clone(), depth);
        self.parents.insert(label.clone(), parents.clone());
        self.expected.insert(
            label.clone(),
            Expected {
                parents,
                depth,
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

    fn retain_rendered(
        &mut self,
        label: &Label,
        state: &BTreeSet<Stmt>,
        rendered: BTreeSet<String>,
    ) {
        self.retained.insert(label.clone(), Arc::new(state.clone()));
        self.expected.get_mut(label).expect("recorded").state = Some(Arc::new(rendered));
    }

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
        // The initial load is split into bulk commits of at most `BULK_ADDS` statements
        // (the API caps a request at 10,000 operations; limits are never raised for
        // benchmarks). They are labelled `main@0.1`, `main@0.2`, …; the last one is `main@0`,
        // the loaded state.
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
            self.record(
                &label,
                parent.iter().cloned().collect(),
                &loaded,
                "main",
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
        self.record(&label, vec![parent], &next, kind_of(branch), provenance);
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

    /// A mixed change (add, delete, replace) confined to the branch's pool.
    fn mixed_commit(&mut self, branch: &str) -> Label {
        let pool = self.pool(branch);
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

    fn branch(&mut self, name: &str, from: &Label) {
        let state = self
            .retained
            .get(from)
            .unwrap_or_else(|| panic!("branch point {from} must be retained"))
            .as_ref()
            .clone();
        self.steps.push(Step::Branch {
            name: name.to_owned(),
            source: "main".into(),
            from: from.clone(),
        });
        self.counters.insert(name.to_owned(), 0);
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

    /// The oracle's expectation of one preview; also returns the merged state.
    fn expect(
        &mut self,
        target: &str,
        source: &str,
        strategy: Strategy,
        designed: &BTreeSet<u64>,
    ) -> (PreviewExpect, Option<BTreeSet<Stmt>>) {
        let (t, tstate) = self.heads[target].clone();
        let (s, sstate) = self.heads[source].clone();
        let at = self.ancestors_of(&t);
        let as_ = self.ancestors_of(&s);
        let ahead = as_.difference(&at).count();
        let behind = at.difference(&as_).count();
        let plain = |classification| PreviewExpect {
            strategy: strategy.as_str(),
            classification,
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
            return (plain("already_equal"), None);
        }
        if at.contains(&s) {
            return (plain("already_contained"), None);
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
            assert_eq!(
                best.len(),
                1,
                "scenario must have a unique merge base: {best:?}"
            );
            best.remove(0)
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
            classification,
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

    /// One merge request: previews with each strategy in `previews`, then optionally apply
    /// `apply` (which must yield a candidate). Returns the integration commit label.
    fn merge(
        &mut self,
        source: &str,
        target: &str,
        previews: &[Strategy],
        apply: Option<Strategy>,
        designed: &BTreeSet<u64>,
    ) -> Option<Label> {
        self.merges += 1;
        let id = format!("merge#{}", self.merges);
        let (t, tstate) = self.heads[target].clone();
        let (s, sstate) = self.heads[source].clone();
        self.retain(&t, &tstate);
        self.retain(&s, &sstate);
        let mut expects = Vec::new();
        for strategy in previews {
            expects.push(self.expect(target, source, *strategy, designed).0);
        }
        let mut applied = None;
        let mut integrated = None;
        if let Some(strategy) = apply {
            let (e, merged) = self.expect(target, source, strategy, designed);
            assert!(
                matches!(e.classification, "fast_forward" | "divergent"),
                "{id}: applying a {} merge",
                e.classification
            );
            let label = id.clone();
            let provenance = Provenance {
                activity: "merge".into(),
                message: id.clone(),
                evidence_refs: vec![format!("urn:bench:merge:{}", self.merges)],
                source_system: None,
            };
            applied = Some(ApplyStep {
                strategy: strategy.as_str(),
                label: label.clone(),
                provenance: provenance.clone(),
            });
            integrated = Some((
                label,
                merged.expect("candidate has a merged state"),
                provenance,
            ));
            if !previews.contains(&strategy) {
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
        self.plan_retain.insert(label.clone());
        self.record(&label, vec![t, s], &merged, "merge", provenance);
        self.heads
            .insert(target.to_owned(), (label.clone(), merged));
        Some(label)
    }
}

/// Generate the workload of a synthetic profile (pure and deterministic).
pub fn generate(p: &Params) -> Workload {
    let mut g = Gen::new(p.clone());
    let genesis = g.genesis();
    g.plan_retain.insert("main@5".into());
    g.plan_retain.insert("main@10".into());

    // Linear main, then a diamond (a, b) forked at a historical commit, plus a growing-state
    // and a constant-state history forked even earlier.
    for _ in 0..p.main_pre {
        g.mixed_commit("main");
    }
    let fork = "main@10".to_owned();
    let early = "main@5".to_owned();
    g.branch("feature/a", &fork);
    g.branch("feature/b", &fork);
    g.branch("growth", &early);
    g.branch("churn", &early);
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
    let none = BTreeSet::new();
    use Strategy::*;
    // Diamond: both sides of the fork integrate into the moved main (divergent).
    let m1 = g.merge("feature/a", "main", &[Abort], Some(Abort), &none);
    g.merge("feature/b", "main", &[Abort], Some(Abort), &none);
    // Repeated merge: contained; syncing back is a fast-forward class; then no change.
    g.merge("feature/a", "main", &[Abort], None, &none);
    g.merge("main", "feature/a", &[Abort], Some(Abort), &none);
    g.merge("feature/a", "main", &[Abort], None, &none);
    // A branch at the head is already equal; with work it merges as a fast-forward class.
    let head = g.heads["main"].0.clone();
    g.retain(&head, &g.heads["main"].1.clone());
    g.branch("feature/e", &head);
    g.merge("feature/e", "main", &[Abort], None, &none);
    for _ in 0..p.fast {
        g.mixed_commit("feature/e");
    }
    g.merge("feature/e", "main", &[Abort], Some(Abort), &none);

    // Designed conflicts: c and d fork together; c integrates first (fast-forward class),
    // then d conflicts with it on `conflicts` slots (the first `delete_modify` are
    // delete-vs-modify) and agrees with it on `convergent` slots.
    let head = g.heads["main"].0.clone();
    g.retain(&head, &g.heads["main"].1.clone());
    g.branch("feature/c", &head);
    g.branch("feature/d", &head);
    let contested = g.pool("contested");
    let base_state = g.heads["main"].1.clone();
    let value_at = |s: u64| -> Stmt {
        *base_state
            .range(stmt(0, s, 0, false, 0)..stmt(0, s, 1, false, 0))
            .next()
            .expect("contested slot has its initial value")
    };
    let (mut c_adds, mut c_dels, mut d_adds, mut d_dels) = (
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    let mut designed = BTreeSet::new();
    for (i, s) in contested.clone().enumerate() {
        let old = value_at(s);
        let i = i as u32;
        if i < p.conflicts {
            designed.insert(slot(old));
            c_dels.insert(old);
            if i >= p.delete_modify {
                let x = g.fresh();
                c_adds.insert(stmt(0, s, 0, true, x));
            }
            let y = g.fresh();
            d_dels.insert(old);
            d_adds.insert(stmt(0, s, 0, true, y));
        } else {
            let z = g.fresh();
            c_dels.insert(old);
            c_adds.insert(stmt(0, s, 0, true, z));
            d_dels.insert(old);
            d_adds.insert(stmt(0, s, 0, true, z));
        }
    }
    g.commit("feature/c", c_adds, c_dels);
    g.commit("feature/d", d_adds, d_dels);
    for _ in 1..p.contested {
        g.mixed_commit("feature/c");
        g.mixed_commit("feature/d");
    }
    let before_conflict = g.heads["main"].0.clone();
    g.merge("feature/c", "main", &[Abort], Some(Abort), &none);
    let resolved = g.merge(
        "feature/d",
        "main",
        &[Abort, TakeTarget, TakeSource, Union],
        Some(Union),
        &designed,
    );
    g.merge("feature/d", "main", &[Abort], None, &none);
    g.merge("main", "feature/c", &[Abort], Some(Abort), &none);
    g.merge("main", "feature/d", &[Abort], Some(Abort), &none);
    // The growing- and constant-state histories integrate last (divergent, early base).
    let growth_head = g.heads["growth"].0.clone();
    let churn_head = g.heads["churn"].0.clone();
    g.merge("growth", "main", &[Abort], Some(Abort), &none);
    g.merge("churn", "main", &[Abort], Some(Abort), &none);
    for _ in 0..p.main_post {
        g.mixed_commit("main");
    }

    let final_heads: BTreeMap<String, Label> = g
        .heads
        .iter()
        .map(|(b, (l, _))| (b.clone(), l.clone()))
        .collect();
    for (b, l) in &final_heads {
        let state = g.heads[b].1.clone();
        g.retain(l, &state);
    }
    let main_head = final_heads["main"].clone();
    let diff_pairs = vec![
        (genesis.clone(), main_head),
        (fork, m1.expect("applied")),
        (before_conflict, resolved.expect("applied")),
        (early.clone(), growth_head),
        (early, churn_head),
    ];
    Workload {
        steps: g.steps,
        expected: g.expected,
        final_heads,
        diff_pairs,
        verify_all_history: p.verify_all_history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workload::workload_checksum;

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
        let branches = a.final_heads.len();
        assert!((4..=8).contains(&branches), "{branches}");
        let merges: Vec<&MergeStep> = a
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Merge(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(merges.iter().filter(|m| m.apply.is_some()).count() >= 10);
        let conflicts = merges
            .iter()
            .flat_map(|m| &m.previews)
            .map(|p| p.conflict_count)
            .max()
            .unwrap();
        assert!(conflicts >= 5);
        let classes: BTreeSet<&str> = merges
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
        ] {
            assert!(classes.contains(c), "missing {c}: {classes:?}");
        }
        // Additions, deletions and replacements all occur; provenance is unique per change.
        let commits: Vec<&CommitStep> = a
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Commit(c) => Some(c),
                _ => None,
            })
            .collect();
        // Every commit fits one API request (≤ 10,000 operations; limits are never raised).
        assert!(
            commits
                .iter()
                .all(|c| c.adds.len() + c.deletes.len() <= 10_000)
        );
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
        // Every diff pair and every final head is retained in full.
        for (x, y) in &a.diff_pairs {
            assert!(a.expected[x].state.is_some() && a.expected[y].state.is_some());
        }
    }

    #[test]
    fn the_oracle_states_are_consistent_with_the_steps() {
        // Replaying the generated steps with plain set algebra reproduces every recorded
        // digest (the oracle's own bookkeeping is self-consistent).
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

    #[test]
    fn designed_conflicts_resolve_per_strategy() {
        let w = generate(&Params::ci());
        let m = w
            .steps
            .iter()
            .find_map(|s| match s {
                Step::Merge(m) if m.previews.len() == 4 => Some(m),
                _ => None,
            })
            .expect("the conflict scenario");
        let by: BTreeMap<&str, &PreviewExpect> =
            m.previews.iter().map(|p| (p.strategy, p)).collect();
        assert_eq!(by["abort"].classification, "conflicted");
        assert!(by["abort"].merged.is_none());
        assert_eq!(by["abort"].conflict_count, 6);
        let t = by["take-target"].merged.as_ref().unwrap();
        let s = by["take-source"].merged.as_ref().unwrap();
        let u = by["union"].merged.as_ref().unwrap();
        assert!(t.is_subset(u) && s.is_subset(u) && t != s);
        assert_eq!(u.len(), t.union(s).count());
    }

    #[test]
    #[ignore = "≈90 s in a debug build; `ledger-bench validate --profile local` (release, in scripts/benchmark.sh) checks the same generation and its manifest"]
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
