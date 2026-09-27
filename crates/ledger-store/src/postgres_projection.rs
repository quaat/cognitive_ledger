//! Projection streams (ADR-0021; Plan 0007): durable per-stream progress, stream leases
//! fenced by `lease_epoch`, backoff, and status. The projector never holds a transaction
//! while it talks to the target: [`ProjectionRepository::claim`] commits the lease,
//! [`ProjectionRepository::acknowledge`] / [`ProjectionRepository::fail`] commit the outcome
//! only if the lease is still the caller's.
//!
//! Enabling and disabling a stream are operator actions under the owner identity; the
//! projector identity only updates progress, lease, backoff and error columns (0011 grants).

use crate::{ReconstructionLimits, db_error, postgres_workflow::WorkflowRepository};
use ledger_core::{CommitId, GraphId, LedgerError, TenantId};
use ledger_rdf::Quad;
use sqlx::{PgPool, Row};
use std::{collections::BTreeSet, str::FromStr, time::Duration};

pub const MAX_TARGET_ID_BYTES: usize = 128;
pub const MAX_LEASE_OWNER_BYTES: usize = 256;

/// One projection stream's identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StreamKey {
    pub graph_id: GraphId,
    pub branch: String,
    pub target_id: String,
}

/// A stream lease held by `owner` at `epoch` (the fencing token).
#[derive(Clone, Debug)]
pub struct Claim {
    pub key: StreamKey,
    pub tenant_id: TenantId,
    /// The cognitive graph IRI recorded at enable time.
    pub cognitive_graph: String,
    pub projected: Option<(CommitId, i64)>,
    pub owner: String,
    pub epoch: i64,
    /// Failed attempts since the last success (backoff input).
    pub consecutive_failures: i32,
}

/// What a claimed stream must project: the latest accepted outbox event beyond its recorded
/// progress, and the ref's accepted head version.
#[derive(Clone, Debug)]
pub struct WorkItem {
    pub outbox_id: i64,
    pub commit: CommitId,
    pub ref_version: i64,
    pub head_version: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseOutcome {
    Committed,
    /// The lease expired and another worker holds the stream; nothing was written.
    LeaseLost,
}

/// Where a failed attempt leaves the stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureDisposition {
    /// Retry after the given delay.
    Retry(Duration),
    /// Permanent target/configuration error: `blocked` until an operator acts.
    Block,
    /// The target is ahead of the ledger: `rebuild_required` (never regressed automatically).
    RebuildRequired,
}

/// A stream's status joined with the ledger's accepted head and the pending backlog.
#[derive(Clone, Debug)]
pub struct StreamStatus {
    pub key: StreamKey,
    pub cognitive_graph: String,
    pub status: String,
    pub projected_commit: Option<String>,
    pub projected_ref_version: Option<i64>,
    pub head_commit: Option<String>,
    pub head_version: Option<i64>,
    pub pending_events: i64,
    pub oldest_pending_seconds: Option<f64>,
    pub last_success_seconds_ago: Option<f64>,
    pub last_error_code: Option<String>,
    pub consecutive_failures: i32,
    pub rebuilds: i64,
    pub leased: bool,
}

impl StreamStatus {
    /// Accepted versions not yet represented in the target.
    pub fn lag_versions(&self) -> i64 {
        self.head_version.unwrap_or(0) - self.projected_ref_version.unwrap_or(0)
    }
}

fn invalid(field: &'static str, reason: impl Into<String>) -> LedgerError {
    LedgerError::InvalidIdentifier {
        field,
        reason: reason.into(),
    }
}

fn validate_key(key: &StreamKey) -> Result<(), LedgerError> {
    ledger_core::validate_token("branch", &key.branch, crate::MAX_BRANCH_BYTES)?;
    if key.target_id.is_empty()
        || key.target_id.len() > MAX_TARGET_ID_BYTES
        || !key
            .target_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
    {
        return Err(invalid("target_id", "must match [A-Za-z0-9._:-]{1,128}"));
    }
    Ok(())
}

