//! PostgreSQL-backed shared immutable store (ADR-0012): content-addressed
//! `immutable_objects` plus a derived, verified `commit_index`/`commit_parents` written
//! in the same transaction as the commit bytes. Every replica sharing the database sees
//! the same content, which is what makes multi-replica deployment correct.
//!
//! Publication is *truthful*: after every `INSERT … ON CONFLICT DO NOTHING` the
//! authoritative row is read back and compared with what the caller derived. A conflict
//! therefore means "identical, already published" or an explicit error — never a silent
//! success over foreign bytes or a foreign graph binding.

use crate::{db_error, decode_commit_object, reject_commit_bytes_as_content, validate_patch_bytes};
use ledger_core::{AnyCommit, CommitId, ContentId, GraphId, ImmutableStore, LedgerError};
use sqlx::{PgConnection, PgPool, Row, postgres::PgPoolOptions};
use std::str::FromStr;

/// Bounds of one retrieval window (Plan 0012). Reconstruction and ancestry walks fetch
/// immutable rows in windows of this size instead of one row per ancestor; the windows
/// bound memory and the work of one statement, and are not a correctness parameter: every
/// window size yields the same states, histories and errors. Crate-internal defaults; the
/// `test-hooks` feature lets tests shrink them to exercise the window boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetrievalWindows {
    /// Commits per first-parent chain window and patches per patch window (≥ 1).
    pub objects: usize,
    /// Object bytes one window may return before it is cut (the first object of a window
    /// is always returned, so a window holds at most this many bytes plus one object).
    pub bytes: usize,
    /// Commits per DAG ancestry window (≥ 1). `1` is the unwindowed walk of Phase 4/5.
    pub ancestry: usize,
}

impl RetrievalWindows {
    /// 256 objects and 8 MiB per window: at the ≈ 100 µs per round trip Plan 0011 measured,
    /// the amortized round trip per ancestor is under 1 µs (under a tenth of the fold's
    /// ≥ 10 µs), while a window's recursive query is 256 primary-key probes and its memory
    /// at most 256 envelopes or 8 MiB plus one object.
    pub const DEFAULT: Self = Self {
        objects: 256,
        bytes: 8 * 1024 * 1024,
        ancestry: 256,
    };

    fn checked(self) -> Self {
        Self {
            objects: self.objects.max(1),
            bytes: self.bytes.max(1),
            ancestry: self.ancestry.max(1),
        }
    }
}

