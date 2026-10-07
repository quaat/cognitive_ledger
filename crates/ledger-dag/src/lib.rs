//! Bounded history/reachability primitives over the ledger's commit DAG.
//!
//! The ledger stores commits as an immutable, content-addressed DAG: each commit names its
//! parents in commit order (position 0 is the first parent). This crate answers bounded
//! questions over that DAG through the [`ParentProvider`] boundary:
//!
//! - [`is_ancestor`]: is a commit reachable from another through any parent edges (used to
//!   validate that a requested historical branch point is reachable from a source head);
//! - [`first_parent_history`]: the first-parent chain from a head;
//! - [`ancestors`]: every commit reachable from a head, each once, in a deterministic order;
//! - [`merge_base`], [`ahead_behind`] and [`analyze`]: the Phase-5 merge inputs (ADR-0024) —
//!   the unique best common ancestor (or an explicit ambiguous / unrelated answer), how many
//!   commits each side has that the other lacks, and the ancestry relation that classifies a
//!   merge (equal / source contained / fast-forward / divergent, ADR-0023).
//!
//! Every traversal is iterative (no recursion, so 100k-deep linear history cannot overflow
//! the stack), keeps a visited set (each distinct commit is entered at most once per call, so
//! duplicate ancestry paths such as diamonds do not multiply work), is bounded by
//! [`TraversalLimits`] and fails closed on corruption: a missing parent is
//! [`DagError::UnknownCommit`] and a cycle presented by the provider is [`DagError::Cycle`].
//!
//! Retrieval is windowed (Plan 0012): a walk asks the provider for a bounded
//! [`ParentProvider::ancestry_window`] around the commit it needs and keeps the other
//! entries of the reply for later, so a provider backed by a database answers a deep linear
//! history in `ceil(n / window)` round trips instead of `n`. The window is a retrieval hint
//! only: visit limits count commits the walk enters, every answer is still derived from the
//! parents the provider returned for exactly that commit, and a provider that cannot batch
//! keeps the default window of one commit.
//!
//! Out of scope: three-way state merge (`ledger-merge`) and anything that reads RDF.
//!
//! The crate is infrastructure-free: it never talks to a database, HTTP or containers.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Instant;

use ledger_core::CommitId;

/// Which parent edges a retrieval window follows from its anchor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowKind {
    /// Every parent (the commits an ancestry walk will need).
    Ancestry,
    /// Position 0 only (the commits a first-parent history will need).
    FirstParent,
}

/// Supplies a commit's parents (in commit order; position 0 = first parent).
/// `Ok(None)` = the commit is unknown to this provider (e.g. not in the graph).
#[async_trait::async_trait]
pub trait ParentProvider: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, Self::Error>;

    /// A retrieval window: the parents of up to `max` (≥ 1) commits reachable from `start`
    /// through the edges `kind` names, `start` among them when it is known. Only commits
    /// the provider knows are returned (an unknown `start` yields an empty window), each
    /// at most once, with exactly the parents [`Self::parents`] would return for it. The
    /// walk treats the extra entries purely as prefetched answers; it never infers
    /// reachability from a commit's presence in a window. A provider reports an error only
    /// for `start` itself: a prefetched commit it cannot answer for (corrupt rows) is left
    /// out, so that a walk which never needs it is unaffected and one that does fails at
    /// the same point as an unwindowed walk, when it asks for it as an anchor.
    ///
    /// The default returns `start` alone, which makes every existing provider a window of
    /// one. Batching providers override it with a bounded, set-based fetch.
    async fn ancestry_window(
        &self,
        start: &CommitId,
        max: usize,
        kind: WindowKind,
    ) -> Result<Vec<(CommitId, Vec<CommitId>)>, Self::Error> {
        let _ = (max, kind);
        Ok(self
            .parents(start)
            .await?
            .map(|parents| vec![(start.clone(), parents)])
            .unwrap_or_default())
    }
}

/// Growth factor of the window a walk asks for: the first provider call asks for one
/// commit, the next for four, then sixteen, and so on, until the provider's own window
/// caps it. [`window_calls`] gives the resulting number of calls for a linear walk.
pub const WINDOW_RAMP: usize = 4;

