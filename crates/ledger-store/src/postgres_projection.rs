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
/// The only ref projection v1 projects (ADR-0020).
pub const PROJECTED_REF: &str = "main";
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
    /// The stream is being disabled: the only work is fencing the target (ADR-0020), then
    /// [`ProjectionRepository::finish_disable`].
    pub disabling: bool,
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

/// Which event a claimed stream projects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkMode {
    /// The latest accepted event beyond the recorded progress (normal processing).
    Pending,
    /// The ref's accepted head event (operator rebuild).
    Head,
    /// The recorded projection itself (reconciliation of an idle stream).
    Recorded,
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
    /// CHECK and partial-index fingerprints validated at start-up (projector identity);
    /// readiness refuses any change (as the runtime store does).
    fingerprints: Option<std::collections::BTreeMap<String, String>>,
}

impl ProjectionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            fingerprints: None,
        }
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
        let report = crate::schema::verify(&pool).await?;
        crate::schema::verify_projector_identity(&pool).await?;
        crate::schema::verify_definitions(&pool).await?;
        Ok(Self {
            pool,
            fingerprints: Some(report.fingerprints),
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Readiness: the schema is still exactly what this build requires, no CHECK or
    /// partial-index definition changed since start-up, and the connected role still holds
    /// exactly the projector model (catalog reads only).
    pub async fn ready(&self) -> Result<(), LedgerError> {
        let report = crate::schema::verify(&self.pool).await?;
        if let Some(expected) = &self.fingerprints
            && *expected != report.fingerprints
        {
            let changed: Vec<&String> = expected
                .iter()
                .filter(|(k, v)| report.fingerprints.get(*k) != Some(v))
                .map(|(k, _)| k)
                .collect();
            return Err(LedgerError::SchemaIncompatible(format!(
                "constraint or index definitions changed since start-up ({changed:?}); \
                 refusing to project until they are verified again"
            )));
        }
        if self.fingerprints.is_some() {
            crate::schema::verify_projector_identity(&self.pool).await?;
        }
        Ok(())
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
        // v1 projects the accepted cognitive ref only; the cognitive graph IRI carries no ref
        // name, so other refs need their own identity rules first (ADR-0020, Phase 4).
        if key.branch != PROJECTED_REF {
            return Err(invalid(
                "branch",
                format!("projection v1 projects the `{PROJECTED_REF}` ref only (ADR-0020)"),
            ));
        }
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
        let status: String = graph.try_get("status").map_err(db_error)?;
        if status != "active" {
            return Err(invalid(
                "graph_id",
                format!("the graph is {status}; only active graphs are projected"),
            ));
        }
        // A head moved without an accepted ref event (bootstrap/import) has no outbox event
        // and cannot be recorded as projected (ADR-0021): accept a change first.
        let unaccounted: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM refs r WHERE r.graph_id = $1 AND r.branch = $2 \
             AND NOT EXISTS (SELECT 1 FROM projection_outbox o WHERE o.graph_id = r.graph_id \
                             AND o.branch = r.branch AND o.ref_version = r.version \
                             AND o.commit_id = r.head))",
        )
        .bind(key.graph_id.as_str())
        .bind(&key.branch)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
        if unaccounted {
            return Err(invalid(
                "branch",
                "the ref head was not reached by an accepted change (bootstrap or import), so it \
                 has no projection event; accept a change on it first",
            ));
        }
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
             AND cognitive_graph = $2 AND status <> 'disabled'",
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
        // A stream's cognitive graph is fixed at creation (identity column); if the graph's
        // KB id changed since, re-enabling would report a graph the stream does not write.
        let recorded: Option<String> = sqlx::query_scalar(
            "SELECT cognitive_graph FROM projection_state \
             WHERE graph_id = $1 AND branch = $2 AND target_id = $3",
        )
        .bind(key.graph_id.as_str())
        .bind(&key.branch)
        .bind(&key.target_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error)?;
        if recorded.as_ref().is_some_and(|g| *g != cognitive_graph) {
            return Err(invalid(
                "knowledge_base_id",
                "the graph's knowledge_base_id changed since this stream was created; a stream's \
                 cognitive graph never changes — enable it under another target id instead",
            ));
        }
        sqlx::query(
            "INSERT INTO projection_state (graph_id, branch, target_id, tenant_id, cognitive_graph) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (graph_id, branch, target_id) DO UPDATE SET status = 'active', \
                 last_success_at = NULL, next_attempt_at = now() \
             WHERE projection_state.status IN ('disabled', 'disabling')",
        )
        .bind(key.graph_id.as_str())
        .bind(&key.branch)
        .bind(&key.target_id)
        .bind(&tenant)
        .bind(&cognitive_graph)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d)
                if d.code().as_deref() == Some("23505")
                    && d.constraint() == Some("projection_state_graph_unique") =>
            {
                invalid(
                    "cognitive_graph",
                    "another stream of this target already projects into this cognitive graph",
                )
            }
            _ => db_error(e),
        })?;
        tx.commit().await.map_err(db_error)?;
        Ok(cognitive_graph)
    }

    /// Start disabling `key` (the row stays; progress is kept): the stream becomes
    /// `disabling` and keeps its cognitive graph until a projector has fenced the target
    /// (ADR-0020) — so no write of this stream still in flight can land after another stream
    /// takes the graph — and then marks it `disabled`. A live lease is left to finish or
    /// expire first. `false` if there is no such stream.
    pub async fn disable(&self, key: &StreamKey) -> Result<bool, LedgerError> {
        validate_key(key)?;
        let done = sqlx::query(
            "UPDATE projection_state SET status = CASE WHEN status = 'disabled' THEN status \
                 ELSE 'disabling' END, next_attempt_at = now() \
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

    /// Disable `key` at once, **without** fencing the target (operator escape hatch when the
    /// target is gone for good): a write of this stream still in flight could then land after
    /// another stream took its cognitive graph. Clears any lease.
    pub async fn disable_unfenced(&self, key: &StreamKey) -> Result<bool, LedgerError> {
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

    /// Complete a disable after the claim's holder fenced the target: `disabling` →
    /// `disabled`, lease released (under the claim's fencing).
    pub async fn finish_disable(&self, claim: &Claim) -> Result<LeaseOutcome, LedgerError> {
        let done = sqlx::query(
            "UPDATE projection_state SET status = 'disabled', lease_owner = NULL, lease_until = NULL, \
                 consecutive_failures = 0 \
             WHERE graph_id = $1 AND branch = $2 AND target_id = $3 AND lease_owner = $4 \
               AND lease_epoch = $5 AND status = 'disabling'",
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

    // ---- projector ----------------------------------------------------------------------

    /// Lease one due stream of `target_id` that is `disabling` (to be fenced) or `active` with
    /// accepted events beyond its recorded progress. `SKIP LOCKED` keeps concurrent claimers from blocking each other;
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
                 WHERE s.target_id = $1 AND s.next_attempt_at <= now() \
                   AND (s.lease_until IS NULL OR s.lease_until < now()) \
                   AND (s.status = 'disabling' OR (s.status = 'active' \
                        AND EXISTS (SELECT 1 FROM projection_outbox o WHERE o.graph_id = s.graph_id \
                                    AND o.branch = s.branch \
                                    AND o.ref_version > coalesce(s.projected_ref_version, 0)))) \
                 ORDER BY (s.status = 'disabling') DESC, s.next_attempt_at, s.graph_id, s.branch \
                 LIMIT 1 FOR UPDATE OF s SKIP LOCKED) \
             UPDATE projection_state s SET lease_owner = $2, \
                 lease_until = now() + make_interval(secs => $3), lease_epoch = s.lease_epoch + 1 \
             FROM candidate c \
             WHERE s.graph_id = c.graph_id AND s.branch = c.branch AND s.target_id = c.target_id \
             RETURNING s.graph_id, s.branch, s.target_id, s.tenant_id, s.cognitive_graph, \
                       s.projected_commit, s.projected_ref_version, s.lease_epoch, s.consecutive_failures, \
                       s.status",
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
    /// `disabling` / `disabled`, as long as no live lease is held.
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
             WHERE s.graph_id = $1 AND s.branch = $2 AND s.target_id = $3 \
               AND s.status NOT IN ('disabling', 'disabled') \
               AND (s.lease_until IS NULL OR s.lease_until < now()) \
             RETURNING s.graph_id, s.branch, s.target_id, s.tenant_id, s.cognitive_graph, \
                       s.projected_commit, s.projected_ref_version, s.lease_epoch, s.consecutive_failures, \
                       s.status",
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

    /// Lease an **idle** active stream (no pending events, projection recorded), due (backoff
    /// respected), whose last successful check is older than `idle`, to re-observe its target
    /// (reconciliation: detects a target that lost or diverged from its data without a new
    /// event). Streams checked least recently — by success or failure — go first.
    pub async fn claim_reconcile(
        &self,
        target_id: &str,
        owner: &str,
        ttl: Duration,
        idle: Duration,
    ) -> Result<Option<Claim>, LedgerError> {
        validate_owner(owner)?;
        let row = sqlx::query(
            "WITH candidate AS ( \
                 SELECT s.graph_id, s.branch, s.target_id FROM projection_state s \
                 WHERE s.target_id = $1 AND s.status = 'active' AND s.projected_ref_version IS NOT NULL \
                   AND (s.lease_until IS NULL OR s.lease_until < now()) \
                   AND s.next_attempt_at <= now() \
                   AND (s.last_success_at IS NULL OR s.last_success_at < now() - make_interval(secs => $4)) \
                   AND NOT EXISTS (SELECT 1 FROM projection_outbox o WHERE o.graph_id = s.graph_id \
                                   AND o.branch = s.branch AND o.ref_version > s.projected_ref_version) \
                 ORDER BY greatest(s.last_success_at, s.last_error_at) NULLS FIRST, s.graph_id, s.branch \
                 LIMIT 1 FOR UPDATE OF s SKIP LOCKED) \
             UPDATE projection_state s SET lease_owner = $2, \
                 lease_until = now() + make_interval(secs => $3), lease_epoch = s.lease_epoch + 1 \
             FROM candidate c \
             WHERE s.graph_id = c.graph_id AND s.branch = c.branch AND s.target_id = c.target_id \
             RETURNING s.graph_id, s.branch, s.target_id, s.tenant_id, s.cognitive_graph, \
                       s.projected_commit, s.projected_ref_version, s.lease_epoch, s.consecutive_failures, \
                       s.status",
        )
        .bind(target_id)
        .bind(owner)
        .bind(ttl.as_secs_f64())
        .bind(idle.as_secs_f64())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        row.map(|r| claim_from_row(&r, owner)).transpose()
    }

    /// The event a claimed stream projects under `mode`, with the ref's accepted head version
    /// read in the same statement (so `head_version >= ref_version` always holds). `None`
    /// when there is nothing to do. `Head` refuses a head that no accepted outbox event
    /// reached (bootstrap/import heads, ADR-0021).
    pub async fn work_for(
        &self,
        claim: &Claim,
        mode: WorkMode,
    ) -> Result<Option<WorkItem>, LedgerError> {
        let recorded = claim.projected.as_ref().map(|(_, v)| *v);
        let (floor, exact) = match mode {
            WorkMode::Pending => (recorded.unwrap_or(0), None),
            WorkMode::Head => (0, None),
            WorkMode::Recorded => match recorded {
                Some(v) => (0, Some(v)),
                None => return Ok(None),
            },
        };
        let row = sqlx::query(
            "SELECT r.version AS head_version, r.head AS head_commit, o.outbox_id, o.commit_id, \
                    o.ref_version \
             FROM refs r \
             JOIN LATERAL (SELECT outbox_id, commit_id, ref_version FROM projection_outbox \
                           WHERE graph_id = r.graph_id AND branch = r.branch AND ref_version > $3 \
                             AND ($4::bigint IS NULL OR ref_version = $4) \
                           ORDER BY ref_version DESC LIMIT 1) o ON true \
             WHERE r.graph_id = $1 AND r.branch = $2",
        )
        .bind(claim.key.graph_id.as_str())
        .bind(&claim.key.branch)
        .bind(floor)
        .bind(exact)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let commit: String = row.try_get("commit_id").map_err(db_error)?;
        let ref_version: i64 = row.try_get("ref_version").map_err(db_error)?;
        let head_version: i64 = row.try_get("head_version").map_err(db_error)?;
        let head_commit: String = row.try_get("head_commit").map_err(db_error)?;
        if mode == WorkMode::Head && (ref_version != head_version || commit != head_commit) {
            return Err(LedgerError::LineageMismatch(
                "the ref head was not reached by an accepted change; it has no projection event"
                    .into(),
            ));
        }
        Ok(Some(WorkItem {
            outbox_id: row.try_get("outbox_id").map_err(db_error)?,
            commit: CommitId::from_str(&commit)?,
            ref_version,
            head_version: head_version.max(ref_version),
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
            // A ledger-state fact, not a transient condition: never retried in a loop.
            return Err(LedgerError::InvalidIdentifier {
                field: "commit",
                reason: "the projected commit is not indexed under the stream's graph".into(),
            });
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
                 last_success_at = now(), next_attempt_at = now(), \
                 status = CASE WHEN status = 'disabling' THEN status ELSE 'active' END, \
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
                 status = CASE WHEN status = 'disabling' THEN status ELSE coalesce($8, status) END \
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

    /// Whether `claim` still holds its lease now (database clock). A worker asks this after
    /// observing the target and before writing (ADR-0020): if it holds the lease at a moment
    /// after its observation, no other worker can have fenced or taken the stream's graph
    /// in between (that needs the lease gone first), so its compare-and-swap is sound.
    pub async fn holds(&self, claim: &Claim) -> Result<bool, LedgerError> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM projection_state WHERE graph_id = $1 AND branch = $2 \
             AND target_id = $3 AND lease_owner = $4 AND lease_epoch = $5 AND lease_until > now())",
        )
        .bind(claim.key.graph_id.as_str())
        .bind(&claim.key.branch)
        .bind(&claim.key.target_id)
        .bind(&claim.owner)
        .bind(claim.epoch)
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)
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

    /// Outbox events of projection-eligible refs (`main` under v1) that no stream projects
    /// (unconfigured backlog).
    pub async fn unconfigured_pending(&self) -> Result<i64, LedgerError> {
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        Self::unconfigured_pending_on(&mut conn).await
    }

    /// [`Self::unconfigured_pending`] on a caller's connection (for example inside one
    /// snapshot with other readings).
    pub async fn unconfigured_pending_on(
        conn: &mut sqlx::PgConnection,
    ) -> Result<i64, LedgerError> {
        // Only projection-eligible refs count (ADR-0022): v1 projects `main`, so outbox rows
        // of cognitive work branches are kept (future protocols) but never a backlog alarm.
        sqlx::query_scalar(
            "SELECT count(*) FROM projection_outbox o WHERE o.delivered_at IS NULL \
             AND o.branch = $1 \
             AND NOT EXISTS (SELECT 1 FROM projection_state s WHERE s.graph_id = o.graph_id \
                             AND s.branch = o.branch AND s.status <> 'disabled')",
        )
        .bind(PROJECTED_REF)
        .fetch_one(conn)
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
        disabling: row.try_get::<String, _>("status").map_err(db_error)? == "disabling",
    })
}