impl Default for RetrievalWindows {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One requested object of a retrieval window, before verification. The bytes are reachable
/// only through [`FetchedObject::verified`], which re-hashes them against the requested id,
/// so no caller can use stored bytes the database returned without that check; `None`
/// bytes mean the row does not exist (the caller maps that to its typed missing-object
/// error, never to a shorter history).
pub(crate) struct FetchedObject {
    id: ContentId,
    bytes: Option<Vec<u8>>,
}

impl FetchedObject {
    /// `Ok(None)`: no such object. `Err(CorruptObject)`: the stored bytes do not hash to
    /// the id (the wording of every scalar object read in this crate). `Ok(Some)`: verified.
    pub(crate) fn verified(self) -> Result<Option<Vec<u8>>, LedgerError> {
        let Some(bytes) = self.bytes else {
            return Ok(None);
        };
        if ContentId::for_bytes(&bytes) != self.id {
            return Err(corrupt(&self.id, "stored bytes do not hash to id"));
        }
        Ok(Some(bytes))
    }
}

/// Retrieve several immutable objects in one statement on the caller's connection
/// (Plan 0012 M1). `ids` are bound as one array parameter and never interpolated; the
/// reply is re-ordered by the request position regardless of the row order PostgreSQL
/// chose, so the result is a prefix of `ids` in request order: every requested id up to the
/// cut is accounted for, a missing row as `None` bytes. The cut keeps the window's bytes
/// bounded: PostgreSQL stops adding rows once the bytes of the preceding rows reach
/// `max_bytes` (the first row is always served), using the stored length without
/// detoasting the objects it does not return. The caller continues from the first
/// unserved id. At most `max_objects` ids may be requested.
pub(crate) async fn fetch_objects_window(
    conn: &mut PgConnection,
    ids: &[ContentId],
    windows: RetrievalWindows,
) -> Result<Vec<FetchedObject>, LedgerError> {
    let windows = windows.checked();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > windows.objects {
        return Err(LedgerError::Storage(format!(
            "retrieval window of {} objects exceeds the bound {}",
            ids.len(),
            windows.objects
        )));
    }
    let requested: Vec<String> = ids.iter().map(ToString::to_string).collect();
    let rows = sqlx::query(
        "WITH req AS ( \
             SELECT r.id, r.ord FROM unnest($1::text[]) WITH ORDINALITY AS r(id, ord) \
         ), sized AS ( \
             SELECT r.ord, r.id, o.bytes, \
                    sum(octet_length(o.bytes)) OVER ( \
                        ORDER BY r.ord ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING \
                    ) AS before \
             FROM req r LEFT JOIN immutable_objects o ON o.id = r.id \
         ) \
         SELECT ord, id, bytes FROM sized WHERE before IS NULL OR before < $2 ORDER BY ord",
    )
    .bind(&requested)
    .bind(i64::try_from(windows.bytes).unwrap_or(i64::MAX))
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)?;
    let mut served: Vec<(i64, String, Option<Vec<u8>>)> = rows
        .iter()
        .map(|row| {
            Ok((
                row.try_get("ord").map_err(db_error)?,
                row.try_get("id").map_err(db_error)?,
                row.try_get("bytes").map_err(db_error)?,
            ))
        })
        .collect::<Result<_, LedgerError>>()?;
    served.sort_by_key(|(ord, _, _)| *ord);
    // Deterministic whatever the row order: the reply must be exactly the positions
    // 1..=n of the request, each naming the id requested there.
    if served.is_empty() {
        return Err(LedgerError::Storage(
            "retrieval window returned no row for a non-empty request".into(),
        ));
    }
    let mut out = Vec::with_capacity(served.len());
    for (position, (ord, id, bytes)) in served.into_iter().enumerate() {
        let expected = &requested[position];
        if usize::try_from(ord).ok() != Some(position + 1) || &id != expected {
            return Err(LedgerError::Storage(
                "retrieval window reply is not a prefix of the request".into(),
            ));
        }
        out.push(FetchedObject {
            id: ids[position].clone(),
            bytes,
        });
    }
    Ok(out)
}

/// `(depth, id, next_hint, bytes)` as the chain-window statement returns it.
type RawChainRow = (i64, String, Option<String>, Option<Vec<u8>>);

/// One row of a first-parent chain window: the commit the index placed at this depth, the
/// id the index names as its first parent (`None`: no position-0 row), and its object.
pub(crate) struct ChainRow {
    pub(crate) id: CommitId,
    pub(crate) next_hint: Option<CommitId>,
    pub(crate) object: FetchedObject,
}