fn validate_owner(owner: &str) -> Result<(), LedgerError> {
    ledger_core::validate_token("lease_owner", owner, MAX_LEASE_OWNER_BYTES)
}

#[derive(Clone, Debug)]
pub struct ProjectionRepository {
    pool: PgPool,
}

impl ProjectionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Connect as the **projector** identity: verify the schema level, every definition, and
    /// that the connected role holds exactly the projector privilege model (ADR-0021). Never
    /// migrates.
    pub async fn connect(
        database_url: &str,
        limits: crate::DbSessionLimits,
    ) -> Result<Self, LedgerError> {
        let pool = limits
            .pool_options()
            .connect(database_url)
            .await
            .map_err(db_error)?;
        crate::schema::verify(&pool).await?;
        crate::schema::verify_projector_identity(&pool).await?;
        crate::schema::verify_definitions(&pool).await?;
        Ok(Self::new(pool))
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Readiness: the schema is still exactly what this build requires.
    pub async fn ready(&self) -> Result<(), LedgerError> {
        crate::schema::verify(&self.pool).await.map(|_| ())
    }

    // ---- operator (owner identity) ------------------------------------------------------

    /// Enable projection of `key` into `cognitive_graph` (derived by the caller from the
    /// graph's knowledge-base id, ADR-0020). Idempotent for the same graph; re-enables a
    /// disabled stream. Refuses a graph without a knowledge-base id, an unknown graph, and a
    /// cognitive graph another stream of the target already writes.
    pub async fn enable(
        &self,
        key: &StreamKey,
        derive_cognitive_graph: impl FnOnce(&str) -> Result<String, String>,
    ) -> Result<String, LedgerError> {
        validate_key(key)?;
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        let graph = sqlx::query(
            "SELECT tenant_id, knowledge_base_id, status FROM graphs WHERE graph_id = $1 FOR SHARE",
        )
        .bind(key.graph_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error)?
        .ok_or_else(|| invalid("graph_id", "no such graph"))?;
        let tenant: String = graph.try_get("tenant_id").map_err(db_error)?;
        let kb: Option<String> = graph.try_get("knowledge_base_id").map_err(db_error)?;
        let kb = kb.ok_or_else(|| {
            invalid(
                "knowledge_base_id",
                "the graph has no knowledge_base_id; a projection target is never invented \
                 (ADR-0020)",
            )
        })?;
        let cognitive_graph =
            derive_cognitive_graph(&kb).map_err(|reason| invalid("knowledge_base_id", reason))?;
        if let Some(existing) = sqlx::query(
            "SELECT graph_id, branch FROM projection_state WHERE target_id = $1 \
             AND cognitive_graph = $2",
        )
        .bind(&key.target_id)
        .bind(&cognitive_graph)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error)?
        {
            let graph_id: String = existing.try_get("graph_id").map_err(db_error)?;
            let branch: String = existing.try_get("branch").map_err(db_error)?;
            if graph_id != key.graph_id.as_str() || branch != key.branch {
                return Err(invalid(
                    "cognitive_graph",
                    "another stream of this target already projects into this cognitive graph",
                ));
            }
        }
        sqlx::query(
            "INSERT INTO projection_state (graph_id, branch, target_id, tenant_id, cognitive_graph) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (graph_id, branch, target_id) DO UPDATE SET status = 'active' \
             WHERE projection_state.status = 'disabled'",
        )
        .bind(key.graph_id.as_str())
        .bind(&key.branch)
        .bind(&key.target_id)
        .bind(&tenant)
        .bind(&cognitive_graph)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        tx.commit().await.map_err(db_error)?;
        Ok(cognitive_graph)
    }

    /// Stop projecting `key` (the row stays; progress is kept).
    pub async fn disable(&self, key: &StreamKey) -> Result<bool, LedgerError> {
        validate_key(key)?;
        let done = sqlx::query(
            "UPDATE projection_state SET status = 'disabled', lease_owner = NULL, lease_until = NULL \
             WHERE graph_id = $1 AND branch = $2 AND target_id = $3",
        )
        .bind(key.graph_id.as_str())
        .bind(&key.branch)
        .bind(&key.target_id)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(done.rows_affected() == 1)
    }

    // ---- projector ----------------------------------------------------------------------

    /// Lease one due, active stream of `target_id` that has accepted events beyond its
    /// recorded progress. `SKIP LOCKED` keeps concurrent claimers from blocking each other;
    /// an expired lease is claimable (crash recovery). Commits before returning.
    pub async fn claim(
        &self,
        target_id: &str,
        owner: &str,
        ttl: Duration,
    ) -> Result<Option<Claim>, LedgerError> {
        validate_owner(owner)?;
        let row = sqlx::query(
            "WITH candidate AS ( \
                 SELECT s.graph_id, s.branch, s.target_id FROM projection_state s \
                 WHERE s.target_id = $1 AND s.status = 'active' AND s.next_attempt_at <= now() \
                   AND (s.lease_until IS NULL OR s.lease_until < now()) \
                   AND EXISTS (SELECT 1 FROM projection_outbox o WHERE o.graph_id = s.graph_id \
                               AND o.branch = s.branch \
                               AND o.ref_version > coalesce(s.projected_ref_version, 0)) \
                 ORDER BY s.next_attempt_at, s.graph_id, s.branch \
                 LIMIT 1 FOR UPDATE OF s SKIP LOCKED) \
             UPDATE projection_state s SET lease_owner = $2, \
                 lease_until = now() + make_interval(secs => $3), lease_epoch = s.lease_epoch + 1 \
             FROM candidate c \
             WHERE s.graph_id = c.graph_id AND s.branch = c.branch AND s.target_id = c.target_id \
             RETURNING s.graph_id, s.branch, s.target_id, s.tenant_id, s.cognitive_graph, \
                       s.projected_commit, s.projected_ref_version, s.lease_epoch, s.consecutive_failures",
        )
        .bind(target_id)
        .bind(owner)
        .bind(ttl.as_secs_f64())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        row.map(|r| claim_from_row(&r, owner)).transpose()
    }

    /// Lease a specific stream for an operator rebuild, whatever its status except
    /// `disabled`, as long as no live lease is held.
    pub async fn claim_stream(
        &self,
        key: &StreamKey,
        owner: &str,
        ttl: Duration,
    ) -> Result<Option<Claim>, LedgerError> {
        validate_key(key)?;
        validate_owner(owner)?;
        let row = sqlx::query(
            "UPDATE projection_state s SET lease_owner = $4, \
                 lease_until = now() + make_interval(secs => $5), lease_epoch = s.lease_epoch + 1 \
             WHERE s.graph_id = $1 AND s.branch = $2 AND s.target_id = $3 AND s.status <> 'disabled' \
               AND (s.lease_until IS NULL OR s.lease_until < now()) \
             RETURNING s.graph_id, s.branch, s.target_id, s.tenant_id, s.cognitive_graph, \
                       s.projected_commit, s.projected_ref_version, s.lease_epoch, s.consecutive_failures",
        )
        .bind(key.graph_id.as_str())
        .bind(&key.branch)
        .bind(&key.target_id)
        .bind(owner)
        .bind(ttl.as_secs_f64())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        row.map(|r| claim_from_row(&r, owner)).transpose()
    }

    /// The latest accepted outbox event beyond the claim's recorded progress (or, when
    /// `at_head` is set, the ref's accepted head event for a rebuild) and the head version.
    /// `None` when there is nothing to do.
    pub async fn work_for(
        &self,
        claim: &Claim,
        at_head: bool,
    ) -> Result<Option<WorkItem>, LedgerError> {
        let head = sqlx::query("SELECT version FROM refs WHERE graph_id = $1 AND branch = $2")
            .bind(claim.key.graph_id.as_str())
            .bind(&claim.key.branch)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_error)?;
        let Some(head) = head else {
            return Ok(None);
        };
        let head_version: i64 = head.try_get("version").map_err(db_error)?;
        let floor = if at_head {
            0
        } else {
            claim.projected.as_ref().map_or(0, |(_, v)| *v)
        };
        let row = sqlx::query(
            "SELECT outbox_id, commit_id, ref_version FROM projection_outbox \
             WHERE graph_id = $1 AND branch = $2 AND ref_version > $3 \
             ORDER BY ref_version DESC LIMIT 1",
        )
        .bind(claim.key.graph_id.as_str())
        .bind(&claim.key.branch)
        .bind(floor)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let commit: String = row.try_get("commit_id").map_err(db_error)?;
        Ok(Some(WorkItem {
            outbox_id: row.try_get("outbox_id").map_err(db_error)?,
            commit: CommitId::from_str(&commit)?,
            ref_version: row.try_get("ref_version").map_err(db_error)?,
            head_version,
        }))
    }

    /// The ref's accepted head commit at `version` (from the audit chain), if any.
    pub async fn commit_at(
        &self,
        graph: &GraphId,
        branch: &str,
        version: i64,
    ) -> Result<Option<CommitId>, LedgerError> {
        let head: Option<String> = sqlx::query_scalar(
            "SELECT new_head FROM ref_events WHERE graph_id = $1 AND branch = $2 AND new_version = $3",
        )
        .bind(graph.as_str())
        .bind(branch)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        head.map(|h| CommitId::from_str(&h)).transpose()
    }

    /// Reconstruct the accepted state at `commit` of `graph` (bounded); the commit must be
    /// indexed under that graph.
    pub async fn state_at(
        &self,
        graph: &GraphId,
        commit: &CommitId,
        limits: &ReconstructionLimits,
    ) -> Result<BTreeSet<Quad>, LedgerError> {
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        let indexed: Option<String> =
            sqlx::query_scalar("SELECT graph_id FROM commit_index WHERE id = $1")
                .bind(commit.to_string())
                .fetch_optional(&mut *conn)
                .await
                .map_err(db_error)?;
        if indexed.as_deref() != Some(graph.as_str()) {
            return Err(LedgerError::Storage(
                "the projected commit is not indexed under the stream's graph".into(),
            ));
        }
        Ok(WorkflowRepository::state_at_on(&mut conn, commit, limits)
            .await?
            .state)
    }

    /// Record that the target represents `(commit, version)`: advance the stream, reset
    /// backoff, release the lease and mark every outbox row of the stream up to `version`
    /// delivered — one transaction, only if the caller still holds the lease.
    pub async fn acknowledge(
        &self,
        claim: &Claim,
        attempted_outbox_id: Option<i64>,
        commit: &CommitId,
        version: i64,
        rebuilt: bool,
    ) -> Result<LeaseOutcome, LedgerError> {
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        let done = sqlx::query(
            "UPDATE projection_state SET projected_commit = $6, projected_ref_version = $7, \
                 lease_owner = NULL, lease_until = NULL, consecutive_failures = 0, \
                 last_success_at = now(), next_attempt_at = now(), status = 'active', \
                 rebuilds = rebuilds + $8 \
             WHERE graph_id = $1 AND branch = $2 AND target_id = $3 AND lease_owner = $4 \
               AND lease_epoch = $5",
        )
        .bind(claim.key.graph_id.as_str())
        .bind(&claim.key.branch)
        .bind(&claim.key.target_id)
        .bind(&claim.owner)
        .bind(claim.epoch)
        .bind(commit.to_string())
        .bind(version)
        .bind(i64::from(rebuilt))
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        if done.rows_affected() != 1 {
            tx.rollback().await.map_err(db_error)?;
            return Ok(LeaseOutcome::LeaseLost);
        }
        sqlx::query(
            "UPDATE projection_outbox SET delivered_at = now() \
             WHERE graph_id = $1 AND branch = $2 AND ref_version <= $3 AND delivered_at IS NULL",
        )
        .bind(claim.key.graph_id.as_str())
        .bind(&claim.key.branch)
        .bind(version)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        if let Some(id) = attempted_outbox_id {
            count_attempt(&mut tx, id).await?;
        }
        tx.commit().await.map_err(db_error)?;
        Ok(LeaseOutcome::Committed)
    }

    /// Record a failed attempt under the same fencing and release the lease.
    pub async fn fail(
        &self,
        claim: &Claim,
        attempted_outbox_id: Option<i64>,
        code: &str,
        disposition: FailureDisposition,
    ) -> Result<LeaseOutcome, LedgerError> {
        let (status, delay) = match disposition {
            FailureDisposition::Retry(delay) => (None, delay),
            FailureDisposition::Block => (Some("blocked"), Duration::ZERO),
            FailureDisposition::RebuildRequired => (Some("rebuild_required"), Duration::ZERO),
        };
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        let done = sqlx::query(
            "UPDATE projection_state SET lease_owner = NULL, lease_until = NULL, \
                 consecutive_failures = consecutive_failures + 1, last_error_at = now(), \
                 last_error_code = $6, next_attempt_at = now() + make_interval(secs => $7), \
                 status = coalesce($8, status) \
             WHERE graph_id = $1 AND branch = $2 AND target_id = $3 AND lease_owner = $4 \
               AND lease_epoch = $5",
        )
        .bind(claim.key.graph_id.as_str())
        .bind(&claim.key.branch)
        .bind(&claim.key.target_id)
        .bind(&claim.owner)
        .bind(claim.epoch)
        .bind(code)
        .bind(delay.as_secs_f64())
        .bind(status)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        if done.rows_affected() != 1 {
            tx.rollback().await.map_err(db_error)?;
            return Ok(LeaseOutcome::LeaseLost);
        }
        if let Some(id) = attempted_outbox_id {
            count_attempt(&mut tx, id).await?;
        }
        tx.commit().await.map_err(db_error)?;
        Ok(LeaseOutcome::Committed)
    }

    /// Release a lease without recording an outcome (nothing to do / shutdown).
    pub async fn release(&self, claim: &Claim) -> Result<LeaseOutcome, LedgerError> {
        let done = sqlx::query(
            "UPDATE projection_state SET lease_owner = NULL, lease_until = NULL \
             WHERE graph_id = $1 AND branch = $2 AND target_id = $3 AND lease_owner = $4 \
               AND lease_epoch = $5",
        )
        .bind(claim.key.graph_id.as_str())
        .bind(&claim.key.branch)
        .bind(&claim.key.target_id)
        .bind(&claim.owner)
        .bind(claim.epoch)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(if done.rows_affected() == 1 {
            LeaseOutcome::Committed
        } else {
            LeaseOutcome::LeaseLost
        })
    }

    /// Every stream (optionally of one target) with the ledger head and backlog.
    pub async fn status(&self, target_id: Option<&str>) -> Result<Vec<StreamStatus>, LedgerError> {
        let rows = sqlx::query(
            "SELECT s.graph_id, s.branch, s.target_id, s.cognitive_graph, s.status, \
                    s.projected_commit, s.projected_ref_version, r.head AS head_commit, \
                    r.version AS head_version, s.last_error_code, s.consecutive_failures, \
                    s.rebuilds, (s.lease_until IS NOT NULL AND s.lease_until >= now()) AS leased, \
                    extract(epoch FROM now() - s.last_success_at)::float8 AS last_success_seconds_ago, \
                    (SELECT count(*) FROM projection_outbox o WHERE o.graph_id = s.graph_id \
                       AND o.branch = s.branch \
                       AND o.ref_version > coalesce(s.projected_ref_version, 0)) AS pending_events, \
                    (SELECT extract(epoch FROM now() - min(o.created_at))::float8 FROM projection_outbox o \
                     WHERE o.graph_id = s.graph_id AND o.branch = s.branch \
                       AND o.ref_version > coalesce(s.projected_ref_version, 0)) AS oldest_pending_seconds \
             FROM projection_state s \
             LEFT JOIN refs r ON r.graph_id = s.graph_id AND r.branch = s.branch \
             WHERE $1::text IS NULL OR s.target_id = $1 \
             ORDER BY s.target_id, s.graph_id, s.branch",
        )
        .bind(target_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|r| {
                Ok(StreamStatus {
                    key: StreamKey {
                        graph_id: GraphId::new(
                            r.try_get::<String, _>("graph_id").map_err(db_error)?,
                        )?,
                        branch: r.try_get("branch").map_err(db_error)?,
                        target_id: r.try_get("target_id").map_err(db_error)?,
                    },
                    cognitive_graph: r.try_get("cognitive_graph").map_err(db_error)?,
                    status: r.try_get("status").map_err(db_error)?,
                    projected_commit: r.try_get("projected_commit").map_err(db_error)?,
                    projected_ref_version: r.try_get("projected_ref_version").map_err(db_error)?,
                    head_commit: r.try_get("head_commit").map_err(db_error)?,
                    head_version: r.try_get("head_version").map_err(db_error)?,
                    pending_events: r.try_get("pending_events").map_err(db_error)?,
                    oldest_pending_seconds: r
                        .try_get("oldest_pending_seconds")
                        .map_err(db_error)?,
                    last_success_seconds_ago: r
                        .try_get("last_success_seconds_ago")
                        .map_err(db_error)?,
                    last_error_code: r.try_get("last_error_code").map_err(db_error)?,
                    consecutive_failures: r.try_get("consecutive_failures").map_err(db_error)?,
                    rebuilds: r.try_get("rebuilds").map_err(db_error)?,
                    leased: r.try_get("leased").map_err(db_error)?,
                })
            })
            .collect()
    }

    /// Outbox events of `(graph, branch)` pairs no stream projects (unconfigured backlog).
    pub async fn unconfigured_pending(&self) -> Result<i64, LedgerError> {
        sqlx::query_scalar(
            "SELECT count(*) FROM projection_outbox o WHERE o.delivered_at IS NULL \
             AND NOT EXISTS (SELECT 1 FROM projection_state s WHERE s.graph_id = o.graph_id \
                             AND s.branch = o.branch AND s.status <> 'disabled')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)
    }
}

