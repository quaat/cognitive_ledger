use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ledger_core::{CommitId, ContentId};

use super::*;

fn id(n: u64) -> CommitId {
    CommitId(ContentId::for_bytes(
        format!("ledger-dag-test-{n}").as_bytes(),
    ))
}

fn ids(ns: &[u64]) -> Vec<CommitId> {
    ns.iter().copied().map(id).collect()
}

const L: TraversalLimits = TraversalLimits::DEFAULT;

fn limit(max_visited: usize) -> TraversalLimits {
    TraversalLimits {
        max_visited,
        deadline: None,
    }
}

/// Linear history 0 <- 1 <- ... <- n-1 (commit i has first parent i-1).
fn linear(n: u64) -> MemoryDag {
    let mut dag = MemoryDag::new();
    for i in 0..n {
        let parents = if i == 0 { vec![] } else { vec![id(i - 1)] };
        dag.insert(id(i), parents);
    }
    dag
}

/// Wraps a provider and counts calls per commit.
struct Counting<P> {
    inner: P,
    calls: Mutex<HashMap<CommitId, usize>>,
}

impl<P> Counting<P> {
    fn new(inner: P) -> Self {
        Self {
            inner,
            calls: Mutex::new(HashMap::new()),
        }
    }
    fn total(&self) -> usize {
        self.calls.lock().unwrap().values().sum()
    }
    fn max_per_commit(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .values()
            .copied()
            .max()
            .unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl<P: ParentProvider> ParentProvider for Counting<P> {
    type Error = P::Error;
    async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, Self::Error> {
        *self
            .calls
            .lock()
            .unwrap()
            .entry(commit.clone())
            .or_default() += 1;
        self.inner.parents(commit).await
    }
}

/// Sleeps (blocking) on every call.
struct Slow(MemoryDag, Duration);

#[async_trait::async_trait]
impl ParentProvider for Slow {
    type Error = std::convert::Infallible;
    async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, Self::Error> {
        std::thread::sleep(self.1);
        self.0.parents(commit).await
    }
}

#[derive(Debug, thiserror::Error)]
#[error("backend down")]
struct Down;

/// Fails for one specific commit.
struct Failing(MemoryDag, CommitId);

#[async_trait::async_trait]
impl ParentProvider for Failing {
    type Error = Down;
    async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, Self::Error> {
        if *commit == self.1 {
            return Err(Down);
        }
        Ok(self.0.parents(commit).await.unwrap())
    }
}

// ---------------------------------------------------------------- linear histories

#[tokio::test]
async fn linear_history() {
    let dag = linear(5);
    assert!(is_ancestor(&dag, &id(0), &id(4), L).await.unwrap());
    assert!(is_ancestor(&dag, &id(2), &id(4), L).await.unwrap());
    assert!(is_ancestor(&dag, &id(4), &id(4), L).await.unwrap());
    assert!(!is_ancestor(&dag, &id(4), &id(2), L).await.unwrap());
    assert!(!is_ancestor(&dag, &id(99), &id(4), L).await.unwrap());
    assert_eq!(
        first_parent_history(&dag, &id(4), 10, L).await.unwrap(),
        ids(&[4, 3, 2, 1, 0])
    );
    assert_eq!(
        first_parent_history(&dag, &id(4), 2, L).await.unwrap(),
        ids(&[4, 3])
    );
    assert!(
        first_parent_history(&dag, &id(4), 0, L)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ancestors(&dag, &id(4), L).await.unwrap(),
        ids(&[4, 3, 2, 1, 0])
    );
    assert_eq!(ancestors(&dag, &id(0), L).await.unwrap(), ids(&[0]));
}

#[tokio::test]
async fn unknown_start_commit() {
    let dag = linear(3);
    assert!(matches!(
        is_ancestor(&dag, &id(0), &id(7), L).await,
        Err(DagError::UnknownCommit(c)) if c == id(7)
    ));
    // Even ancestor == descendant requires the commit to be known.
    assert!(matches!(
        is_ancestor(&dag, &id(7), &id(7), L).await,
        Err(DagError::UnknownCommit(c)) if c == id(7)
    ));
    assert!(matches!(
        first_parent_history(&dag, &id(7), 5, L).await,
        Err(DagError::UnknownCommit(c)) if c == id(7)
    ));
    assert!(matches!(
        ancestors(&dag, &id(7), L).await,
        Err(DagError::UnknownCommit(c)) if c == id(7)
    ));
}

// ---------------------------------------------------------------- forks and merges

#[tokio::test]
async fn forks_are_not_mutually_reachable() {
    // 0 <- 1 <- 2 (branch a), 1 <- 3 <- 4 (branch b)
    let dag = MemoryDag::new()
        .commit(id(0), [])
        .commit(id(1), [id(0)])
        .commit(id(2), [id(1)])
        .commit(id(3), [id(1)])
        .commit(id(4), [id(3)]);
    assert!(is_ancestor(&dag, &id(1), &id(2), L).await.unwrap());
    assert!(is_ancestor(&dag, &id(1), &id(4), L).await.unwrap());
    assert!(!is_ancestor(&dag, &id(2), &id(4), L).await.unwrap());
    assert!(!is_ancestor(&dag, &id(3), &id(2), L).await.unwrap());
    assert_eq!(
        ancestors(&dag, &id(4), L).await.unwrap(),
        ids(&[4, 3, 1, 0])
    );
}

#[tokio::test]
async fn two_parent_commits() {
    // 0 <- 1 <- 2 ; 0 <- 3 ; 4 = merge(first 2, second 3)
    let dag = MemoryDag::new()
        .commit(id(0), [])
        .commit(id(1), [id(0)])
        .commit(id(2), [id(1)])
        .commit(id(3), [id(0)])
        .commit(id(4), [id(2), id(3)]);
    // Reachable only through the second parent.
    assert!(is_ancestor(&dag, &id(3), &id(4), L).await.unwrap());
    assert!(is_ancestor(&dag, &id(1), &id(4), L).await.unwrap());
    assert!(!is_ancestor(&dag, &id(4), &id(3), L).await.unwrap());
    // First-parent history skips the second parent.
    assert_eq!(
        first_parent_history(&dag, &id(4), 10, L).await.unwrap(),
        ids(&[4, 2, 1, 0])
    );
    // DFS preorder, parents in position order; 0 is discovered via the first parent.
    assert_eq!(
        ancestors(&dag, &id(4), L).await.unwrap(),
        ids(&[4, 2, 1, 0, 3])
    );
}

#[tokio::test]
async fn duplicate_parent_entry_is_not_a_cycle() {
    let dag = MemoryDag::new()
        .commit(id(0), [])
        .commit(id(1), [id(0), id(0)]);
    assert_eq!(ancestors(&dag, &id(1), L).await.unwrap(), ids(&[1, 0]));
    assert!(!is_ancestor(&dag, &id(9), &id(1), L).await.unwrap());
}

// ---------------------------------------------------------------- deep history

#[tokio::test]
async fn deep_linear_history_does_not_overflow() {
    const N: u64 = 50_000;
    let dag = linear(N);
    let head = id(N - 1);
    assert!(is_ancestor(&dag, &id(0), &head, L).await.unwrap());
    assert!(!is_ancestor(&dag, &id(N + 1), &head, L).await.unwrap());
    let all = ancestors(&dag, &head, L).await.unwrap();
    assert_eq!(all.len(), N as usize);
    assert_eq!(all.first(), Some(&head));
    assert_eq!(all.last(), Some(&id(0)));
    let fp = first_parent_history(&dag, &head, usize::MAX, L)
        .await
        .unwrap();
    assert_eq!(fp, all);
}

#[tokio::test]
async fn visit_limit_on_deep_history() {
    const N: u64 = 50_000;
    let dag = linear(N);
    let head = id(N - 1);
    assert!(matches!(
        is_ancestor(&dag, &id(0), &head, limit(1_000)).await,
        Err(DagError::VisitLimit { visited: 1_000 })
    ));
    assert!(matches!(
        ancestors(&dag, &head, limit(1_000)).await,
        Err(DagError::VisitLimit { visited: 1_000 })
    ));
    assert!(matches!(
        first_parent_history(&dag, &head, usize::MAX, limit(1_000)).await,
        Err(DagError::VisitLimit { visited: 1_000 })
    ));
    // Exactly at the limit succeeds; a hit within the limit succeeds.
    let small = linear(10);
    assert_eq!(
        ancestors(&small, &id(9), limit(10)).await.unwrap().len(),
        10
    );
    assert!(matches!(
        ancestors(&small, &id(9), limit(9)).await,
        Err(DagError::VisitLimit { visited: 9 })
    ));
    assert!(
        is_ancestor(&dag, &id(N - 500), &head, limit(1_000))
            .await
            .unwrap()
    );
    assert_eq!(
        first_parent_history(&dag, &head, 1_000, limit(1_000))
            .await
            .unwrap()
            .len(),
        1_000
    );
    assert!(matches!(
        ancestors(&small, &id(9), limit(0)).await,
        Err(DagError::VisitLimit { visited: 0 })
    ));
}

// ---------------------------------------------------------------- duplicate ancestry paths

#[tokio::test]
async fn diamond_visits_each_commit_once() {
    // 0 <- 1, 0 <- 2, 3 = merge(1, 2)
    let dag = Counting::new(
        MemoryDag::new()
            .commit(id(0), [])
            .commit(id(1), [id(0)])
            .commit(id(2), [id(0)])
            .commit(id(3), [id(1), id(2)]),
    );
    assert_eq!(
        ancestors(&dag, &id(3), L).await.unwrap(),
        ids(&[3, 1, 0, 2])
    );
    assert_eq!(dag.total(), 4);
    assert_eq!(dag.max_per_commit(), 1);
}

/// A ladder of `rungs` diamonds: naive path enumeration would be 2^rungs.
fn ladder(rungs: u64) -> (MemoryDag, CommitId, CommitId) {
    let mut dag = MemoryDag::new();
    let mut n = 0u64;
    let root = id(n);
    dag.insert(root.clone(), []);
    let mut top = root.clone();
    for _ in 0..rungs {
        let left = id(n + 1);
        let right = id(n + 2);
        let join = id(n + 3);
        n += 3;
        dag.insert(left.clone(), [top.clone()]);
        dag.insert(right.clone(), [top.clone()]);
        dag.insert(join.clone(), [left, right]);
        top = join;
    }
    (dag, root, top)
}

#[tokio::test]
async fn diamond_ladder_visits_each_commit_once() {
    const RUNGS: u64 = 200;
    let commits = (3 * RUNGS + 1) as usize;
    let (dag, root, head) = ladder(RUNGS);
    let dag = Counting::new(dag);

    let all = ancestors(&dag, &head, L).await.unwrap();
    assert_eq!(all.len(), commits);
    assert_eq!(all.iter().collect::<HashSet<_>>().len(), commits);
    assert_eq!(dag.total(), commits);
    assert_eq!(dag.max_per_commit(), 1);

    // A miss searches everything, still once per commit.
    let dag = Counting::new(ladder(RUNGS).0);
    assert!(!is_ancestor(&dag, &id(u64::MAX), &head, L).await.unwrap());
    assert_eq!(dag.total(), commits);
    assert_eq!(dag.max_per_commit(), 1);

    let dag = Counting::new(ladder(RUNGS).0);
    assert!(is_ancestor(&dag, &root, &head, L).await.unwrap());
    assert!(dag.total() <= commits);
    assert_eq!(dag.max_per_commit(), 1);

    // The visit limit counts distinct commits, not paths.
    let (dag, _, head) = ladder(RUNGS);
    assert_eq!(
        ancestors(&dag, &head, limit(commits)).await.unwrap().len(),
        commits
    );
}

// ---------------------------------------------------------------- malformed providers

#[tokio::test]
async fn missing_parent_is_unknown_commit() {
    let dag = MemoryDag::new()
        .commit(id(1), [id(0)])
        .commit(id(2), [id(1)]);
    for result in [
        is_ancestor(&dag, &id(9), &id(2), L).await.map(|_| ()),
        ancestors(&dag, &id(2), L).await.map(|_| ()),
        first_parent_history(&dag, &id(2), 10, L).await.map(|_| ()),
    ] {
        assert!(matches!(result, Err(DagError::UnknownCommit(c)) if c == id(0)));
    }
    // Missing second parent.
    let dag = MemoryDag::new()
        .commit(id(0), [])
        .commit(id(2), [id(0), id(1)]);
    assert!(matches!(
        ancestors(&dag, &id(2), L).await,
        Err(DagError::UnknownCommit(c)) if c == id(1)
    ));
    // First-parent history does not look at the second parent.
    assert_eq!(
        first_parent_history(&dag, &id(2), 10, L).await.unwrap(),
        ids(&[2, 0])
    );
}

#[tokio::test]
async fn self_loop_is_a_cycle() {
    let dag = MemoryDag::new().commit(id(0), [id(0)]);
    assert!(matches!(
        ancestors(&dag, &id(0), L).await,
        Err(DagError::Cycle(c)) if c == id(0)
    ));
    assert!(matches!(
        is_ancestor(&dag, &id(5), &id(0), L).await,
        Err(DagError::Cycle(_))
    ));
    assert!(matches!(
        first_parent_history(&dag, &id(0), 10, L).await,
        Err(DagError::Cycle(c)) if c == id(0)
    ));
    // Early exit on the start commit itself is allowed.
    assert!(is_ancestor(&dag, &id(0), &id(0), L).await.unwrap());
}

#[tokio::test]
async fn three_cycle_is_detected() {
    // 3 -> 2 -> 1 -> 0 -> 2
    let dag = MemoryDag::new()
        .commit(id(3), [id(2)])
        .commit(id(2), [id(1)])
        .commit(id(1), [id(0)])
        .commit(id(0), [id(2)]);
    assert!(matches!(
        ancestors(&dag, &id(3), L).await,
        Err(DagError::Cycle(c)) if c == id(2)
    ));
    assert!(matches!(
        is_ancestor(&dag, &id(9), &id(3), L).await,
        Err(DagError::Cycle(_))
    ));
    assert!(matches!(
        first_parent_history(&dag, &id(3), 100, L).await,
        Err(DagError::Cycle(c)) if c == id(2)
    ));
    // A hit before the cycle closes is allowed.
    assert!(is_ancestor(&dag, &id(1), &id(3), L).await.unwrap());
}

#[tokio::test]
async fn cycle_reachable_only_through_second_parent() {
    // 4 = merge(first 1, second 3); 1 -> 0 (root); 3 -> 2 -> 3 (cycle)
    let dag = MemoryDag::new()
        .commit(id(0), [])
        .commit(id(1), [id(0)])
        .commit(id(2), [id(3)])
        .commit(id(3), [id(2)])
        .commit(id(4), [id(1), id(3)]);
    assert!(matches!(
        ancestors(&dag, &id(4), L).await,
        Err(DagError::Cycle(_))
    ));
    assert!(matches!(
        is_ancestor(&dag, &id(9), &id(4), L).await,
        Err(DagError::Cycle(_))
    ));
    // The first-parent chain is acyclic.
    assert_eq!(
        first_parent_history(&dag, &id(4), 10, L).await.unwrap(),
        ids(&[4, 1, 0])
    );
    // Hit through the first parent before the cycle is explored.
    assert!(is_ancestor(&dag, &id(0), &id(4), L).await.unwrap());
}

#[tokio::test]
async fn provider_error_is_propagated() {
    let dag = Failing(linear(5), id(2));
    assert!(matches!(
        ancestors(&dag, &id(4), L).await,
        Err(DagError::Provider(Down))
    ));
    assert!(matches!(
        is_ancestor(&dag, &id(0), &id(4), L).await,
        Err(DagError::Provider(Down))
    ));
    assert!(is_ancestor(&dag, &id(3), &id(4), L).await.unwrap());
    assert!(matches!(
        first_parent_history(&dag, &id(4), 10, L).await,
        Err(DagError::Provider(Down))
    ));
}

// ---------------------------------------------------------------- deadline

#[tokio::test]
async fn deadline_in_the_past() {
    let dag = Counting::new(linear(5));
    let limits = TraversalLimits {
        max_visited: 100,
        deadline: Some(Instant::now()),
    };
    assert!(matches!(
        ancestors(&dag, &id(4), limits).await,
        Err(DagError::Deadline)
    ));
    assert!(matches!(
        is_ancestor(&dag, &id(4), &id(4), limits).await,
        Err(DagError::Deadline)
    ));
    assert!(matches!(
        first_parent_history(&dag, &id(4), 3, limits).await,
        Err(DagError::Deadline)
    ));
    // The deadline is checked before the provider is asked.
    assert_eq!(dag.total(), 0);
}

#[tokio::test]
async fn deadline_during_slow_traversal() {
    let dag = Counting::new(Slow(linear(1_000), Duration::from_millis(2)));
    let limits = TraversalLimits {
        max_visited: 100_000,
        deadline: Some(Instant::now() + Duration::from_millis(30)),
    };
    assert!(matches!(
        ancestors(&dag, &id(999), limits).await,
        Err(DagError::Deadline)
    ));
    assert!(dag.total() < 1_000, "stopped early: {}", dag.total());
}

// ---------------------------------------------------------------- generated DAGs

/// xorshift64* (deterministic, dependency-free).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Generated {
    dag: MemoryDag,
    /// parents by index, in position order.
    parents: Vec<Vec<usize>>,
    /// reach[i] = indices reachable from i, including i (brute-force transitive closure).
    reach: Vec<BTreeSet<usize>>,
}

fn generate(seed: u64) -> Generated {
    let mut rng = Rng::new(seed);
    let n = 1 + rng.below(40) as usize;
    let mut parents: Vec<Vec<usize>> = Vec::with_capacity(n);
    for i in 0..n {
        let ps = if i == 0 || rng.below(10) == 0 {
            vec![] // roots, including occasional extra roots
        } else if rng.below(3) == 0 {
            vec![rng.below(i as u64) as usize, rng.below(i as u64) as usize]
        } else {
            vec![rng.below(i as u64) as usize]
        };
        parents.push(ps);
    }
    let mut reach: Vec<BTreeSet<usize>> = Vec::with_capacity(n);
    for (i, ps) in parents.iter().enumerate() {
        let mut r = BTreeSet::from([i]);
        for &p in ps {
            r.extend(reach[p].iter().copied());
        }
        reach.push(r);
    }
    let mut dag = MemoryDag::new();
    for (i, ps) in parents.iter().enumerate() {
        dag.insert(id(i as u64), ps.iter().map(|&p| id(p as u64)));
    }
    Generated {
        dag,
        parents,
        reach,
    }
}

const SEEDS: u64 = 300;

#[tokio::test]
async fn generated_is_ancestor_matches_oracle() {
    for seed in 0..SEEDS {
        let g = generate(seed);
        let n = g.parents.len();
        for d in 0..n {
            for a in 0..n {
                let got = is_ancestor(&g.dag, &id(a as u64), &id(d as u64), L).await;
                let want = g.reach[d].contains(&a);
                assert!(
                    matches!(got, Ok(v) if v == want),
                    "seed {seed}: is_ancestor({a}, {d}) = {got:?}, oracle {want}"
                );
            }
        }
    }
}

#[tokio::test]
async fn generated_ancestors_matches_oracle() {
    for seed in 0..SEEDS {
        let g = generate(seed);
        let n = g.parents.len();
        for h in 0..n {
            let dag = Counting::new(g.dag.clone());
            let got = ancestors(&dag, &id(h as u64), L).await;
            let got = got.unwrap_or_else(|e| panic!("seed {seed}: ancestors({h}) failed: {e:?}"));
            let want: HashSet<CommitId> = g.reach[h].iter().map(|&i| id(i as u64)).collect();
            assert_eq!(got.len(), want.len(), "seed {seed}: head {h} length");
            assert_eq!(
                got.iter().cloned().collect::<HashSet<_>>(),
                want,
                "seed {seed}: head {h} set"
            );
            assert_eq!(got.first(), Some(&id(h as u64)), "seed {seed}: head first");
            assert_eq!(dag.max_per_commit(), 1, "seed {seed}: head {h} revisits");
            let again = ancestors(&g.dag, &id(h as u64), L).await.unwrap();
            assert_eq!(got, again, "seed {seed}: head {h} nondeterministic order");

            // First-parent history against a direct walk of the generated parents.
            let mut want_fp = vec![id(h as u64)];
            let mut cur = h;
            while let Some(&p) = g.parents[cur].first() {
                want_fp.push(id(p as u64));
                cur = p;
            }
            let fp = first_parent_history(&g.dag, &id(h as u64), usize::MAX, L).await;
            assert!(
                matches!(&fp, Ok(v) if *v == want_fp),
                "seed {seed}: first_parent_history({h}) = {fp:?}"
            );
        }
    }
}

#[tokio::test]
async fn generated_back_edge_is_a_cycle() {
    let mut injected = 0;
    for seed in 0..SEEDS {
        let mut g = generate(seed);
        let n = g.parents.len();
        let mut rng = Rng::new(seed ^ 0xC1C1_E5ED);
        // Pick i < j with i reachable from j and add edge i -> j (parent j): i -> j ->* i.
        let candidates: Vec<(usize, usize)> = (0..n)
            .flat_map(|j| g.reach[j].iter().map(move |&i| (i, j)))
            .filter(|&(i, j)| i != j)
            .collect();
        let (i, j) = if candidates.is_empty() {
            let k = rng.below(n as u64) as usize;
            (k, k) // self-loop
        } else {
            candidates[rng.below(candidates.len() as u64) as usize]
        };
        let pos = rng.below(g.parents[i].len() as u64 + 1) as usize;
        g.parents[i].insert(pos, j);
        g.dag
            .insert(id(i as u64), g.parents[i].iter().map(|&p| id(p as u64)));
        injected += 1;

        // Every head that reaches i (in the original closure, or j itself) reaches the cycle.
        for h in 0..n {
            if !(g.reach[h].contains(&i) || h == j) {
                continue;
            }
            let got = ancestors(&g.dag, &id(h as u64), L).await;
            assert!(
                matches!(got, Err(DagError::Cycle(_))),
                "seed {seed}: ancestors({h}) with back edge {i}->{j} = {got:?}"
            );
            let got = is_ancestor(&g.dag, &id(u64::MAX), &id(h as u64), L).await;
            assert!(
                matches!(got, Err(DagError::Cycle(_))),
                "seed {seed}: is_ancestor(absent, {h}) with back edge {i}->{j} = {got:?}"
            );
        }
    }
    assert_eq!(injected, SEEDS);
}

// ---------------------------------------------------------------------------------------
// Phase 5 (ADR-0024): merge base, ahead/behind, analysis — against the brute-force closure
// ---------------------------------------------------------------------------------------

/// Slow reference: the maximal elements of reach[t] ∩ reach[s] (c is dominated if it is
/// reachable from another common ancestor d ≠ c), ascending by commit id.
fn oracle_base(g: &Generated, t: usize, s: usize) -> MergeBase {
    let common: Vec<usize> = g.reach[t].intersection(&g.reach[s]).copied().collect();
    let mut best: Vec<CommitId> = common
        .iter()
        .filter(|&&c| !common.iter().any(|&d| d != c && g.reach[d].contains(&c)))
        .map(|&c| id(c as u64))
        .collect();
    best.sort();
    match best.len() {
        0 => MergeBase::Unrelated,
        1 => MergeBase::Unique(best.remove(0)),
        _ => MergeBase::Ambiguous(best),
    }
}

#[tokio::test]
async fn generated_merge_base_and_analysis_match_oracle() {
    // All ordered pairs of every generated DAG against a cubic oracle: 100 seeds keep the
    // debug-mode run short while still covering every outcome (asserted below).
    const MERGE_SEEDS: u64 = 100;
    let (mut unique, mut ambiguous, mut unrelated) = (0, 0, 0);
    for seed in 0..MERGE_SEEDS {
        let g = generate(seed);
        let n = g.parents.len();
        for t in 0..n {
            for s in 0..n {
                let (ti, si) = (id(t as u64), id(s as u64));
                let want = oracle_base(&g, t, s);
                match &want {
                    MergeBase::Unique(_) => unique += 1,
                    MergeBase::Ambiguous(_) => ambiguous += 1,
                    MergeBase::Unrelated => unrelated += 1,
                }
                let ahead = g.reach[s].difference(&g.reach[t]).count();
                let behind = g.reach[t].difference(&g.reach[s]).count();
                // The standalone entry points on a deterministic half of the pairs (the
                // shared `ancestry` core is covered on every pair through `analyze`).
                if (t + s + seed as usize) % 2 == 0 {
                    // Symmetric by definition.
                    assert_eq!(merge_base(&g.dag, &si, &ti, L).await.unwrap(), want);
                    assert_eq!(
                        ahead_behind(&g.dag, &ti, &si, L).await.unwrap(),
                        (ahead, behind),
                        "seed {seed}: ahead_behind({t}, {s})"
                    );
                }
                let a = analyze(&g.dag, &ti, &si, L).await.unwrap();
                let relation = if t == s {
                    Relation::Equal
                } else if g.reach[t].contains(&s) {
                    Relation::SourceContained
                } else if g.reach[s].contains(&t) {
                    Relation::FastForward
                } else {
                    Relation::Divergent(want.clone())
                };
                assert_eq!(
                    a,
                    Analysis {
                        relation,
                        ahead: if t == s { 0 } else { ahead },
                        behind: if t == s { 0 } else { behind },
                    },
                    "seed {seed}: analyze({t}, {s})"
                );
                // The unique base of a divergent pair is a common ancestor of both.
                if let Relation::Divergent(MergeBase::Unique(b)) = &a.relation {
                    assert!(is_ancestor(&g.dag, b, &ti, L).await.unwrap());
                    assert!(is_ancestor(&g.dag, b, &si, L).await.unwrap());
                }
            }
        }
    }
    // The generator must actually exercise every outcome.
    assert!(
        unique > 0 && ambiguous > 0 && unrelated > 0,
        "{unique}/{ambiguous}/{unrelated}"
    );
}

/// Fork from 0: target 0 <- 1 <- 2, source 0 <- 3 <- 4.
fn fork() -> MemoryDag {
    MemoryDag::new()
        .commit(id(0), [])
        .commit(id(1), [id(0)])
        .commit(id(2), [id(1)])
        .commit(id(3), [id(0)])
        .commit(id(4), [id(3)])
}

#[tokio::test]
async fn fixtures_linear_fork_nested_and_diamond() {
    let lin = linear(10);
    let a = analyze(&lin, &id(3), &id(7), L).await.unwrap();
    assert_eq!(
        a,
        Analysis {
            relation: Relation::FastForward,
            ahead: 4,
            behind: 0
        }
    );
    let a = analyze(&lin, &id(7), &id(3), L).await.unwrap();
    assert_eq!(
        a,
        Analysis {
            relation: Relation::SourceContained,
            ahead: 0,
            behind: 4
        }
    );
    assert_eq!(
        analyze(&lin, &id(5), &id(5), L).await.unwrap().relation,
        Relation::Equal
    );

    let f = fork();
    let a = analyze(&f, &id(2), &id(4), L).await.unwrap();
    assert_eq!(
        a,
        Analysis {
            relation: Relation::Divergent(MergeBase::Unique(id(0))),
            ahead: 2,
            behind: 2
        }
    );
    // Nested fork: 5 branches from 3; base(2, 5) is still 0, base(4, 5) is 3.
    let nested = fork().commit(id(5), [id(3)]);
    assert_eq!(
        merge_base(&nested, &id(2), &id(5), L).await.unwrap(),
        MergeBase::Unique(id(0))
    );
    assert_eq!(
        merge_base(&nested, &id(4), &id(5), L).await.unwrap(),
        MergeBase::Unique(id(3))
    );
    // Diamond: 0 <- {1, 2} <- 3 = [1, 2]; then 4 <- 3 and 5 <- 1: base(4, 5) = 1.
    let diamond = MemoryDag::new()
        .commit(id(0), [])
        .commit(id(1), [id(0)])
        .commit(id(2), [id(0)])
        .commit(id(3), [id(1), id(2)])
        .commit(id(4), [id(3)])
        .commit(id(5), [id(1)]);
    assert_eq!(
        merge_base(&diamond, &id(4), &id(5), L).await.unwrap(),
        MergeBase::Unique(id(1))
    );
}

#[tokio::test]
async fn repeated_merge_and_ping_pong_terminate_in_containment() {
    // target 2, source 4 diverge from 0; integration I = 10 = [2, 4] (ADR-0023).
    let mut dag = fork().commit(id(10), [id(2), id(4)]);
    // Repeating the merge: the source is contained.
    assert_eq!(
        analyze(&dag, &id(10), &id(4), L).await.unwrap().relation,
        Relation::SourceContained
    );
    // The source advances (5 <- 4): divergent again, base = the old source head.
    dag.insert(id(5), [id(4)]);
    let a = analyze(&dag, &id(10), &id(5), L).await.unwrap();
    assert_eq!(a.relation, Relation::Divergent(MergeBase::Unique(id(4))));
    assert_eq!((a.ahead, a.behind), (1, 3));
    // Merging the target back into the source (source branch at 5, incoming 10 + 11 = [10, 5]):
    // integration on the source side is fast-forward class, and afterwards both directions
    // are contained — ping-pong terminates.
    dag.insert(id(11), [id(10), id(5)]);
    assert_eq!(
        analyze(&dag, &id(5), &id(11), L).await.unwrap().relation,
        Relation::FastForward
    );
    dag.insert(id(12), [id(5), id(11)]); // the source's integration commit
    assert_eq!(
        analyze(&dag, &id(12), &id(11), L).await.unwrap().relation,
        Relation::SourceContained
    );
    assert_eq!(
        analyze(&dag, &id(11), &id(12), L).await.unwrap().relation,
        Relation::FastForward
    );
}

#[tokio::test]
async fn criss_cross_is_ambiguous_and_never_picks_one() {
    // 1 and 2 fork from 0; 3 = [1, 2] and 4 = [2, 1] (criss-cross); 5 <- 3, 6 <- 4.
    let dag = MemoryDag::new()
        .commit(id(0), [])
        .commit(id(1), [id(0)])
        .commit(id(2), [id(0)])
        .commit(id(3), [id(1), id(2)])
        .commit(id(4), [id(2), id(1)])
        .commit(id(5), [id(3)])
        .commit(id(6), [id(4)]);
    let mut want = vec![id(1), id(2)];
    want.sort();
    assert_eq!(
        merge_base(&dag, &id(5), &id(6), L).await.unwrap(),
        MergeBase::Ambiguous(want.clone())
    );
    assert_eq!(
        analyze(&dag, &id(5), &id(6), L).await.unwrap().relation,
        Relation::Divergent(MergeBase::Ambiguous(want))
    );
}

#[tokio::test]
async fn unrelated_missing_cycle_and_limits_fail_closed() {
    let two_roots = MemoryDag::new()
        .commit(id(0), [])
        .commit(id(1), [id(0)])
        .commit(id(2), [])
        .commit(id(3), [id(2)]);
    assert_eq!(
        merge_base(&two_roots, &id(1), &id(3), L).await.unwrap(),
        MergeBase::Unrelated
    );
    let missing = MemoryDag::new().commit(id(1), [id(0)]).commit(id(2), []);
    assert!(matches!(
        merge_base(&missing, &id(1), &id(2), L).await,
        Err(DagError::UnknownCommit(c)) if c == id(0)
    ));
    assert!(matches!(
        analyze(&missing, &id(2), &id(1), L).await,
        Err(DagError::UnknownCommit(_))
    ));
    let cycle = MemoryDag::new()
        .commit(id(1), [id(2)])
        .commit(id(2), [id(1)])
        .commit(id(3), []);
    assert!(matches!(
        merge_base(&cycle, &id(1), &id(3), L).await,
        Err(DagError::Cycle(_))
    ));
    // Deep history: 10 000 linear commits plus a fork at the top; bounded both ways.
    let mut deep = linear(10_000);
    deep.insert(id(20_000), [id(9_998)]);
    let a = analyze(&deep, &id(9_999), &id(20_000), L).await.unwrap();
    assert_eq!(
        a.relation,
        Relation::Divergent(MergeBase::Unique(id(9_998)))
    );
    assert_eq!((a.ahead, a.behind), (1, 1));
    assert!(matches!(
        analyze(&deep, &id(9_999), &id(20_000), limit(5_000)).await,
        Err(DagError::VisitLimit { .. })
    ));
}

// ---------------------------------------------------------------------------------------
// Plan 0012: windowed retrieval — a window is a prefetch, never an answer
// ---------------------------------------------------------------------------------------

/// Forwards windows to an inner provider and counts the window calls.
struct CountingWindows<P> {
    inner: P,
    windows: Mutex<usize>,
}

impl<P> CountingWindows<P> {
    fn new(inner: P) -> Self {
        Self {
            inner,
            windows: Mutex::new(0),
        }
    }
    fn windows(&self) -> usize {
        *self.windows.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl<P: ParentProvider> ParentProvider for CountingWindows<P> {
    type Error = P::Error;
    async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, Self::Error> {
        self.inner.parents(commit).await
    }
    async fn ancestry_window(
        &self,
        start: &CommitId,
        max: usize,
        kind: WindowKind,
    ) -> Result<Vec<(CommitId, Vec<CommitId>)>, Self::Error> {
        *self.windows.lock().unwrap() += 1;
        self.inner.ancestry_window(start, max, kind).await
    }
}

const WINDOWS: [usize; 6] = [1, 2, 3, 7, 64, 100_000];

#[tokio::test]
async fn windows_do_not_change_any_answer_on_generated_dags() {
    // Every window size against the unwindowed provider (window 1 is the reference, and it
    // is also checked against the brute-force closure by the tests above): ancestors, the
    // first-parent history, reachability of every pair, and the full merge analysis with
    // both recorded ancestries.
    for seed in 0..60 {
        let g = generate(seed);
        let n = g.parents.len();
        for &window in &WINDOWS[1..] {
            let wide = g.dag.clone().with_window(window);
            for h in 0..n {
                let head = id(h as u64);
                assert_eq!(
                    ancestors(&wide, &head, L).await.unwrap(),
                    ancestors(&g.dag, &head, L).await.unwrap(),
                    "seed {seed} window {window}: ancestors({h})"
                );
                assert_eq!(
                    first_parent_history(&wide, &head, usize::MAX, L)
                        .await
                        .unwrap(),
                    first_parent_history(&g.dag, &head, usize::MAX, L)
                        .await
                        .unwrap(),
                    "seed {seed} window {window}: first_parent_history({h})"
                );
                for a in 0..n {
                    let want = is_ancestor(&g.dag, &id(a as u64), &head, L).await.unwrap();
                    let got = is_ancestor(&wide, &id(a as u64), &head, L).await.unwrap();
                    assert_eq!(
                        got, want,
                        "seed {seed} window {window}: is_ancestor({a}, {h})"
                    );
                }
            }
            for t in 0..n {
                for s in 0..n {
                    let (target, source) = (id(t as u64), id(s as u64));
                    let want = analyze_with_ancestries(&g.dag, &target, &source, L)
                        .await
                        .unwrap();
                    let got = analyze_with_ancestries(&wide, &target, &source, L)
                        .await
                        .unwrap();
                    assert_eq!(
                        got.0, want.0,
                        "seed {seed} window {window}: analyze({t}, {s})"
                    );
                    assert_eq!(
                        got.1.difference(&got.2),
                        want.1.difference(&want.2),
                        "seed {seed} window {window}: target-only({t}, {s})"
                    );
                    assert_eq!(
                        got.2.difference(&got.1),
                        want.2.difference(&want.1),
                        "seed {seed} window {window}: source-only({t}, {s})"
                    );
                    assert_eq!(
                        best_common_ancestors(&got.1, &got.2),
                        best_common_ancestors(&want.1, &want.2),
                        "seed {seed} window {window}: best common ancestors({t}, {s})"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn windows_cut_provider_calls_on_deep_linear_history() {
    const N: u64 = 5_000;
    let head = id(N - 1);
    for &window in &[1usize, 2, 3, 7, 64, 256, 1_000] {
        let dag = CountingWindows::new(linear(N).with_window(window));
        let all = ancestors(&dag, &head, L).await.unwrap();
        assert_eq!(all.len(), N as usize);
        assert_eq!(
            dag.windows(),
            window_calls(N as usize, window),
            "window {window}: ancestors"
        );
        let dag = CountingWindows::new(linear(N).with_window(window));
        let fp = first_parent_history(&dag, &head, usize::MAX, L)
            .await
            .unwrap();
        assert_eq!(fp, all);
        assert_eq!(
            dag.windows(),
            window_calls(N as usize, window),
            "window {window}: first_parent_history"
        );
        // A hit stops the search; the ramp keeps a near hit cheap and a far hit costs at
        // most the windows that cover the commits entered.
        let dag = CountingWindows::new(linear(N).with_window(window));
        assert!(is_ancestor(&dag, &id(N - 1_000), &head, L).await.unwrap());
        assert_eq!(
            dag.windows(),
            window_calls(1_000, window),
            "window {window}: hit"
        );
        let dag = CountingWindows::new(linear(N).with_window(window));
        assert!(is_ancestor(&dag, &id(N - 2), &head, L).await.unwrap());
        assert_eq!(
            dag.windows(),
            window_calls(2, window),
            "window {window}: near hit"
        );
        assert_eq!(window_calls(2, 256), 2);
        // Merge analysis of two heads on one chain: one walk per side, each with its own
        // ramp.
        let dag = CountingWindows::new(linear(N).with_window(window));
        let analysis = analyze(&dag, &head, &id(N - 1_001), L).await.unwrap();
        assert_eq!(analysis.relation, Relation::SourceContained);
        assert_eq!((analysis.ahead, analysis.behind), (0, 1_000));
        assert_eq!(
            dag.windows(),
            window_calls(N as usize, window) + window_calls(N as usize - 1_000, window),
            "window {window}: analysis"
        );
    }
}

#[tokio::test]
async fn windows_keep_the_visit_limit_the_deadline_cycles_and_unknowns() {
    const N: u64 = 50_000;
    let head = id(N - 1);
    // The visit limit counts entered commits, not prefetched rows: a window of 1,000 with a
    // limit of 1,000 behaves exactly like the unwindowed walk, and a limit of 0 calls the
    // provider not at all.
    let dag = CountingWindows::new(linear(N).with_window(1_000));
    assert!(matches!(
        ancestors(&dag, &head, limit(1_000)).await,
        Err(DagError::VisitLimit { visited: 1_000 })
    ));
    assert_eq!(dag.windows(), window_calls(1_000, 1_000));
    assert!(matches!(
        first_parent_history(&dag, &head, usize::MAX, limit(1_000)).await,
        Err(DagError::VisitLimit { visited: 1_000 })
    ));
    assert!(
        is_ancestor(&dag, &id(N - 500), &head, limit(1_000))
            .await
            .unwrap()
    );
    let dag = CountingWindows::new(linear(10).with_window(1_000));
    assert!(matches!(
        ancestors(&dag, &id(9), limit(0)).await,
        Err(DagError::VisitLimit { visited: 0 })
    ));
    assert_eq!(dag.windows(), 0);
    // The window a walk asks for never exceeds its remaining budget: with a limit of 10 on a
    // 10-commit chain the ramp asks for 1, 4 and then the remaining 5.
    assert_eq!(ancestors(&dag, &id(9), limit(10)).await.unwrap().len(), 10);
    assert_eq!(dag.windows(), window_calls(10, 1_000));
    assert_eq!(window_calls(10, 1_000), 3);
    // A deadline in the past fails before any window.
    let past = TraversalLimits {
        max_visited: usize::MAX,
        deadline: Some(Instant::now() - Duration::from_millis(1)),
    };
    let dag = CountingWindows::new(linear(10).with_window(1_000));
    assert!(matches!(
        ancestors(&dag, &id(9), past).await,
        Err(DagError::Deadline)
    ));
    assert_eq!(dag.windows(), 0);

    // Cycles crossing a window boundary: 0 <- 1 <- 2 <- 3 <- 4, plus 0's parent is 4.
    for window in [1usize, 2, 3, 4, 5, 100] {
        let mut dag = linear(5).with_window(window);
        dag.insert(id(0), [id(4)]);
        assert!(
            matches!(ancestors(&dag, &id(4), L).await, Err(DagError::Cycle(c)) if c == id(4)),
            "window {window}"
        );
        assert!(
            matches!(first_parent_history(&dag, &id(4), usize::MAX, L).await, Err(DagError::Cycle(c)) if c == id(4)),
            "window {window}"
        );
        assert!(matches!(
            is_ancestor(&dag, &id(99), &id(4), L).await,
            Err(DagError::Cycle(_))
        ));
        // A hit before the cycle closes still succeeds, whatever the window prefetched.
        assert!(is_ancestor(&dag, &id(1), &id(4), L).await.unwrap());
    }
    // A missing parent crossing a window boundary is still UnknownCommit of that parent,
    // and an unknown start commit is UnknownCommit of the start.
    for window in [1usize, 2, 3, 4, 100] {
        let mut dag = linear(4).with_window(window);
        dag.insert(id(0), [id(77)]);
        assert!(
            matches!(ancestors(&dag, &id(3), L).await, Err(DagError::UnknownCommit(c)) if c == id(77)),
            "window {window}"
        );
        assert!(
            matches!(first_parent_history(&dag, &id(3), usize::MAX, L).await, Err(DagError::UnknownCommit(c)) if c == id(77)),
            "window {window}"
        );
        assert!(
            matches!(ancestors(&dag, &id(55), L).await, Err(DagError::UnknownCommit(c)) if c == id(55)),
            "window {window}"
        );
        // Only the first parent is needed by the first-parent history: a merge whose
        // second parent is unknown still lists its first-parent chain, with any window.
        let mut dag = linear(4).with_window(window);
        dag.insert(id(3), [id(2), id(88)]);
        assert_eq!(
            first_parent_history(&dag, &id(3), usize::MAX, L)
                .await
                .unwrap(),
            ids(&[3, 2, 1, 0]),
            "window {window}"
        );
        assert!(
            matches!(ancestors(&dag, &id(3), L).await, Err(DagError::UnknownCommit(c)) if c == id(88)),
            "window {window}"
        );
    }
}

#[tokio::test]
async fn a_window_entry_never_stands_in_for_the_anchor() {
    // A provider whose windows return other commits but not the anchor: the walk must not
    // take the anchor's presence for granted (UnknownCommit), and must not use an entry's
    // presence in a window as evidence of reachability.
    struct Evasive(MemoryDag);
    #[async_trait::async_trait]
    impl ParentProvider for Evasive {
        type Error = std::convert::Infallible;
        async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, Self::Error> {
            self.0.parents(commit).await
        }
        async fn ancestry_window(
            &self,
            start: &CommitId,
            max: usize,
            kind: WindowKind,
        ) -> Result<Vec<(CommitId, Vec<CommitId>)>, Self::Error> {
            let mut w = self.0.ancestry_window(start, max, kind).await?;
            w.retain(|(c, _)| c != &id(2));
            Ok(w)
        }
    }
    let dag = Evasive(linear(4).with_window(10));
    assert!(
        matches!(ancestors(&dag, &id(3), L).await, Err(DagError::UnknownCommit(c)) if c == id(2))
    );
    assert!(matches!(
        is_ancestor(&dag, &id(0), &id(3), L).await,
        Err(DagError::UnknownCommit(_))
    ));
}

#[tokio::test]
async fn a_first_parent_history_never_asks_for_more_than_it_still_wants() {
    // The window a bounded history requests is capped by the entries still wanted, so a
    // provider is never asked about commits beyond `max`; and damage a provider leaves out
    // of a window (contract: only the anchor's damage is reported) stays invisible to a
    // walk that never reaches it.
    struct Recording {
        inner: MemoryDag,
        damaged: CommitId,
        asked: Mutex<Vec<usize>>,
    }
    #[async_trait::async_trait]
    impl ParentProvider for Recording {
        type Error = Down;
        async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, Self::Error> {
            if commit == &self.damaged {
                return Err(Down);
            }
            Ok(self.inner.parents(commit).await.unwrap())
        }
        async fn ancestry_window(
            &self,
            start: &CommitId,
            max: usize,
            kind: WindowKind,
        ) -> Result<Vec<(CommitId, Vec<CommitId>)>, Self::Error> {
            if start == &self.damaged {
                return Err(Down);
            }
            self.asked.lock().unwrap().push(max);
            let mut w = self.inner.ancestry_window(start, max, kind).await.unwrap();
            w.retain(|(c, _)| c != &self.damaged);
            Ok(w)
        }
    }
    let dag = Recording {
        inner: linear(20).with_window(1_000),
        damaged: id(10),
        asked: Mutex::new(Vec::new()),
    };
    // Entries 19..=11 are fine; the damaged 10 is beyond a 9-entry history.
    assert_eq!(
        first_parent_history(&dag, &id(19), 9, L).await.unwrap(),
        ids(&[19, 18, 17, 16, 15, 14, 13, 12, 11])
    );
    assert!(
        dag.asked.lock().unwrap().iter().all(|&m| m <= 9),
        "{:?}",
        dag.asked.lock().unwrap()
    );
    // One more entry reaches the damage, at the same point as an unwindowed walk.
    assert!(matches!(
        first_parent_history(&dag, &id(19), 10, L).await,
        Err(DagError::Provider(Down))
    ));
    // An early hit before the damage succeeds; a full search fails on it.
    assert!(is_ancestor(&dag, &id(15), &id(19), L).await.unwrap());
    assert!(matches!(
        is_ancestor(&dag, &id(0), &id(19), L).await,
        Err(DagError::Provider(Down))
    ));
    assert!(matches!(
        ancestors(&dag, &id(19), L).await,
        Err(DagError::Provider(Down))
    ));
    // The remaining visit budget caps the window too.
    let dag = Recording {
        inner: linear(20).with_window(1_000),
        damaged: id(99),
        asked: Mutex::new(Vec::new()),
    };
    assert_eq!(
        ancestors(&dag, &id(19), limit(5))
            .await
            .unwrap_err()
            .to_string(),
        "traversal visit limit reached after 5 commits"
    );
    assert_eq!(*dag.asked.lock().unwrap(), vec![1, 4]);
}

#[test]
fn window_calls_follows_the_ramp_then_full_windows() {
    assert_eq!(window_calls(0, 256), 0);
    assert_eq!(window_calls(1, 256), 1);
    assert_eq!(window_calls(5, 256), 2);
    assert_eq!(window_calls(85, 256), 4);
    assert_eq!(window_calls(86, 256), 5);
    assert_eq!(
        window_calls(5_000, 256),
        4 + (5_000 - 85usize).div_ceil(256)
    );
    assert_eq!(window_calls(1_000, 1), 1_000);
    assert_eq!(window_calls(7, 2), 4);
}

#[tokio::test]
async fn generated_back_edges_are_cycles_under_every_window_and_limits_match() {
    // The injected-cycle and visit-limit answers are the same for every window size as for
    // the unwindowed provider, on the generated DAGs.
    for seed in 0..40 {
        let g = generate(seed);
        let n = g.parents.len();
        for &window in &WINDOWS[1..] {
            let wide = g.dag.clone().with_window(window);
            for h in 0..n {
                let head = id(h as u64);
                for k in [0usize, 1, 2, 3, n / 2, n] {
                    let want = ancestors(&g.dag, &head, limit(k))
                        .await
                        .map_err(|e| e.to_string());
                    let got = ancestors(&wide, &head, limit(k))
                        .await
                        .map_err(|e| e.to_string());
                    assert_eq!(want, got, "seed {seed} window {window} head {h} limit {k}");
                    let want = first_parent_history(&g.dag, &head, k, L).await.unwrap();
                    let got = first_parent_history(&wide, &head, k, L).await.unwrap();
                    assert_eq!(want, got, "seed {seed} window {window} head {h} max {k}");
                }
            }
        }
        let mut g = g;
        let mut rng = Rng::new(seed ^ 0xC1C1_E5ED);
        let candidates: Vec<(usize, usize)> = (0..n)
            .flat_map(|j| g.reach[j].iter().map(move |&i| (i, j)))
            .filter(|&(i, j)| i != j)
            .collect();
        let (i, j) = if candidates.is_empty() {
            let k = rng.below(n as u64) as usize;
            (k, k)
        } else {
            candidates[rng.below(candidates.len() as u64) as usize]
        };
        let pos = rng.below(g.parents[i].len() as u64 + 1) as usize;
        g.parents[i].insert(pos, j);
        g.dag
            .insert(id(i as u64), g.parents[i].iter().map(|&p| id(p as u64)));
        for &window in &WINDOWS[1..] {
            let wide = g.dag.clone().with_window(window);
            for h in 0..n {
                let head = id(h as u64);
                let want = ancestors(&g.dag, &head, L).await.map_err(|e| e.to_string());
                let got = ancestors(&wide, &head, L).await.map_err(|e| e.to_string());
                assert_eq!(
                    want, got,
                    "seed {seed} window {window} head {h} with back edge"
                );
                let want = first_parent_history(&g.dag, &head, usize::MAX, L)
                    .await
                    .map_err(|e| e.to_string());
                let got = first_parent_history(&wide, &head, usize::MAX, L)
                    .await
                    .map_err(|e| e.to_string());
                assert_eq!(
                    want, got,
                    "seed {seed} window {window} head {h} first-parent with back edge"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_provider_that_overfills_or_repeats_entries_does_not_change_answers() {
    // Contract violations the walk tolerates: more entries than asked for, and the anchor
    // listed twice. Answers must still come only from parent edges.
    struct Loud(MemoryDag);
    #[async_trait::async_trait]
    impl ParentProvider for Loud {
        type Error = std::convert::Infallible;
        async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, Self::Error> {
            self.0.parents(commit).await
        }
        async fn ancestry_window(
            &self,
            start: &CommitId,
            _max: usize,
            kind: WindowKind,
        ) -> Result<Vec<(CommitId, Vec<CommitId>)>, Self::Error> {
            let mut w = self.0.ancestry_window(start, usize::MAX, kind).await?;
            if let Some(first) = w.first().cloned() {
                w.push(first);
            }
            Ok(w)
        }
    }
    for seed in 0..40 {
        let g = generate(seed);
        let loud = Loud(g.dag.clone().with_window(1_000));
        let n = g.parents.len();
        for h in 0..n {
            let head = id(h as u64);
            assert_eq!(
                ancestors(&loud, &head, L).await.unwrap(),
                ancestors(&g.dag, &head, L).await.unwrap(),
                "seed {seed} head {h}"
            );
            assert_eq!(
                ancestors(&loud, &head, limit(2))
                    .await
                    .map_err(|e| e.to_string()),
                ancestors(&g.dag, &head, limit(2))
                    .await
                    .map_err(|e| e.to_string()),
                "seed {seed} head {h} limit 2"
            );
        }
        for t in 0..n {
            for s in 0..n {
                assert_eq!(
                    analyze(&loud, &id(t as u64), &id(s as u64), L)
                        .await
                        .unwrap(),
                    analyze(&g.dag, &id(t as u64), &id(s as u64), L)
                        .await
                        .unwrap(),
                    "seed {seed} analyze({t}, {s})"
                );
            }
        }
    }
}