/// The number of provider calls a walk makes to retrieve `commits` commits in a row from
/// a provider whose window is `window` (linear history; the ramp of [`WINDOW_RAMP`] then
/// full windows). Zero commits need no call.
pub fn window_calls(commits: usize, window: usize) -> usize {
    let window = window.max(1);
    let (mut done, mut ask, mut calls) = (0usize, 1usize, 0usize);
    while done < commits {
        done += ask.min(window);
        calls += 1;
        ask = ask.saturating_mul(WINDOW_RAMP);
    }
    calls
}

/// Bounds on a single traversal call.
#[derive(Clone, Copy, Debug)]
pub struct TraversalLimits {
    /// Maximum number of distinct commits a traversal may visit (enter; rows a provider
    /// prefetched into a window but the walk never entered do not count). Visiting one more
    /// is [`DagError::VisitLimit`].
    pub max_visited: usize,
    /// Wall-clock deadline, checked before and after every provider call and before every
    /// commit served from a prefetched window. A deadline that is already at or before
    /// "now" fails the traversal with [`DagError::Deadline`].
    pub deadline: Option<Instant>,
}

impl TraversalLimits {
    pub const DEFAULT: Self = Self {
        max_visited: 100_000,
        deadline: None,
    };
}

impl Default for TraversalLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Traversal failure. Every variant fails closed: no partial answer is returned.
#[derive(Debug, thiserror::Error)]
pub enum DagError<E> {
    /// A start commit or a referenced parent is unknown to the provider (a missing parent is
    /// storage corruption).
    #[error("unknown commit {0}")]
    UnknownCommit(CommitId),
    /// The provider presented a cycle through this commit (storage must be acyclic).
    #[error("commit graph cycle through {0}")]
    Cycle(CommitId),
    /// More than `max_visited` distinct commits would have been visited.
    #[error("traversal visit limit reached after {visited} commits")]
    VisitLimit { visited: usize },
    /// The traversal deadline passed.
    #[error("traversal deadline exceeded")]
    Deadline,
    /// The provider failed.
    #[error("parent provider error: {0}")]
    Provider(#[source] E),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Colour {
    /// On the current DFS path (entered, not yet finished).
    Grey,
    /// Fully explored: every commit reachable from it has been finished without a cycle.
    Black,
}

/// Shared traversal state: the visited set (with DFS colours), the limits, the provider and
/// the parents prefetched by retrieval windows but not entered yet.
struct Walk<'a, P: ParentProvider + ?Sized> {
    provider: &'a P,
    limits: TraversalLimits,
    kind: WindowKind,
    colour: HashMap<CommitId, Colour>,
    /// Parents returned by a window for commits the walk has not entered. An entry is
    /// removed when its commit is entered, and the map never holds more than
    /// `max_visited` entries, so memory stays bounded by the visit limit even when the
    /// provider's windows are wide.
    prefetched: HashMap<CommitId, Vec<CommitId>>,
    /// The most commits the next window may ask for. It starts at one and quadruples
    /// after every provider call ([`WINDOW_RAMP`]), so a search that ends after a few
    /// commits (a branch point near the head, a short history page) costs a few small
    /// windows, while a deep walk reaches the provider's full window after four calls.
    ramp: usize,
    /// When set, every fetched commit's parents are recorded ([`ancestry`]).
    record: Option<HashMap<CommitId, Vec<CommitId>>>,
}

enum DfsOutcome {
    /// The visitor asked to stop at this commit.
    Hit,
    /// Every reachable commit was visited; no cycle was found.
    Complete,
}

impl<'a, P: ParentProvider + ?Sized> Walk<'a, P> {
    fn new(provider: &'a P, limits: TraversalLimits, kind: WindowKind) -> Self {
        Self {
            provider,
            limits,
            kind,
            colour: HashMap::new(),
            prefetched: HashMap::new(),
            ramp: 1,
            record: None,
        }
    }

    fn check_deadline(&self) -> Result<(), DagError<P::Error>> {
        match self.limits.deadline {
            Some(deadline) if Instant::now() >= deadline => Err(DagError::Deadline),
            _ => Ok(()),
        }
    }

