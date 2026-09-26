//! PostgreSQL-backed shared immutable store (ADR-0012): content-addressed
//! `immutable_objects` plus a derived, verified `commit_index`/`commit_parents` written
//! in the same transaction as the commit bytes. Every replica sharing the database sees
//! the same content, which is what makes multi-replica deployment correct.
//!
//! Publication is *truthful*: after every `INSERT … ON CONFLICT DO NOTHING` the
//! authoritative row is read back and compared with what the caller derived. A conflict
//! therefore means "identical, already published" or an explicit error — never a silent
//! success over foreign bytes or a foreign graph binding.

use crate::{decode_commit_object, reject_commit_bytes_as_content, storage};
use ledger_core::{AnyCommit, CommitId, ContentId, GraphId, ImmutableStore, LedgerError};
use sqlx::{PgConnection, PgPool, Row, postgres::PgPoolOptions};
use std::str::FromStr;

/// How v1 envelopes (which carry no `graph_id`) are bound to a graph on write
/// (ADR-0010, "Production v1 graph-binding policy").
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum V1Binding {
    /// Production default: v1 commits are not accepted by this store.
    Reject,
    /// Bootstrap / import: index v1 commits under this graph.
    BindTo(GraphId),
}

#[derive(Clone, Debug)]
pub struct PostgresImmutableStore {
    pool: PgPool,
    v1_binding: V1Binding,
}

/// The shared publication primitive. Verifies `bytes` hash to `id`, inserts when absent,
/// then reads the authoritative row back: identical bytes are idempotent success, any
/// other stored bytes are `ObjectCollision`. Immutable bytes are never overwritten. Works
/// inside or outside a transaction (`&mut *tx` or a pool connection).
pub(crate) async fn publish_object(
    conn: &mut PgConnection,
    id: &ContentId,
    bytes: &[u8],
) -> Result<(), LedgerError> {
    if &ContentId::for_bytes(bytes) != id {
        return Err(LedgerError::ObjectCollision(id.clone()));
    }
    let id_s = id.to_string();
    sqlx::query(
        "INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
    )
    .bind(&id_s)
    .bind(bytes)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    let row = sqlx::query("SELECT bytes FROM immutable_objects WHERE id = $1")
        .bind(&id_s)
        .fetch_one(&mut *conn)
        .await
        .map_err(storage)?;
    let stored: Vec<u8> = row.try_get("bytes").map_err(storage)?;
    if stored != bytes {
        return Err(LedgerError::ObjectCollision(id.clone()));
    }
    Ok(())
}

fn corrupt(id: &ContentId, reason: &str) -> LedgerError {
    LedgerError::CorruptObject {
        id: id.clone(),
        reason: reason.to_owned(),
    }
}

