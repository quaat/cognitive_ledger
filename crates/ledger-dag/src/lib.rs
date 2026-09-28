//! Bounded history/reachability primitives over the ledger's commit DAG.
//!
//! The ledger stores commits as an immutable, content-addressed DAG: each commit names its
//! parents in commit order (position 0 is the first parent). This crate answers bounded
//! questions over that DAG through the [`ParentProvider`] boundary:
//!
//! - [`is_ancestor`]: is a commit reachable from another through any parent edges (used to
//!   validate that a requested historical branch point is reachable from a source head);
//! - [`first_parent_history`]: the first-parent chain from a head;
//! - [`ancestors`]: every commit reachable from a head, each once, in a deterministic order.
//!
//! Every traversal is iterative (no recursion, so 100k-deep linear history cannot overflow
//! the stack), keeps a visited set (each distinct commit is fetched from the provider at most
//! once per call, so duplicate ancestry paths such as diamonds do not multiply work), is
//! bounded by [`TraversalLimits`] and fails closed on corruption: a missing parent is
//! [`DagError::UnknownCommit`] and a cycle presented by the provider is [`DagError::Cycle`].
//!
//! Out of scope: merge-base computation and three-way merge.
//!
//! The crate is infrastructure-free: it never talks to a database, HTTP or containers.

use std::collections::HashMap;
use std::time::Instant;

use ledger_core::CommitId;

/// Supplies a commit's parents (in commit order; position 0 = first parent).
/// `Ok(None)` = the commit is unknown to this provider (e.g. not in the graph).
#[async_trait::async_trait]
pub trait ParentProvider: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, Self::Error>;
}

/// Bounds on a single traversal call.
#[derive(Clone, Copy, Debug)]
pub struct TraversalLimits {
    /// Maximum number of distinct commits a traversal may visit (fetch from the provider).
    /// Visiting one more is [`DagError::VisitLimit`].
    pub max_visited: usize,
    /// Wall-clock deadline, checked before and after every provider call. A deadline that is
    /// already at or before "now" fails the traversal with [`DagError::Deadline`].
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

/// Shared traversal state: the visited set (with DFS colours), the limits and the provider.
struct Walk<'a, P: ParentProvider + ?Sized> {
    provider: &'a P,
    limits: TraversalLimits,
    colour: HashMap<CommitId, Colour>,
}

enum DfsOutcome {
    /// The visitor asked to stop at this commit.
    Hit,
    /// Every reachable commit was visited; no cycle was found.
    Complete,
}

impl<'a, P: ParentProvider + ?Sized> Walk<'a, P> {
    fn new(provider: &'a P, limits: TraversalLimits) -> Self {
        Self {
            provider,
            limits,
            colour: HashMap::new(),
        }
    }

    fn check_deadline(&self) -> Result<(), DagError<P::Error>> {
        match self.limits.deadline {
            Some(deadline) if Instant::now() >= deadline => Err(DagError::Deadline),
            _ => Ok(()),
        }
    }

    /// Fetches the parents of a commit that has not been visited yet, charging it against
    /// the visit limit and checking the deadline around the provider call.
    async fn load(&mut self, id: &CommitId) -> Result<Vec<CommitId>, DagError<P::Error>> {
        let visited = self.colour.len();
        if visited >= self.limits.max_visited {
            return Err(DagError::VisitLimit { visited });
        }
        self.check_deadline()?;
        let parents = self
            .provider
            .parents(id)
            .await
            .map_err(DagError::Provider)?;
        self.check_deadline()?;
        parents.ok_or_else(|| DagError::UnknownCommit(id.clone()))
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
        let parents = self.load(start).await?;
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
                        let parents = self.load(&parent).await?;
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
    let mut walk = Walk::new(provider, limits);
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
    let mut walk = Walk::new(provider, limits);
    let mut current = head.clone();
    loop {
        let parents = walk.load(&current).await?;
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
    let mut walk = Walk::new(provider, limits);
    let mut order = Vec::new();
    walk.dfs(head, |id| {
        order.push(id.clone());
        false
    })
    .await?;
    Ok(order)
}

/// An in-memory, `HashMap`-backed [`ParentProvider`] for tests (including other crates'
/// unit tests). It accepts any graph, including malformed ones (missing parents, cycles).
#[derive(Clone, Debug, Default)]
pub struct MemoryDag {
    parents: HashMap<CommitId, Vec<CommitId>>,
}

impl MemoryDag {
    pub fn new() -> Self {
        Self::default()
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
}

#[cfg(test)]
mod tests;