    /// Fetches the parents of a commit that has not been visited yet, charging it against
    /// the visit limit and checking the deadline around the provider call. A commit already
    /// prefetched by an earlier window is served from memory; otherwise the provider is
    /// asked for a window anchored at the commit, bounded by the remaining visit budget
    /// (and by `wanted`, the most commits the caller can still use), and the window's
    /// other entries are kept for later.
    async fn load(
        &mut self,
        id: &CommitId,
        wanted: usize,
    ) -> Result<Vec<CommitId>, DagError<P::Error>> {
        let visited = self.colour.len();
        if visited >= self.limits.max_visited {
            return Err(DagError::VisitLimit { visited });
        }
        self.check_deadline()?;
        let parents = match self.prefetched.remove(id) {
            Some(parents) => parents,
            None => {
                // `visited < max_visited`, so the budget is at least one.
                let max = (self.limits.max_visited - visited)
                    .min(wanted)
                    .min(self.ramp)
                    .max(1);
                self.ramp = self.ramp.saturating_mul(WINDOW_RAMP);
                let window = self
                    .provider
                    .ancestry_window(id, max, self.kind)
                    .await
                    .map_err(DagError::Provider)?;
                self.check_deadline()?;
                let mut found = None;
                for (commit, parents) in window {
                    if &commit == id {
                        // The anchor's own answer; a duplicate entry for it is ignored.
                        found.get_or_insert(parents);
                    } else if !self.colour.contains_key(&commit)
                        && self.prefetched.len() < self.limits.max_visited
                    {
                        self.prefetched.entry(commit).or_insert(parents);
                    }
                }
                found.ok_or_else(|| DagError::UnknownCommit(id.clone()))?
            }
        };
        if let Some(record) = &mut self.record {
            record.insert(id.clone(), parents.clone());
        }
        Ok(parents)
    }

    /// Iterative DFS with white/grey/black colouring from `start`, following parents in
    /// position order. `on_enter` is called once per distinct commit, in DFS preorder, after
    /// the commit's parents were successfully fetched; returning `true` stops the search.
    ///
    /// Reaching a grey commit (one on the current path) is a cycle. If the search completes,
    /// every reachable commit was finished, so no cycle is reachable from `start`.
    async fn dfs(
        &mut self,
        start: &CommitId,
        mut on_enter: impl FnMut(&CommitId) -> bool + Send,
    ) -> Result<DfsOutcome, DagError<P::Error>> {
        struct Frame {
            id: CommitId,
            parents: Vec<CommitId>,
            next: usize,
        }
        let parents = self.load(start, usize::MAX).await?;
        self.colour.insert(start.clone(), Colour::Grey);
        if on_enter(start) {
            return Ok(DfsOutcome::Hit);
        }
        let mut stack = vec![Frame {
            id: start.clone(),
            parents,
            next: 0,
        }];
        while let Some(top) = stack.last_mut() {
            if top.next < top.parents.len() {
                let parent = top.parents[top.next].clone();
                top.next += 1;
                match self.colour.get(&parent) {
                    Some(Colour::Grey) => return Err(DagError::Cycle(parent)),
                    Some(Colour::Black) => {}
                    None => {
                        let parents = self.load(&parent, usize::MAX).await?;
                        self.colour.insert(parent.clone(), Colour::Grey);
                        if on_enter(&parent) {
                            return Ok(DfsOutcome::Hit);
                        }
                        stack.push(Frame {
                            id: parent,
                            parents,
                            next: 0,
                        });
                    }
                }
            } else {
                let done = stack.pop().expect("stack top exists");
                self.colour.insert(done.id, Colour::Black);
            }
        }
        Ok(DfsOutcome::Complete)
    }
}