impl PostgresImmutableStore {
    /// Connect a fresh pool, run migrations, and apply `v1_binding` on writes.
    pub async fn connect(database_url: &str, v1_binding: V1Binding) -> Result<Self, LedgerError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(storage)?;
        Self::from_pool(pool, v1_binding).await
    }

    /// Compose over an existing pool. Runs migrations.
    pub async fn from_pool(pool: PgPool, v1_binding: V1Binding) -> Result<Self, LedgerError> {
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .map_err(storage)?;
        Ok(Self { pool, v1_binding })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn v1_binding(&self) -> &V1Binding {
        &self.v1_binding
    }

    /// The effective graph a commit is published under (ADR-0010 policy).
    pub fn graph_for(&self, commit: &AnyCommit) -> Result<GraphId, LedgerError> {
        match (commit.graph_id(), &self.v1_binding) {
            (Some(graph), _) => Ok(graph.clone()),
            (None, V1Binding::BindTo(graph)) => Ok(graph.clone()),
            (None, V1Binding::Reject) => Err(LedgerError::InvalidCommit(
                "v1 commits carry no graph_id and this store rejects them (V1Binding::Reject)"
                    .into(),
            )),
        }
    }

    /// Re-derive every `commit_index`/`commit_parents` row in the database from the
    /// stored bytes and compare. Returns the number of indexed commits verified. This is
    /// the ADR-0012 gate check, not a repair. Cost is O(all commits); use
    /// [`Self::verify_commits`] to scope it.
    pub async fn verify_commit_index(&self) -> Result<usize, LedgerError> {
        let ids: Vec<String> = sqlx::query("SELECT id FROM commit_index ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?
            .iter()
            .map(|row| row.try_get("id").map_err(storage))
            .collect::<Result<_, _>>()?;
        self.verify_ids(&ids).await
    }

    /// Verify the index rows of exactly these commits. A commit without an index row is
    /// reported as `NotFound`, so an envelope that only exists as raw content cannot
    /// pass for an indexed commit.
    pub async fn verify_commits(&self, ids: &[CommitId]) -> Result<usize, LedgerError> {
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        self.verify_ids(&ids).await
    }

    /// What every byte-derivable column must equal, plus the two relational rules the
    /// index adds: ordered parent rows, and every parent in the child's graph. The one
    /// column that is *not* byte-derivable is a v1 row's `graph_id` (binding policy,
    /// ADR-0010); it is checked only for consistency with the parents' graph.
    async fn verify_ids(&self, ids: &[String]) -> Result<usize, LedgerError> {
        let mut verified = 0usize;
        for id in ids {
            let row = sqlx::query(
                "SELECT c.id, c.graph_id, c.version, c.patch_id, c.parent_count, o.bytes \
                 FROM commit_index c JOIN immutable_objects o ON o.id = c.id WHERE c.id = $1",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
            let content_id = ContentId::from_str(id)?;
            let Some(row) = row else {
                return Err(LedgerError::NotFound(content_id));
            };
            let mismatch = |reason: &str| corrupt(&content_id, reason);
            let bytes: Vec<u8> = row.try_get("bytes").map_err(storage)?;
            if ContentId::for_bytes(&bytes) != content_id {
                return Err(mismatch("bytes do not hash to id"));
            }
            let commit = AnyCommit::from_canonical_bytes(&bytes)
                .map_err(|e| mismatch(&format!("indexed object is not a commit: {e}")))?;
            let indexed = IndexRow::from_row(&row)?;
            indexed.check_against(&commit, &content_id, CheckMode::Verify)?;
            let parents = fetch_parent_rows(&self.pool, id).await?;
            check_parent_rows(&parents, &commit, &content_id)?;
            for (_, parent_id) in &parents {
                let parent_graph = sqlx::query("SELECT graph_id FROM commit_index WHERE id = $1")
                    .bind(parent_id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(storage)?;
                let Some(parent_graph) = parent_graph else {
                    return Err(mismatch("indexed parent is not itself indexed"));
                };
                let parent_graph: String = parent_graph.try_get("graph_id").map_err(storage)?;
                if parent_graph != indexed.graph_id {
                    return Err(mismatch("indexed parent belongs to another graph"));
                }
            }
            verified += 1;
        }
        Ok(verified)
    }
}

/// How an index row is compared with a commit: at publication the caller's requested
/// binding is authoritative and a different stored binding is a legitimate conflict; at
/// verification any divergence of a byte-derivable column is corruption.
#[derive(Clone, Copy)]
enum CheckMode<'a> {
    Publish { requested_graph: &'a GraphId },
    Verify,
}

/// The metadata `commit_index` holds for one commit, as read back from the database.
struct IndexRow {
    graph_id: String,
    version: i16,
    patch_id: String,
    parent_count: i16,
}

impl IndexRow {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, LedgerError> {
        Ok(Self {
            graph_id: row.try_get("graph_id").map_err(storage)?,
            version: row.try_get("version").map_err(storage)?,
            patch_id: row.try_get("patch_id").map_err(storage)?,
            parent_count: row.try_get("parent_count").map_err(storage)?,
        })
    }

    /// Compare with what the bytes say (and, at publication, with the requested binding).
    fn check_against(
        &self,
        commit: &AnyCommit,
        id: &ContentId,
        mode: CheckMode<'_>,
    ) -> Result<(), LedgerError> {
        match mode {
            CheckMode::Publish { requested_graph } => {
                let expected = commit
                    .graph_id()
                    .map(GraphId::as_str)
                    .unwrap_or(requested_graph.as_str());
                if expected != self.graph_id {
                    // A legitimately different binding is a conflict, not corruption.
                    return Err(LedgerError::GraphBindingConflict {
                        commit: CommitId(id.clone()),
                        indexed: self.graph_id.clone(),
                        requested: expected.to_owned(),
                    });
                }
            }
            CheckMode::Verify => {
                if let Some(embedded) = commit.graph_id()
                    && embedded.as_str() != self.graph_id
                {
                    return Err(corrupt(id, "indexed graph_id differs from bytes"));
                }
            }
        }
        if self.version != i16::from(commit.version()) {
            return Err(corrupt(id, "indexed version differs from bytes"));
        }
        if self.patch_id != commit.patch().to_string() {
            return Err(corrupt(id, "indexed patch_id differs from bytes"));
        }
        if usize::try_from(self.parent_count).ok() != Some(commit.parents().len()) {
            return Err(corrupt(id, "indexed parent_count differs from bytes"));
        }
        Ok(())
    }
}

async fn fetch_parent_rows<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    commit_id: &str,
) -> Result<Vec<(i16, String)>, LedgerError> {
    let rows = sqlx::query(
        "SELECT position, parent_id FROM commit_parents WHERE commit_id = $1 ORDER BY position",
    )
    .bind(commit_id)
    .fetch_all(executor)
    .await
    .map_err(storage)?;
    rows.iter()
        .map(|row| {
            Ok((
                row.try_get("position").map_err(storage)?,
                row.try_get("parent_id").map_err(storage)?,
            ))
        })
        .collect()
}

fn check_parent_rows(
    rows: &[(i16, String)],
    commit: &AnyCommit,
    id: &ContentId,
) -> Result<(), LedgerError> {
    if rows.len() != commit.parents().len() {
        return Err(corrupt(id, "commit_parents row count differs from bytes"));
    }
    for (expected_position, ((position, parent_id), parent)) in
        rows.iter().zip(commit.parents()).enumerate()
    {
        if usize::try_from(*position).ok() != Some(expected_position)
            || *parent_id != parent.to_string()
        {
            return Err(corrupt(id, "commit_parents differ from bytes"));
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl ImmutableStore for PostgresImmutableStore {
    async fn put_content(&self, id: &ContentId, bytes: &[u8]) -> Result<(), LedgerError> {
        reject_commit_bytes_as_content(bytes)?;
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        publish_object(&mut conn, id, bytes).await
    }

    async fn get_content(&self, id: &ContentId) -> Result<Option<Vec<u8>>, LedgerError> {
        let row = sqlx::query("SELECT bytes FROM immutable_objects WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let bytes: Vec<u8> = row.try_get("bytes").map_err(storage)?;
        if &ContentId::for_bytes(&bytes) != id {
            return Err(corrupt(id, "stored bytes do not hash to id"));
        }
        Ok(Some(bytes))
    }

    /// One transaction: typed same-graph parent checks, patch existence, object
    /// publication, index rows, then verification of the *authoritative* index row and
    /// parent rows against the commit. Concurrent publication of the same id under an
    /// incompatible binding is resolved by the database (the second inserter waits on the
    /// unique index, does nothing, reads the winner's row, and fails with
    /// `GraphBindingConflict`); identical concurrent publications both succeed.
    async fn put_commit(&self, commit: &AnyCommit) -> Result<CommitId, LedgerError> {
        let graph_id = self.graph_for(commit)?;
        let bytes = commit.canonical_bytes()?;
        let id = commit.id()?;
        let id_s = id.to_string();
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // The conflict-resolution contract below (loser reads the winner's committed row
        // after its blocked INSERT) is a READ COMMITTED property; pin it regardless of
        // the server's default_transaction_isolation.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;

        for parent in commit.parents() {
            let row = sqlx::query("SELECT graph_id FROM commit_index WHERE id = $1")
                .bind(parent.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
            let Some(row) = row else {
                return Err(LedgerError::MissingParent(parent.clone()));
            };
            let parent_graph: String = row.try_get("graph_id").map_err(storage)?;
            if parent_graph != graph_id.as_str() {
                return Err(LedgerError::CrossGraphParent {
                    parent: parent.clone(),
                    parent_graph,
                    graph: graph_id.to_string(),
                });
            }
        }
        let patch_present = sqlx::query("SELECT 1 FROM immutable_objects WHERE id = $1")
            .bind(commit.patch().to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
        if patch_present.is_none() {
            return Err(LedgerError::MissingPatch(commit.patch().clone()));
        }
        // A patch must be content, not a commit: a commit whose "patch" is another commit
        // would index but could never be reconstructed.
        let patch_is_commit = sqlx::query("SELECT 1 FROM commit_index WHERE id = $1")
            .bind(commit.patch().to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
        if patch_is_commit.is_some() {
            return Err(LedgerError::InvalidCommit(format!(
                "patch {} names an indexed commit, not a patch",
                commit.patch()
            )));
        }
        let graph_row = sqlx::query("SELECT status FROM graphs WHERE graph_id = $1")
            .bind(graph_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
        let Some(graph_row) = graph_row else {
            return Err(LedgerError::UnknownGraph(graph_id.to_string()));
        };
        if commit.graph_id().is_none() {
            // ADR-0010: v1 history may only be bound to the bootstrap graph or to a graph
            // that is explicitly receiving an audited import.
            let status: String = graph_row.try_get("status").map_err(storage)?;
            if status != "bootstrap" && status != "importing" {
                return Err(LedgerError::InvalidCommit(format!(
                    "v1 commits may only be bound to a graph in status bootstrap or importing; \
                     {graph_id} is {status}"
                )));
            }
        }

        publish_object(&mut tx, &id.0, &bytes).await?;
        let parent_count = i16::try_from(commit.parents().len())
            .map_err(|_| LedgerError::InvalidCommit("parent count exceeds index range".into()))?;
        sqlx::query(
            "INSERT INTO commit_index (id, graph_id, version, patch_id, parent_count) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&id_s)
        .bind(graph_id.as_str())
        .bind(i16::from(commit.version()))
        .bind(commit.patch().to_string())
        .bind(parent_count)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        for (position, parent) in commit.parents().iter().enumerate() {
            let position = i16::try_from(position)
                .map_err(|_| LedgerError::InvalidCommit("parent position exceeds range".into()))?;
            sqlx::query(
                "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, $2, $3) \
                 ON CONFLICT (commit_id, position) DO NOTHING",
            )
            .bind(&id_s)
            .bind(position)
            .bind(parent.to_string())
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }

        // Authoritative verification: whatever row now exists (ours, or a concurrent or
        // pre-existing one) must agree with this commit and the requested binding.
        let row = sqlx::query(
            "SELECT graph_id, version, patch_id, parent_count FROM commit_index WHERE id = $1",
        )
        .bind(&id_s)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        IndexRow::from_row(&row)?.check_against(
            commit,
            &id.0,
            CheckMode::Publish {
                requested_graph: &graph_id,
            },
        )?;
        let parents = fetch_parent_rows(&mut *tx, &id_s).await?;
        check_parent_rows(&parents, commit, &id.0)?;

        tx.commit().await.map_err(storage)?;
        Ok(id)
    }

    /// Typed read: a stored object that is not a commit envelope yields `None`; a
    /// commit-family object that does not decode, or does not hash to its id, is an
    /// error. Bytes are authoritative; the index is a derived view of them.
    async fn get_commit(&self, id: &CommitId) -> Result<Option<AnyCommit>, LedgerError> {
        let Some(bytes) = self.get_content(&id.0).await? else {
            return Ok(None);
        };
        decode_commit_object(&id.0, &bytes)
    }

    async fn exists(&self, id: &ContentId) -> Result<bool, LedgerError> {
        let row = sqlx::query("SELECT 1 FROM immutable_objects WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
        Ok(row.is_some())
    }
}
