//! Administrative filesystem → PostgreSQL immutable-content migration (ADR-0012).
//!
//! Properties: content-id and byte preserving (no envelope is regenerated, no id is
//! reassigned), idempotent and resumable (every publication is a truthful idempotent
//! write, so a re-run after interruption simply completes), non-destructive (the source
//! is opened read-only and never written), verification-first (source digests, graph
//! membership, destination bytes, the derived commit index and the reconstructed HEAD
//! state are all checked before any ref moves), and explicit about the v1 binding (the
//! destination store's `V1Binding`, which must name the target graph). A failure can
//! leave orphaned immutable content in PostgreSQL — never a ref pointing at a partially
//! imported or foreign-graph history.
//!
//! HEAD resolution covers both deployed topologies: a filesystem-only ledger keeps HEAD
//! in `refs/main`; the `FileStore` + `PgRefStore` topology keeps HEAD in the shared
//! `refs` table and never writes `refs/main`. Whichever exists is the HEAD; if both
//! exist they must agree; and in every case the HEAD must be among the imported commits,
//! so a wrong or partial `--source` cannot report success.

use crate::{FileStore, Ledger, PgRefStore, PostgresImmutableStore, V1Binding};
use ledger_core::{AnyCommit, CommitId, ContentId, GraphId, ImmutableStore, LedgerError, RefStore};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationOutcome {
    /// Content verified and the destination ref was installed from absent.
    RefInstalled,
    /// Content verified and the destination ref already equalled the HEAD (either a
    /// completed earlier run, or the pre-existing shared ref of the
    /// `FileStore` + `PgRefStore` topology whose content is now verified in PostgreSQL).
    AlreadyMigrated,
    /// Content verified; neither side has a HEAD, so no ref was touched.
    ContentOnlyNoHead,
}

#[derive(Clone, Debug, Serialize)]
pub struct MigrationReport {
    pub source_objects: usize,
    pub content_objects: usize,
    pub commit_objects: usize,
    pub commits_published_in_order: Vec<String>,
    /// Index rows re-derived and verified for exactly the migrated commits.
    pub commit_index_rows_verified: usize,
    pub source_head: Option<String>,
    pub destination_head_before: Option<String>,
    pub destination_head_after: Option<String>,
    pub graph_id: String,
    pub branch: String,
    /// Number of quads in the reconstructed HEAD state (identical on both backends).
    pub head_state_quads: Option<usize>,
    /// SHA-256 over the sorted canonical N-Quads of the HEAD state, identical on both
    /// backends by construction of the check.
    pub head_state_digest: Option<String>,
    pub outcome: MigrationOutcome,
}

pub struct FsToPgMigration {
    source: Arc<FileStore>,
    destination: Arc<PostgresImmutableStore>,
    destination_refs: Arc<PgRefStore>,
    graph_id: GraphId,
}

impl FsToPgMigration {
    /// The target graph is the one `destination_refs` coordinates; the destination
    /// store's v1 binding must name that same graph (or reject v1 altogether), so there
    /// is exactly one graph identity in play and the report cannot name another.
    pub fn new(
        source: FileStore,
        destination: PostgresImmutableStore,
        destination_refs: PgRefStore,
    ) -> Result<Self, LedgerError> {
        let graph_id = GraphId::new(destination_refs.graph_id())?;
        match destination.v1_binding() {
            V1Binding::Reject => {}
            V1Binding::BindTo(bound) if *bound == graph_id => {}
            V1Binding::BindTo(bound) => {
                return Err(LedgerError::InvalidCommit(format!(
                    "destination v1 binding {bound} does not match the target ref graph {graph_id}"
                )));
            }
        }
        Ok(Self {
            source: Arc::new(source),
            destination: Arc::new(destination),
            destination_refs: Arc::new(destination_refs),
            graph_id,
        })
    }