/// Whether `ancestor` is `descendant` itself or reachable from it through ANY parent edges.
///
/// Searches from `descendant` and exits early on a hit (a commit is only reported as a hit
/// after the provider confirmed it is known). Without a hit the whole reachable history is
/// searched, so a reachable cycle is reported as [`DagError::Cycle`] and a missing parent as
/// [`DagError::UnknownCommit`]. An unknown `descendant` is `UnknownCommit(descendant)`; an
/// `ancestor` unknown to the provider is simply not reachable (`Ok(false)`).
pub async fn is_ancestor<P: ParentProvider + ?Sized>(
    provider: &P,
    ancestor: &CommitId,
    descendant: &CommitId,
    limits: TraversalLimits,
) -> Result<bool, DagError<P::Error>> {
    let mut walk = Walk::new(provider, limits, WindowKind::Ancestry);
    let outcome = walk.dfs(descendant, |id| id == ancestor).await?;
    Ok(matches!(outcome, DfsOutcome::Hit))
}

/// The first-parent chain starting at `head` (head first), at most `max` entries; stops at a
/// root (a commit without parents).
///
/// Every returned commit was confirmed known by the provider; a missing first parent is
/// [`DagError::UnknownCommit`]. A commit repeated along the chain is [`DagError::Cycle`].
/// Only first-parent edges are followed, so cycles through other parents are not examined.
/// `max == 0` returns an empty history without calling the provider. The visit limit applies
/// to the number of chain entries fetched.
pub async fn first_parent_history<P: ParentProvider + ?Sized>(
    provider: &P,
    head: &CommitId,
    max: usize,
    limits: TraversalLimits,
) -> Result<Vec<CommitId>, DagError<P::Error>> {
    let mut history = Vec::new();
    if max == 0 {
        return Ok(history);
    }
    let mut walk = Walk::new(provider, limits, WindowKind::FirstParent);
    let mut current = head.clone();
    loop {
        // A window never larger than the entries still wanted: a damaged or missing commit
        // beyond `max` is never read, exactly as before.
        let parents = walk.load(&current, max - history.len()).await?;
        walk.colour.insert(current.clone(), Colour::Grey);
        history.push(current);
        if history.len() >= max {
            return Ok(history);
        }
        let Some(first) = parents.into_iter().next() else {
            return Ok(history);
        };
        if walk.colour.contains_key(&first) {
            return Err(DagError::Cycle(first));
        }
        current = first;
    }
}

/// Every commit reachable from `head` (including head), each exactly once.
///
/// Order (deterministic for a given provider): depth-first preorder by discovery, starting
/// at `head` and following each commit's parents in position order (first parent first); a
/// commit already discovered through an earlier path is not repeated. For a linear history
/// this is head-to-root order.
///
/// The whole reachable history is examined: a reachable cycle is [`DagError::Cycle`] and a
/// missing parent is [`DagError::UnknownCommit`].
pub async fn ancestors<P: ParentProvider + ?Sized>(
    provider: &P,
    head: &CommitId,
    limits: TraversalLimits,
) -> Result<Vec<CommitId>, DagError<P::Error>> {
    let mut walk = Walk::new(provider, limits, WindowKind::Ancestry);
    let mut order = Vec::new();
    walk.dfs(head, |id| {
        order.push(id.clone());
        false
    })
    .await?;
    Ok(order)
}

/// Every commit reachable from a head (including the head) with its parents, as fetched
/// during one bounded traversal. Fails closed like [`ancestors`].
#[derive(Clone, Debug, Default)]
pub struct Ancestry {
    parents: HashMap<CommitId, Vec<CommitId>>,
}

impl Ancestry {
    pub fn contains(&self, id: &CommitId) -> bool {
        self.parents.contains_key(id)
    }
    pub fn len(&self) -> usize {
        self.parents.len()
    }
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }
    /// The commits of this ancestry that `other` does not contain, ascending by id.
    pub fn difference(&self, other: &Ancestry) -> Vec<CommitId> {
        let set: BTreeSet<&CommitId> = self.parents.keys().filter(|c| !other.contains(c)).collect();
        set.into_iter().cloned().collect()
    }
}

/// The ancestry of `head`: every reachable commit with its parents (one bounded traversal).
pub async fn ancestry<P: ParentProvider + ?Sized>(
    provider: &P,
    head: &CommitId,
    limits: TraversalLimits,
) -> Result<Ancestry, DagError<P::Error>> {
    let mut walk = Walk::new(provider, limits, WindowKind::Ancestry);
    walk.record = Some(HashMap::new());
    walk.dfs(head, |_| false).await?;
    Ok(Ancestry {
        parents: walk.record.take().unwrap_or_default(),
    })
}