/// A window of the first-parent chain from `anchor`, discovered through `commit_parents`
/// position 0 (a bounded recursive query; the index is a hint for which rows to fetch) and
/// joined to the objects in one statement (Plan 0012 M2). Rows come in chain order,
/// `anchor` first, at most `max_rows`, cut by `windows.bytes` like
/// [`fetch_objects_window`]. The recursion discovers one id beyond the served rows so
/// every served row carries an exact `next_hint`. Objects are unverified
/// [`FetchedObject`]s: the caller verifies them in chain order and must confirm from the
/// decoded bytes that `parents[0]` equals `next_hint` before following it.
pub(crate) async fn fetch_first_parent_window(
    conn: &mut PgConnection,
    anchor: &CommitId,
    max_rows: usize,
    windows: RetrievalWindows,
) -> Result<Vec<ChainRow>, LedgerError> {
    let windows = windows.checked();
    let max_rows = max_rows.clamp(1, windows.objects);
    let rows = sqlx::query(
        "WITH RECURSIVE chain(depth, id) AS ( \
             SELECT 0::bigint, $1::text \
             UNION ALL \
             SELECT c.depth + 1, p.parent_id \
             FROM chain c JOIN commit_parents p ON p.commit_id = c.id AND p.position = 0 \
             WHERE c.depth < $2 \
         ), hinted AS ( \
             SELECT depth, id, lead(id) OVER (ORDER BY depth) AS next_hint FROM chain \
         ), served AS ( \
             SELECT h.depth, h.id, h.next_hint, o.bytes, \
                    sum(octet_length(o.bytes)) OVER ( \
                        ORDER BY h.depth ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING \
                    ) AS before \
             FROM hinted h LEFT JOIN immutable_objects o ON o.id = h.id \
             WHERE h.depth < $2 \
         ) \
         SELECT depth, id, next_hint, bytes FROM served \
         WHERE before IS NULL OR before < $3 ORDER BY depth",
    )
    .bind(anchor.to_string())
    .bind(i64::try_from(max_rows).unwrap_or(i64::MAX))
    .bind(i64::try_from(windows.bytes).unwrap_or(i64::MAX))
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)?;
    let mut served: Vec<RawChainRow> = rows
        .iter()
        .map(|row| {
            Ok((
                row.try_get("depth").map_err(db_error)?,
                row.try_get("id").map_err(db_error)?,
                row.try_get("next_hint").map_err(db_error)?,
                row.try_get("bytes").map_err(db_error)?,
            ))
        })
        .collect::<Result<_, LedgerError>>()?;
    served.sort_by_key(|(depth, _, _, _)| *depth);
    let mut out = Vec::with_capacity(served.len());
    for (position, (depth, id, next_hint, bytes)) in served.into_iter().enumerate() {
        if usize::try_from(depth).ok() != Some(position) {
            return Err(LedgerError::Storage(
                "chain window reply is not a contiguous prefix".into(),
            ));
        }
        let id: CommitId = id.parse()?;
        if position == 0 && &id != anchor {
            return Err(LedgerError::Storage(
                "chain window reply does not start at its anchor".into(),
            ));
        }
        let object = FetchedObject {
            id: id.0.clone(),
            bytes,
        };
        out.push(ChainRow {
            id,
            next_hint: next_hint.map(|h| h.parse()).transpose()?,
            object,
        });
    }
    if out.is_empty() {
        return Err(LedgerError::Storage(
            "chain window returned no row for its anchor".into(),
        ));
    }
    Ok(out)
}

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
    .map_err(db_error)?;
    let row = sqlx::query("SELECT bytes FROM immutable_objects WHERE id = $1")
        .bind(&id_s)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_error)?;
    let stored: Vec<u8> = row.try_get("bytes").map_err(db_error)?;
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
    /// Connect a fresh pool, **verify** the schema level (never migrate) and apply
    /// `v1_binding` on writes.
    pub async fn connect(database_url: &str, v1_binding: V1Binding) -> Result<Self, LedgerError> {
        let pool = crate::DbSessionLimits {
            max_connections: 8,
            ..crate::DbSessionLimits::default()
        }
        .pool_options()
        .connect(database_url)
        .await
        .map_err(db_error)?;
        crate::schema::verify(&pool).await?;
        crate::schema::verify_definitions_at_startup(&pool).await?;
        Ok(Self::from_pool_migrated(pool, v1_binding))
    }

    /// Connect **and migrate** (tests and tooling only; ADR-0016). The server never uses it.
    pub async fn connect_and_migrate(
        database_url: &str,
        v1_binding: V1Binding,
    ) -> Result<Self, LedgerError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(db_error)?;
        Self::from_pool(pool, v1_binding).await
    }

    /// Compose over an existing pool **and migrate** (tests and tooling only; ADR-0016).
    pub async fn from_pool(pool: PgPool, v1_binding: V1Binding) -> Result<Self, LedgerError> {
        crate::schema::migrate_all(&pool).await?;
        Ok(Self::from_pool_migrated(pool, v1_binding))
    }

    /// Compose over a pool whose schema the caller has already migrated.
    pub fn from_pool_migrated(pool: PgPool, v1_binding: V1Binding) -> Self {
        Self { pool, v1_binding }
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
            .map_err(db_error)?
            .iter()
            .map(|row| row.try_get("id").map_err(db_error))
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
            .map_err(db_error)?;
            let content_id = ContentId::from_str(id)?;
            let Some(row) = row else {
                return Err(LedgerError::NotFound(content_id));
            };
            let mismatch = |reason: &str| corrupt(&content_id, reason);
            let bytes: Vec<u8> = row.try_get("bytes").map_err(db_error)?;
            if ContentId::for_bytes(&bytes) != content_id {
                return Err(mismatch("bytes do not hash to id"));
            }
            let commit = AnyCommit::from_canonical_bytes(&bytes)
                .map_err(|e| mismatch(&format!("indexed object is not a commit: {e}")))?;
            let indexed = IndexRow::from_row(&row)?;
            indexed.check_against(&commit, &content_id, CheckMode::Verify)?;
            let patch_row = sqlx::query("SELECT bytes FROM immutable_objects WHERE id = $1")
                .bind(commit.patch().to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(db_error)?;
            let Some(patch_row) = patch_row else {
                return Err(mismatch("referenced patch is missing"));
            };
            let patch_bytes: Vec<u8> = patch_row.try_get("bytes").map_err(db_error)?;
            validate_patch_bytes(commit.patch(), &patch_bytes)
                .map_err(|e| mismatch(&format!("referenced patch is invalid: {e}")))?;
            let parents = fetch_parent_rows(&self.pool, id).await?;
            check_parent_rows(&parents, &commit, &content_id)?;
            for (_, parent_id) in &parents {
                let parent_graph = sqlx::query("SELECT graph_id FROM commit_index WHERE id = $1")
                    .bind(parent_id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(db_error)?;
                let Some(parent_graph) = parent_graph else {
                    return Err(mismatch("indexed parent is not itself indexed"));
                };
                let parent_graph: String = parent_graph.try_get("graph_id").map_err(db_error)?;
                if parent_graph != indexed.graph_id {
                    return Err(mismatch("indexed parent belongs to another graph"));
                }
            }
            verified += 1;
        }
        Ok(verified)
    }

    /// The referenced patch must exist, hash to its id (digest-verified read → corruption
    /// otherwise), and decode as a canonical RDF patch (`InvalidPatch` otherwise).
    pub(crate) async fn validate_patch_of(&self, commit: &AnyCommit) -> Result<(), LedgerError> {
        let Some(patch_bytes) = self.get_content(&commit.patch().0).await? else {
            return Err(LedgerError::MissingPatch(commit.patch().clone()));
        };
        validate_patch_bytes(commit.patch(), &patch_bytes)?;
        Ok(())
    }

    /// Publish a commit inside a caller-owned transaction (already pinned to READ
    /// COMMITTED): typed same-graph parent checks, graph existence and v1 binding policy,
    /// object publication, index rows, and verification of the authoritative row. The
    /// workflow repository reuses this so candidate publication and idempotency commit
    /// together; `put_commit` wraps it in its own transaction. The caller MUST have run
    /// [`Self::validate_patch_of`] (or hold the patch bytes verified) first.
    pub(crate) async fn publish_commit_in(
        &self,
        tx: &mut PgConnection,
        commit: &AnyCommit,
    ) -> Result<CommitId, LedgerError> {
        let graph_id = self.graph_for(commit)?;
        let bytes = commit.canonical_bytes()?;
        let id = commit.id()?;
        let id_s = id.to_string();

        for parent in commit.parents() {
            let row = sqlx::query("SELECT graph_id FROM commit_index WHERE id = $1")
                .bind(parent.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_error)?;
            let Some(row) = row else {
                return Err(LedgerError::MissingParent(parent.clone()));
            };
            let parent_graph: String = row.try_get("graph_id").map_err(db_error)?;
            if parent_graph != graph_id.as_str() {
                return Err(LedgerError::CrossGraphParent {
                    parent: parent.clone(),
                    parent_graph,
                    graph: graph_id.to_string(),
                });
            }
        }
        // Re-check existence inside the transaction so the FK on patch_id cannot surface
        // as an untyped error if the patch row is somehow absent here.
        let patch_present = sqlx::query("SELECT 1 FROM immutable_objects WHERE id = $1")
            .bind(commit.patch().to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_error)?;
        if patch_present.is_none() {
            return Err(LedgerError::MissingPatch(commit.patch().clone()));
        }
        let graph_row = sqlx::query("SELECT status FROM graphs WHERE graph_id = $1")
            .bind(graph_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_error)?;
        let Some(graph_row) = graph_row else {
            return Err(LedgerError::UnknownGraph(graph_id.to_string()));
        };
        if commit.graph_id().is_none() {
            // ADR-0010: v1 history may only be bound to the bootstrap graph or to a graph
            // that is explicitly receiving an audited import.
            let status: String = graph_row.try_get("status").map_err(db_error)?;
            if status != "bootstrap" && status != "importing" {
                return Err(LedgerError::InvalidCommit(format!(
                    "v1 commits may only be bound to a graph in status bootstrap or importing; \
                     {graph_id} is {status}"
                )));
            }
        }

        publish_object(&mut *tx, &id.0, &bytes).await?;
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
        .map_err(db_error)?;
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
            .map_err(db_error)?;
        }

        // Authoritative verification: whatever row now exists (ours, or a concurrent or
        // pre-existing one) must agree with this commit and the requested binding.
        let row = sqlx::query(
            "SELECT graph_id, version, patch_id, parent_count FROM commit_index WHERE id = $1",
        )
        .bind(&id_s)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
        IndexRow::from_row(&row)?.check_against(
            commit,
            &id.0,
            CheckMode::Publish {
                requested_graph: &graph_id,
            },
        )?;
        let parents = fetch_parent_rows(&mut *tx, &id_s).await?;
        check_parent_rows(&parents, commit, &id.0)?;
        Ok(id)
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
            graph_id: row.try_get("graph_id").map_err(db_error)?,
            version: row.try_get("version").map_err(db_error)?,
            patch_id: row.try_get("patch_id").map_err(db_error)?,
            parent_count: row.try_get("parent_count").map_err(db_error)?,
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
    .map_err(db_error)?;
    rows.iter()
        .map(|row| {
            Ok((
                row.try_get("position").map_err(db_error)?,
                row.try_get("parent_id").map_err(db_error)?,
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
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        publish_object(&mut conn, id, bytes).await
    }

    async fn get_content(&self, id: &ContentId) -> Result<Option<Vec<u8>>, LedgerError> {
        let row = sqlx::query("SELECT bytes FROM immutable_objects WHERE id = $1")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let bytes: Vec<u8> = row.try_get("bytes").map_err(db_error)?;
        if &ContentId::for_bytes(&bytes) != id {
            return Err(corrupt(id, "stored bytes do not hash to id"));
        }
        Ok(Some(bytes))
    }

    /// One transaction: typed same-graph parent checks, patch validity, object
    /// publication, index rows, then verification of the *authoritative* index row and
    /// parent rows against the commit. Concurrent publication of the same id under an
    /// incompatible binding is resolved by the database (the second inserter waits on the
    /// unique index, does nothing, reads the winner's row, and fails with
    /// `GraphBindingConflict`); identical concurrent publications both succeed.
    async fn put_commit(&self, commit: &AnyCommit) -> Result<CommitId, LedgerError> {
        // Patch validity is checked before the transaction: rows are write-once, so the
        // check is exact and keeps parsing out of the lock window.
        self.validate_patch_of(commit).await?;
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        let id = self.publish_commit_in(&mut tx, commit).await?;
        tx.commit().await.map_err(db_error)?;
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
            .map_err(db_error)?;
        Ok(row.is_some())
    }
}