    pub async fn run(&self) -> Result<MigrationReport, LedgerError> {
        // 1–4. Enumerate, verify every source object against its id, classify. A
        // commit-family object that does not decode is an error, never "content".
        let ids = self.source.list_objects()?;
        let mut content: BTreeMap<ContentId, Vec<u8>> = BTreeMap::new();
        let mut commits: BTreeMap<CommitId, (AnyCommit, Vec<u8>)> = BTreeMap::new();
        for id in &ids {
            let bytes = self
                .source
                .get_content(id)
                .await?
                .ok_or_else(|| LedgerError::NotFound(id.clone()))?;
            match crate::decode_commit_object(id, &bytes)? {
                Some(commit) => {
                    commits.insert(CommitId(id.clone()), (commit, bytes));
                }
                None => {
                    content.insert(id.clone(), bytes);
                }
            }
        }

        // Graph membership before anything is written: every v2 commit must belong to the
        // target graph, and v1 commits are only importable under a matching binding.
        for (id, (commit, _)) in &commits {
            match commit.graph_id() {
                Some(graph) if *graph != self.graph_id => {
                    return Err(LedgerError::GraphBindingConflict {
                        commit: id.clone(),
                        indexed: graph.to_string(),
                        requested: self.graph_id.to_string(),
                    });
                }
                None if *self.destination.v1_binding() == V1Binding::Reject => {
                    return Err(LedgerError::InvalidCommit(format!(
                        "source contains v1 commit {id} but the destination rejects v1 (no binding)"
                    )));
                }
                _ => {}
            }
        }

        // 6–7 (ordering first): missing parents and cycles abort before any publication.
        let order = topological_order(&commits)?;

        // Resolve HEAD from whichever side holds it, and require agreement.
        let source_head = self.source.head().await?;
        let destination_head_before = self.destination_refs.head().await?;
        let head = match (&source_head, &destination_head_before) {
            (Some(s), Some(d)) if s != d => {
                return Err(LedgerError::HeadChanged {
                    expected: Some(s.clone()),
                    actual: Some(d.clone()),
                });
            }
            (Some(s), _) => Some(s.clone()),
            (None, Some(d)) => Some(d.clone()),
            (None, None) => None,
        };
        if let Some(head) = &head
            && !commits.contains_key(head)
        {
            // The wrong or a partial source directory: never "succeed" on it.
            return Err(LedgerError::MissingTarget(head.clone()));
        }

        // 5. Non-commit content first (patches), so every commit's patch exists.
        for (id, bytes) in &content {
            self.destination.put_content(id, bytes).await?;
        }
        // 7. Topological import: every parent is indexed before its children.
        let mut published = Vec::with_capacity(order.len());
        for id in &order {
            let (commit, _) = &commits[id];
            let stored = self.destination.put_commit(commit).await?;
            if stored != *id {
                return Err(LedgerError::CorruptObject {
                    id: id.0.clone(),
                    reason: "destination assigned a different commit id".into(),
                });
            }
            published.push(id.to_string());
        }

        // 8. Verify destination bytes for every object, then the derived index rows of
        // exactly the migrated commits (including that each is indexed under the target
        // graph).
        for (id, bytes) in content
            .iter()
            .chain(commits.iter().map(|(k, (_, b))| (&k.0, b)))
        {
            let stored = self
                .destination
                .get_content(id)
                .await?
                .ok_or_else(|| LedgerError::NotFound(id.clone()))?;
            if stored != *bytes {
                return Err(LedgerError::ObjectCollision(id.clone()));
            }
        }
        let verified = self.destination.verify_commits(&order).await?;
        for id in &order {
            let row = sqlx::query("SELECT graph_id FROM commit_index WHERE id = $1")
                .bind(id.to_string())
                .fetch_one(self.destination.pool())
                .await
                .map_err(crate::storage)?;
            let indexed: String = row.try_get("graph_id").map_err(crate::storage)?;
            if indexed != self.graph_id.as_str() {
                return Err(LedgerError::GraphBindingConflict {
                    commit: id.clone(),
                    indexed,
                    requested: self.graph_id.to_string(),
                });
            }
        }

        // 9–10. Reconstruct HEAD through both backends and compare.
        let mut head_state_quads = None;
        let mut head_state_digest = None;
        if let Some(head) = &head {
            let source_ledger = Ledger::with_stores(
                self.source.clone() as Arc<dyn ImmutableStore>,
                self.source.clone() as Arc<dyn RefStore>,
            );
            let destination_ledger = Ledger::with_stores(
                self.destination.clone() as Arc<dyn ImmutableStore>,
                self.destination_refs.clone() as Arc<dyn RefStore>,
            );
            let source_state = source_ledger.state_at(head).await?;
            let destination_state = destination_ledger.state_at(head).await?;
            if source_state != destination_state {
                return Err(LedgerError::CorruptObject {
                    id: head.0.clone(),
                    reason: "reconstructed HEAD state differs between source and destination"
                        .into(),
                });
            }
            head_state_quads = Some(source_state.len());
            head_state_digest = Some(state_digest(&source_state));
        }

        // 11. Only now touch the ref, and never overwrite a differing head. A concurrent
        // identical migration that installed the same HEAD first is idempotent success.
        let outcome = match (&head, &destination_head_before) {
            (None, _) => MigrationOutcome::ContentOnlyNoHead,
            (Some(_), Some(_)) => MigrationOutcome::AlreadyMigrated,
            (Some(head), None) => match self.destination_refs.compare_and_set(None, head).await {
                Ok(()) => MigrationOutcome::RefInstalled,
                Err(LedgerError::HeadChanged {
                    actual: Some(actual),
                    ..
                }) if actual == *head => MigrationOutcome::AlreadyMigrated,
                Err(e) => return Err(e),
            },
        };
        let destination_head_after = self.destination_refs.head().await?;

        Ok(MigrationReport {
            source_objects: ids.len(),
            content_objects: content.len(),
            commit_objects: commits.len(),
            commits_published_in_order: published,
            commit_index_rows_verified: verified,
            source_head: source_head.map(|h| h.to_string()),
            destination_head_before: destination_head_before.map(|h| h.to_string()),
            destination_head_after: destination_head_after.map(|h| h.to_string()),
            graph_id: self.graph_id.to_string(),
            branch: self.destination_refs.branch().to_owned(),
            head_state_quads,
            head_state_digest,
            outcome,
        })
    }
}