/// The merge base of two commits (ADR-0024).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeBase {
    /// Exactly one best common ancestor.
    Unique(CommitId),
    /// Several best common ancestors (criss-cross history), in ascending id order. v1 refuses
    /// to choose one: no iteration-, time- or id-based pick.
    Ambiguous(Vec<CommitId>),
    /// No common ancestor (unrelated roots).
    Unrelated,
}

/// The best common ancestors of two ancestries: the common ancestors that are not a proper
/// ancestor of another common ancestor (the maximal elements of `A(a) ∩ A(b)` under the
/// ancestor order), ascending by id.
pub fn best_common_ancestors(a: &Ancestry, b: &Ancestry) -> Vec<CommitId> {
    let common: Vec<&CommitId> = a.parents.keys().filter(|c| b.contains(c)).collect();
    // Every proper ancestor of a common ancestor is itself common (it is reachable from both
    // sides), so marking what the common set reaches through parents finds the dominated
    // ones without leaving `a`'s recorded edges.
    let mut dominated: HashSet<&CommitId> = HashSet::new();
    let mut stack: Vec<&CommitId> = common
        .iter()
        .flat_map(|c| a.parents.get(*c).into_iter().flatten())
        .collect();
    while let Some(c) = stack.pop() {
        if dominated.insert(c) {
            stack.extend(a.parents.get(c).into_iter().flatten());
        }
    }
    let best: BTreeSet<CommitId> = common
        .into_iter()
        .filter(|c| !dominated.contains(c))
        .cloned()
        .collect();
    best.into_iter().collect()
}

fn classify_base(best: Vec<CommitId>) -> MergeBase {
    match best.len() {
        0 => MergeBase::Unrelated,
        1 => MergeBase::Unique(best.into_iter().next().expect("one element")),
        _ => MergeBase::Ambiguous(best),
    }
}

/// The merge base of `a` and `b` (two bounded traversals; each is charged against `limits`
/// separately).
pub async fn merge_base<P: ParentProvider + ?Sized>(
    provider: &P,
    a: &CommitId,
    b: &CommitId,
    limits: TraversalLimits,
) -> Result<MergeBase, DagError<P::Error>> {
    let left = ancestry(provider, a, limits).await?;
    let right = ancestry(provider, b, limits).await?;
    Ok(classify_base(best_common_ancestors(&left, &right)))
}

/// `(ahead, behind)` of `source` relative to `target`: commits reachable from `source` but
/// not from `target`, and the reverse.
pub async fn ahead_behind<P: ParentProvider + ?Sized>(
    provider: &P,
    target: &CommitId,
    source: &CommitId,
    limits: TraversalLimits,
) -> Result<(usize, usize), DagError<P::Error>> {
    let t = ancestry(provider, target, limits).await?;
    let s = ancestry(provider, source, limits).await?;
    Ok(counts(&t, &s))
}

fn counts(t: &Ancestry, s: &Ancestry) -> (usize, usize) {
    let ahead = s.parents.keys().filter(|c| !t.contains(c)).count();
    let behind = t.parents.keys().filter(|c| !s.contains(c)).count();
    (ahead, behind)
}

/// How a source head relates to a target head (ADR-0023 classification; tests in order).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Relation {
    /// `target == source`.
    Equal,
    /// The source head is an ancestor of the target head: nothing to integrate.
    SourceContained,
    /// The target head is an ancestor of the source head (fast-forward class; base = target).
    FastForward,
    /// Neither contains the other; carries the merge base.
    Divergent(MergeBase),
}

/// The full merge analysis of `source` into `target` from one traversal per side.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Analysis {
    pub relation: Relation,
    /// Commits the source has that the target lacks.
    pub ahead: usize,
    /// Commits the target has that the source lacks.
    pub behind: usize,
}

pub async fn analyze<P: ParentProvider + ?Sized>(
    provider: &P,
    target: &CommitId,
    source: &CommitId,
    limits: TraversalLimits,
) -> Result<Analysis, DagError<P::Error>> {
    Ok(analyze_with_ancestries(provider, target, source, limits)
        .await?
        .0)
}