async fn count_attempt(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    outbox_id: i64,
) -> Result<(), LedgerError> {
    sqlx::query("UPDATE projection_outbox SET attempts = attempts + 1 WHERE outbox_id = $1")
        .bind(outbox_id)
        .execute(&mut **tx)
        .await
        .map_err(db_error)?;
    Ok(())
}

fn claim_from_row(row: &sqlx::postgres::PgRow, owner: &str) -> Result<Claim, LedgerError> {
    let commit: Option<String> = row.try_get("projected_commit").map_err(db_error)?;
    let version: Option<i64> = row.try_get("projected_ref_version").map_err(db_error)?;
    let projected = match (commit, version) {
        (Some(c), Some(v)) => Some((CommitId::from_str(&c)?, v)),
        _ => None,
    };
    Ok(Claim {
        key: StreamKey {
            graph_id: GraphId::new(row.try_get::<String, _>("graph_id").map_err(db_error)?)?,
            branch: row.try_get("branch").map_err(db_error)?,
            target_id: row.try_get("target_id").map_err(db_error)?,
        },
        tenant_id: TenantId::new(row.try_get::<String, _>("tenant_id").map_err(db_error)?)?,
        cognitive_graph: row.try_get("cognitive_graph").map_err(db_error)?,
        projected,
        owner: owner.to_owned(),
        epoch: row.try_get("lease_epoch").map_err(db_error)?,
        consecutive_failures: row.try_get("consecutive_failures").map_err(db_error)?,
    })
}