/// Parents before children; `MissingParent` for an unknown parent, `CorruptObject`
/// for a cycle (impossible for honest content-addressed history, checked anyway).
fn topological_order(
    commits: &BTreeMap<CommitId, (AnyCommit, Vec<u8>)>,
) -> Result<Vec<CommitId>, LedgerError> {
    let mut order = Vec::with_capacity(commits.len());
    let mut done: HashSet<CommitId> = HashSet::new();
    for root in commits.keys() {
        // Iterative DFS with an explicit "visiting" set for cycle detection.
        let mut visiting: HashSet<CommitId> = HashSet::new();
        let mut stack: Vec<(CommitId, bool)> = vec![(root.clone(), false)];
        while let Some((id, expanded)) = stack.pop() {
            if done.contains(&id) {
                continue;
            }
            if expanded {
                visiting.remove(&id);
                done.insert(id.clone());
                order.push(id);
                continue;
            }
            if !visiting.insert(id.clone()) {
                return Err(LedgerError::CorruptObject {
                    id: id.0,
                    reason: "commit ancestry cycle".into(),
                });
            }
            let (commit, _) = commits
                .get(&id)
                .ok_or_else(|| LedgerError::MissingParent(id.clone()))?;
            stack.push((id.clone(), true));
            for parent in commit.parents() {
                if !done.contains(parent) {
                    if !commits.contains_key(parent) {
                        return Err(LedgerError::MissingParent(parent.clone()));
                    }
                    if visiting.contains(parent) {
                        return Err(LedgerError::CorruptObject {
                            id: parent.0.clone(),
                            reason: "commit ancestry cycle".into(),
                        });
                    }
                    stack.push((parent.clone(), false));
                }
            }
        }
    }
    Ok(order)
}

fn state_digest(state: &BTreeSet<ledger_rdf::Quad>) -> String {
    let mut hasher = Sha256::new();
    for quad in state {
        hasher.update(quad.to_string().as_bytes());
        hasher.update(b"\n");
    }
    format!("sha256:{}", hex::encode(hasher.finalize()))
}