/// [`analyze`], also returning the target's and the source's ancestries (for callers that
/// need the commits one side has and the other lacks, without walking again).
pub async fn analyze_with_ancestries<P: ParentProvider + ?Sized>(
    provider: &P,
    target: &CommitId,
    source: &CommitId,
    limits: TraversalLimits,
) -> Result<(Analysis, Ancestry, Ancestry), DagError<P::Error>> {
    let t = ancestry(provider, target, limits).await?;
    if target == source {
        let analysis = Analysis {
            relation: Relation::Equal,
            ahead: 0,
            behind: 0,
        };
        return Ok((analysis, t.clone(), t));
    }
    let s = ancestry(provider, source, limits).await?;
    let (ahead, behind) = counts(&t, &s);
    let relation = if t.contains(source) {
        Relation::SourceContained
    } else if s.contains(target) {
        Relation::FastForward
    } else {
        Relation::Divergent(classify_base(best_common_ancestors(&t, &s)))
    };
    let analysis = Analysis {
        relation,
        ahead,
        behind,
    };
    Ok((analysis, t, s))
}

/// An in-memory, `HashMap`-backed [`ParentProvider`] for tests (including other crates'
/// unit tests). It accepts any graph, including malformed ones (missing parents, cycles).
/// Its retrieval window is one commit unless [`MemoryDag::with_window`] widens it.
#[derive(Clone, Debug)]
pub struct MemoryDag {
    parents: HashMap<CommitId, Vec<CommitId>>,
    window: usize,
}

impl Default for MemoryDag {
    fn default() -> Self {
        Self {
            parents: HashMap::new(),
            window: 1,
        }
    }
}

impl MemoryDag {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer [`ParentProvider::ancestry_window`] with up to `window` (≥ 1) commits found by
    /// a breadth-first walk from the anchor (tests of windowed traversal).
    #[must_use]
    pub fn with_window(mut self, window: usize) -> Self {
        self.window = window.max(1);
        self
    }

    /// The retrieval window this provider answers with.
    pub fn window(&self) -> usize {
        self.window
    }

    /// Builder form of [`MemoryDag::insert`].
    #[must_use]
    pub fn commit(mut self, id: CommitId, parents: impl IntoIterator<Item = CommitId>) -> Self {
        self.insert(id, parents);
        self
    }

    /// Records (or replaces) a commit's parents, in commit order.
    pub fn insert(&mut self, id: CommitId, parents: impl IntoIterator<Item = CommitId>) {
        self.parents.insert(id, parents.into_iter().collect());
    }

    /// The recorded parents of a commit, if known.
    pub fn get(&self, id: &CommitId) -> Option<&[CommitId]> {
        self.parents.get(id).map(Vec::as_slice)
    }

    pub fn contains(&self, id: &CommitId) -> bool {
        self.parents.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }
}

#[async_trait::async_trait]
impl ParentProvider for MemoryDag {
    type Error = std::convert::Infallible;
    async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, Self::Error> {
        Ok(self.parents.get(commit).cloned())
    }

    /// Breadth-first from `start` over the edges `kind` names, at most `min(window, max)`
    /// known commits; unknown parents are skipped (a later anchored window reports them).
    async fn ancestry_window(
        &self,
        start: &CommitId,
        max: usize,
        kind: WindowKind,
    ) -> Result<Vec<(CommitId, Vec<CommitId>)>, Self::Error> {
        let max = self.window.min(max).max(1);
        let mut out: Vec<(CommitId, Vec<CommitId>)> = Vec::new();
        let mut queued: HashSet<CommitId> = HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        if self.parents.contains_key(start) {
            queued.insert(start.clone());
            queue.push_back(start.clone());
        }
        while let Some(id) = queue.pop_front() {
            if out.len() >= max {
                break;
            }
            let parents = self.parents[&id].clone();
            let follow: &[CommitId] = match kind {
                WindowKind::Ancestry => &parents,
                WindowKind::FirstParent => &parents[..parents.len().min(1)],
            };
            for parent in follow {
                if self.parents.contains_key(parent) && queued.insert(parent.clone()) {
                    queue.push_back(parent.clone());
                }
            }
            out.push((id, parents));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
