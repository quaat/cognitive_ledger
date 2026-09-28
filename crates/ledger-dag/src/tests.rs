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
