//! PostgreSQL-backed shared immutable store (ADR-0012): content-addressed
//! `immutable_objects` plus a derived, verified `commit_index`/`commit_parents` written
//! in the same transaction as the commit bytes. Every replica sharing the database sees
//! the same content, which is what makes multi-replica deployment correct.

use crate::storage;
use ledger_core::{AnyCommit, CommitId, ContentId, GraphId, ImmutableStore, LedgerError};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
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

    fn graph_for(&self, commit: &AnyCommit) -> Result<GraphId, LedgerError> {
        match (commit.graph_id(), &self.v1_binding) {
            (Some(graph), _) => Ok(graph.clone()),
            (None, V1Binding::BindTo(graph)) => Ok(graph.clone()),
            (None, V1Binding::Reject) => Err(LedgerError::InvalidCommit(
                "v1 commits carry no graph_id and this store rejects them (V1Binding::Reject)"
                    .into(),
            )),
        }
    }

    /// Re-derive every `commit_index`/`commit_parents` row from the stored bytes and
    /// compare. Returns the number of indexed commits verified. Any divergence is
    /// reported as `CorruptObject`; this is the ADR-0012 gate check, not a repair.
    pub async fn verify_commit_index(&self) -> Result<usize, LedgerError> {
        let rows = sqlx::query(
            "SELECT c.id, c.graph_id, c.version, c.patch_id, c.parent_count, o.bytes \
             FROM commit_index c JOIN immutable_objects o ON o.id = c.id ORDER BY c.id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut verified = 0usize;
        for row in rows {
            let id: String = row.try_get("id").map_err(storage)?;
            let content_id = ContentId::from_str(&id)?;
            let mismatch = |reason: &str| corrupt(&content_id, reason);
            let bytes: Vec<u8> = row.try_get("bytes").map_err(storage)?;
            if ContentId::for_bytes(&bytes) != content_id {
                return Err(mismatch("bytes do not hash to id"));
            }
            let commit = AnyCommit::from_canonical_bytes(&bytes)
                .map_err(|e| mismatch(&format!("indexed object is not a commit: {e}")))?;
            let version: i16 = row.try_get("version").map_err(storage)?;
            if version != i16::from(commit.version()) {
                return Err(mismatch("indexed version differs from bytes"));
            }
            let patch_id: String = row.try_get("patch_id").map_err(storage)?;
            if patch_id != commit.patch().to_string() {
                return Err(mismatch("indexed patch_id differs from bytes"));
            }
            let parent_count: i16 = row.try_get("parent_count").map_err(storage)?;
            if usize::try_from(parent_count).ok() != Some(commit.parents().len()) {
                return Err(mismatch("indexed parent_count differs from bytes"));
            }
            let graph_id: String = row.try_get("graph_id").map_err(storage)?;
            if let Some(embedded) = commit.graph_id()
                && embedded.as_str() != graph_id
            {
                return Err(mismatch("indexed graph_id differs from bytes"));
            }
            let parents = sqlx::query(
                "SELECT position, parent_id FROM commit_parents WHERE commit_id = $1 \
                 ORDER BY position",
            )
            .bind(&id)
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?;
            if parents.len() != commit.parents().len() {
                return Err(mismatch("commit_parents row count differs from bytes"));
            }
            for (expected_position, (parent_row, parent)) in
                parents.iter().zip(commit.parents()).enumerate()
            {
                let position: i16 = parent_row.try_get("position").map_err(storage)?;
                let parent_id: String = parent_row.try_get("parent_id").map_err(storage)?;
                if usize::try_from(position).ok() != Some(expected_position)
                    || parent_id != parent.to_string()
                {
                    return Err(mismatch("commit_parents differ from bytes"));
                }
            }
            verified += 1;
        }
        Ok(verified)
    }
}

fn corrupt(id: &ContentId, reason: &str) -> LedgerError {
    LedgerError::CorruptObject {
        id: id.clone(),
        reason: reason.to_owned(),
    }
}

#[async_trait::async_trait]
impl ImmutableStore for PostgresImmutableStore {
    async fn put_content(&self, id: &ContentId, bytes: &[u8]) -> Result<(), LedgerError> {
        if &ContentId::for_bytes(bytes) != id {
            return Err(LedgerError::ObjectCollision(id.clone()));
        }
        // Content-addressed: a conflict can only be the same bytes, so DO NOTHING is
        // both the idempotency and the "no partial object" guarantee.
        sqlx::query(
            "INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
        )
        .bind(id.to_string())
        .bind(bytes)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
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

    async fn put_commit(&self, commit: &AnyCommit) -> Result<CommitId, LedgerError> {
        let graph_id = self.graph_for(commit)?;
        let bytes = commit.canonical_bytes()?;
        let id = commit.id()?;
        let id_s = id.to_string();
        let mut tx = self.pool.begin().await.map_err(storage)?;

        // Idempotent replay: an indexed commit with these bytes is already published.
        let indexed = sqlx::query("SELECT graph_id FROM commit_index WHERE id = $1")
            .bind(&id_s)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
        if let Some(row) = indexed {
            let existing: String = row.try_get("graph_id").map_err(storage)?;
            if existing != graph_id.as_str() {
                return Err(corrupt(&id.0, "commit already indexed under another graph"));
            }
            tx.commit().await.map_err(storage)?;
            return Ok(id);
        }

        // Typed parent existence: parents must be indexed commits, not merely objects.
        for parent in commit.parents() {
            let present = sqlx::query("SELECT 1 FROM commit_index WHERE id = $1")
                .bind(parent.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
            if present.is_none() {
                return Err(LedgerError::MissingParent(parent.clone()));
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

        sqlx::query(
            "INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&id_s)
        .bind(&bytes)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
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
        tx.commit().await.map_err(storage)?;
        Ok(id)
    }

    /// Typed read from bytes, never from the index: a stored object that is not a
    /// commit envelope yields `None`; a decodable commit that does not hash to its id
    /// is corruption.
    async fn get_commit(&self, id: &CommitId) -> Result<Option<AnyCommit>, LedgerError> {
        let Some(bytes) = self.get_content(&id.0).await? else {
            return Ok(None);
        };
        let Ok(commit) = AnyCommit::from_canonical_bytes(&bytes) else {
            return Ok(None);
        };
        if commit.id()? != *id {
            return Err(corrupt(&id.0, "commit ID mismatch"));
        }
        Ok(Some(commit))
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
