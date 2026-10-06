//! Named branches (ADR-0022): creation from a source head or a reachable historical commit,
//! tombstone deletion, restore, and the read surface (status, lifecycle and movement
//! history). Every mutation is one transaction with its idempotency result, under the same
//! complete-actor scope and advisory lock as the workflow; `ref_events` stays the only
//! authority for head movement, `branch_events` records the lifecycle.

use crate::postgres_workflow::{
    Operation, StoredResult, validate_branch, validate_reason, validate_scope_fn,
};
use crate::{RequestScope, WorkflowRepository, db_error};
use ledger_core::{CommitId, GraphId, LedgerError, TenantId};
use ledger_dag::{DagError, ParentProvider, TraversalLimits};
use sqlx::{PgConnection, Row};
use tokio::sync::Mutex;

/// Branch policy v1 (immutable at creation). Only tightens the deployment floor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BranchPolicy {
    /// `refs.protected`: strict effective delta; creation, deletion and restore are
    /// administrative.
    pub protected: bool,
    /// Acceptance must cite a conforming validation even where the deployment would allow
    /// unvalidated acceptance.
    pub require_validation: bool,
    /// The accepting principal must differ from the proposing one.
    pub require_distinct_reviewer: bool,
}

pub struct CreateBranchRequest {
    pub scope: RequestScope,
    pub name: String,
    pub source: String,
    /// `None`: the source head.
    pub from_commit: Option<CommitId>,
    pub policy: BranchPolicy,
}

pub struct BranchLifecycleRequest {
    pub scope: RequestScope,
    pub name: String,
    pub reason: Option<String>,
}

/// The durable outcome of a lifecycle operation, as recorded by its lifecycle event (a
/// replay returns exactly this).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchOutcome {
    pub event: BranchEvent,
    pub replayed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchInfo {
    pub graph_id: GraphId,
    pub name: String,
    pub status: String,
    pub lifecycle_version: i64,
    pub origin: String,
    pub source_branch: Option<String>,
    pub source_commit: Option<CommitId>,
    pub head: CommitId,
    pub version: i64,
    pub policy: BranchPolicy,
    pub created_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchEvent {
    pub event_id: i64,
    pub branch: String,
    pub lifecycle_version: i64,
    pub operation: String,
    pub status_after: String,
    pub head: CommitId,
    pub ref_version: i64,
    pub source_branch: Option<String>,
    pub source_commit: Option<CommitId>,
    pub principal_id: String,
    pub principal_type: String,
    pub on_behalf_of: Option<String>,
    pub reason: Option<String>,
    pub correlation_id: Option<String>,
    pub recorded_at: String,
}

/// One head movement of a ref (`ref_events`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefMovement {
    pub event_id: i64,
    pub operation: String,
    pub old_head: Option<CommitId>,
    pub new_head: CommitId,
    pub old_version: Option<i64>,
    pub new_version: i64,
    pub principal_id: String,
    pub principal_type: String,
    pub on_behalf_of: Option<String>,
    pub reason: Option<String>,
    pub recorded_at: String,
}

/// Commit parents of one graph, read on one connection (immutable rows; a commit not indexed
/// under the graph is unknown — foreign commits never resolve). Fails closed on a parent list
/// that disagrees with the indexed `parent_count` (corruption, never "fewer parents").
pub(crate) struct GraphParents<'c> {
    pub(crate) conn: Mutex<&'c mut PgConnection>,
    pub(crate) graph: GraphId,
}

#[async_trait::async_trait]
impl ParentProvider for GraphParents<'_> {
    type Error = LedgerError;
    async fn parents(&self, commit: &CommitId) -> Result<Option<Vec<CommitId>>, LedgerError> {
        let mut conn = self.conn.lock().await;
        let indexed: Option<i16> = sqlx::query_scalar(
            "SELECT parent_count FROM commit_index WHERE id = $1 AND graph_id = $2",
        )
        .bind(commit.to_string())
        .bind(self.graph.as_str())
        .fetch_optional(&mut **conn)
        .await
        .map_err(db_error)?;
        let Some(parent_count) = indexed else {
            return Ok(None);
        };
        let rows = sqlx::query(
            "SELECT position, parent_id FROM commit_parents WHERE commit_id = $1 ORDER BY position",
        )
        .bind(commit.to_string())
        .fetch_all(&mut **conn)
        .await
        .map_err(db_error)?;
        let contiguous = rows.len() == usize::try_from(parent_count).unwrap_or(usize::MAX)
            && rows.iter().enumerate().all(|(i, r)| {
                r.try_get::<i16, _>("position")
                    .is_ok_and(|p| usize::try_from(p) == Ok(i))
            });
        if !contiguous {
            return Err(LedgerError::CorruptObject {
                id: commit.0.clone(),
                reason: format!("commit_parents rows disagree with parent_count {parent_count}"),
            });
        }
        rows.iter()
            .map(|r| {
                r.try_get::<String, _>("parent_id")
                    .map_err(db_error)?
                    .parse()
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }
}

const BRANCH_EVENT_COLUMNS: &str = "event_id, branch, lifecycle_version, operation, status_after, head, \
     ref_version, source_branch, source_commit, principal_id, principal_type, on_behalf_of, reason, \
     correlation_id, recorded_at::text AS recorded_at";

fn event_from_row(row: &sqlx::postgres::PgRow) -> Result<BranchEvent, LedgerError> {
    let head: String = row.try_get("head").map_err(db_error)?;
    let source_commit: Option<String> = row.try_get("source_commit").map_err(db_error)?;
    Ok(BranchEvent {
        event_id: row.try_get("event_id").map_err(db_error)?,
        branch: row.try_get("branch").map_err(db_error)?,
        lifecycle_version: row.try_get("lifecycle_version").map_err(db_error)?,
        operation: row.try_get("operation").map_err(db_error)?,
        status_after: row.try_get("status_after").map_err(db_error)?,
        head: head.parse()?,
        ref_version: row.try_get("ref_version").map_err(db_error)?,
        source_branch: row.try_get("source_branch").map_err(db_error)?,
        source_commit: source_commit.map(|c| c.parse()).transpose()?,
        principal_id: row.try_get("principal_id").map_err(db_error)?,
        principal_type: row.try_get("principal_type").map_err(db_error)?,
        on_behalf_of: row.try_get("on_behalf_of").map_err(db_error)?,
        reason: row.try_get("reason").map_err(db_error)?,
        correlation_id: row.try_get("correlation_id").map_err(db_error)?,
        recorded_at: row.try_get("recorded_at").map_err(db_error)?,
    })
}

const BRANCH_INFO_SELECT: &str = "SELECT b.graph_id, b.branch, b.status, b.lifecycle_version, b.origin, \
     b.source_branch, b.source_commit, r.head, r.version, r.protected, b.require_validation, \
     b.require_distinct_reviewer, b.created_at::text AS created_at \
     FROM branches b JOIN refs r ON r.graph_id = b.graph_id AND r.branch = b.branch";

fn info_from_row(row: &sqlx::postgres::PgRow) -> Result<BranchInfo, LedgerError> {
    let head: String = row.try_get("head").map_err(db_error)?;
    let source_commit: Option<String> = row.try_get("source_commit").map_err(db_error)?;
    Ok(BranchInfo {
        graph_id: GraphId::new(row.try_get::<String, _>("graph_id").map_err(db_error)?)?,
        name: row.try_get("branch").map_err(db_error)?,
        status: row.try_get("status").map_err(db_error)?,
        lifecycle_version: row.try_get("lifecycle_version").map_err(db_error)?,
        origin: row.try_get("origin").map_err(db_error)?,
        source_branch: row.try_get("source_branch").map_err(db_error)?,
        source_commit: source_commit.map(|c| c.parse()).transpose()?,
        head: head.parse()?,
        version: row.try_get("version").map_err(db_error)?,
        policy: BranchPolicy {
            protected: row.try_get("protected").map_err(db_error)?,
            require_validation: row.try_get("require_validation").map_err(db_error)?,
            require_distinct_reviewer: row
                .try_get("require_distinct_reviewer")
                .map_err(db_error)?,
        },
        created_at: row.try_get("created_at").map_err(db_error)?,
    })
}

/// The branch state an in-transaction workflow step needs, read under a row lock.
pub(crate) struct LockedBranch {
    pub(crate) status: String,
    pub(crate) require_validation: bool,
    pub(crate) require_distinct_reviewer: bool,
}

impl WorkflowRepository {
    /// Lock a branch row (`FOR SHARE` for workflow steps, `FOR UPDATE` for lifecycle
    /// changes). Callers lock the ref first where they lock both (ADR-0022 lock order).
    pub(crate) async fn lock_branch(
        conn: &mut PgConnection,
        graph: &GraphId,
        branch: &str,
        exclusive: bool,
    ) -> Result<Option<LockedBranch>, LedgerError> {
        let sql = if exclusive {
            "SELECT status, require_validation, require_distinct_reviewer FROM branches \
             WHERE graph_id = $1 AND branch = $2 FOR UPDATE"
        } else {
            "SELECT status, require_validation, require_distinct_reviewer FROM branches \
             WHERE graph_id = $1 AND branch = $2 FOR SHARE"
        };
        let row = sqlx::query(sql)
            .bind(graph.as_str())
            .bind(branch)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db_error)?;
        row.map(|r| {
            Ok(LockedBranch {
                status: r.try_get("status").map_err(db_error)?,
                require_validation: r.try_get("require_validation").map_err(db_error)?,
                require_distinct_reviewer: r
                    .try_get("require_distinct_reviewer")
                    .map_err(db_error)?,
            })
        })
        .transpose()
    }

    /// Record `main`'s branch row and its `genesis` lifecycle event inside the genesis
    /// acceptance transaction (only `main` is born by genesis, ADR-0022).
    pub(crate) async fn record_genesis_branch(
        conn: &mut PgConnection,
        scope: &RequestScope,
        branch: &str,
        head: &CommitId,
    ) -> Result<(), LedgerError> {
        sqlx::query(
            "INSERT INTO branches (graph_id, branch, tenant_id, status, lifecycle_version, origin, \
             require_validation, require_distinct_reviewer) \
             VALUES ($1, $2, $3, 'active', 1, 'genesis', false, false)",
        )
        .bind(scope.graph.as_str())
        .bind(branch)
        .bind(scope.principal.tenant_id.as_str())
        .execute(&mut *conn)
        .await
        .map_err(db_error)?;
        Self::insert_branch_event(
            conn, scope, branch, 1, "genesis", "active", head, 1, None, None,
        )
        .await
        .map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_branch_event(
        conn: &mut PgConnection,
        scope: &RequestScope,
        branch: &str,
        lifecycle_version: i64,
        operation: &str,
        status_after: &str,
        head: &CommitId,
        ref_version: i64,
        source: Option<(&str, &CommitId)>,
        reason: Option<&str>,
    ) -> Result<BranchEvent, LedgerError> {
        let actor = scope.principal.actor();
        let row = sqlx::query(&format!(
            "INSERT INTO branch_events (graph_id, branch, tenant_id, lifecycle_version, operation, \
             status_after, head, ref_version, source_branch, source_commit, principal_id, \
             principal_type, on_behalf_of, reason, correlation_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
             RETURNING {BRANCH_EVENT_COLUMNS}"
        ))
        .bind(scope.graph.as_str())
        .bind(branch)
        .bind(scope.principal.tenant_id.as_str())
        .bind(lifecycle_version)
        .bind(operation)
        .bind(status_after)
        .bind(head.to_string())
        .bind(ref_version)
        .bind(source.map(|(b, _)| b.to_owned()))
        .bind(source.map(|(_, c)| c.to_string()))
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(reason)
        .bind(scope.correlation_id.as_deref())
        .fetch_one(&mut *conn)
        .await
        .map_err(db_error)?;
        event_from_row(&row)
    }

    async fn record_branch_result(
        conn: &mut PgConnection,
        scope: &RequestScope,
        operation: Operation,
        result_kind: &str,
        event: &BranchEvent,
    ) -> Result<(), LedgerError> {
        sqlx::query(
            "INSERT INTO idempotency (tenant_id, principal_id, principal_type, on_behalf_of, graph_id, \
             operation, idempotency_key, request_digest, result_kind, result_commit, \
             result_ref_version, result_branch_event_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(scope.principal.tenant_id.as_str())
        .bind(scope.principal.principal_id.as_str())
        .bind(scope.principal.principal_type.as_str())
        .bind(
            scope
                .principal
                .on_behalf_of
                .as_ref()
                .map(|p| p.as_str().to_owned()),
        )
        .bind(scope.graph.as_str())
        .bind(operation.as_str())
        .bind(&scope.idempotency_key)
        .bind(scope.request_digest.to_string())
        .bind(result_kind)
        .bind(event.head.to_string())
        .bind(event.ref_version)
        .bind(event.event_id)
        .execute(&mut *conn)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn replay_branch(
        conn: &mut PgConnection,
        stored: StoredResult,
        scope: &RequestScope,
        result_kind: &str,
    ) -> Result<BranchOutcome, LedgerError> {
        Self::check_digest(&stored, scope)?;
        if stored.result_kind != result_kind {
            return Err(LedgerError::Storage(format!(
                "idempotency row is not a {result_kind} result"
            )));
        }
        let event_id = stored.result_branch_event_id.ok_or_else(|| {
            LedgerError::Storage("branch result without its lifecycle event".into())
        })?;
        let row = sqlx::query(&format!(
            "SELECT {BRANCH_EVENT_COLUMNS} FROM branch_events WHERE event_id = $1 AND graph_id = $2"
        ))
        .bind(event_id)
        .bind(scope.graph.as_str())
        .fetch_one(&mut *conn)
        .await
        .map_err(db_error)?;
        Ok(BranchOutcome {
            event: event_from_row(&row)?,
            replayed: true,
        })
    }

    /// Whether `commit` is the source head or reachable from it, read without locks on a pooled
    /// connection. Returns the head it was decided against. Unknown and foreign commits are
    /// refused without walking (indistinguishable from unreachable ones, ADR-0022).
    async fn reachable_from_source(
        &self,
        scope: &RequestScope,
        request: &CreateBranchRequest,
        commit: &CommitId,
        limits: TraversalLimits,
    ) -> Result<(CommitId, i64, bool), LedgerError> {
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        // Only an active graph of the caller's tenant is walked (nothing is read for, or
        // spent on, another tenant's graph); the transaction reports the precise error.
        let row = sqlx::query(
            "SELECT r.head, r.version FROM refs r JOIN graphs g ON g.graph_id = r.graph_id \
             WHERE r.graph_id = $1 AND r.branch = $2 AND g.tenant_id = $3 AND g.status = 'active'",
        )
        .bind(scope.graph.as_str())
        .bind(&request.source)
        .bind(scope.principal.tenant_id.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Err(LedgerError::BranchNotFound(request.source.clone()));
        };
        let head: CommitId = row
            .try_get::<String, _>("head")
            .map_err(db_error)?
            .parse()?;
        let version: i64 = row.try_get("version").map_err(db_error)?;
        if *commit == head {
            return Ok((head, version, true));
        }
        let known: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM commit_index WHERE id = $1 AND graph_id = $2")
                .bind(commit.to_string())
                .bind(scope.graph.as_str())
                .fetch_optional(&mut *conn)
                .await
                .map_err(db_error)?;
        if known.is_none() {
            return Ok((head, version, false));
        }
        let provider = GraphParents {
            conn: Mutex::new(&mut *conn),
            graph: scope.graph.clone(),
        };
        match ledger_dag::is_ancestor(&provider, commit, &head, limits).await {
            Ok(reachable) => Ok((head, version, reachable)),
            Err(DagError::VisitLimit { visited }) => Err(LedgerError::ResourceLimit(format!(
                "branch point search exceeded {visited} commits"
            ))),
            Err(DagError::Deadline) => Err(LedgerError::ResourceLimit(
                "branch point search exceeded its time limit".into(),
            )),
            // The start commit is indexed, so an unknown commit here is a missing parent.
            Err(DagError::UnknownCommit(c)) => Err(LedgerError::CorruptObject {
                id: c.0,
                reason: "parent commit missing from the graph's index".into(),
            }),
            Err(DagError::Cycle(c)) => Err(LedgerError::CorruptObject {
                id: c.0,
                reason: "commit cycle".into(),
            }),
            Err(DagError::Provider(e)) => Err(e),
        }
    }

    /// Whether `branch` moved from `head_then` (version `from`) to version `to` only through
    /// audited ref events: exactly one event per version in `from+1..=to`, chained, the first
    /// leaving `head_then`. Raw (import) moves write no events, so they fail this.
    async fn moved_by_audited_fast_forwards(
        conn: &mut PgConnection,
        graph: &GraphId,
        branch: &str,
        head_then: &CommitId,
        from: i64,
        to: i64,
    ) -> Result<bool, LedgerError> {
        if to < from {
            return Ok(false);
        }
        let chained: bool = sqlx::query_scalar(
            "SELECT count(*) = $4 - $3 \
                AND bool_and(e.old_head = coalesce( \
                      (SELECT p.new_head FROM ref_events p WHERE p.graph_id = e.graph_id \
                         AND p.branch = e.branch AND p.new_version = e.new_version - 1 \
                         AND e.new_version - 1 > $3), $5)) \
             FROM ref_events e WHERE e.graph_id = $1 AND e.branch = $2 \
               AND e.new_version > $3 AND e.new_version <= $4",
        )
        .bind(graph.as_str())
        .bind(branch)
        .bind(from)
        .bind(to)
        .bind(head_then.to_string())
        .fetch_one(&mut *conn)
        .await
        .map_err(db_error)?;
        Ok(chained)
    }

    /// Create a named branch at the source head or at a commit reachable from it (ADR-0022).
    pub async fn create_branch(
        &self,
        request: &CreateBranchRequest,
        limits: TraversalLimits,
    ) -> Result<BranchOutcome, LedgerError> {
        let scope = &request.scope;
        validate_scope_fn(scope)?;
        validate_branch(&request.name)?;
        validate_branch(&request.source)?;
        if request.name == "main" {
            return Err(LedgerError::BranchPolicyViolation(
                "`main` is created only by the graph's genesis acceptance".into(),
            ));
        }
        // A completed request is replayed before any mutable graph/source/reachability check
        // (ADR-0022): its durable result depends only on the actor scope, graph, operation,
        // key and request digest. This lookup is not the serialization point (no lock is
        // held; the connection is released before walking) — the scoped transaction below
        // checks again under the idempotency lock, for a request completed meanwhile.
        if request.from_commit.is_some() {
            let mut conn = self.pool.acquire().await.map_err(db_error)?;
            if let Some(stored) =
                Self::stored_result(&mut conn, scope, Operation::BranchCreate).await?
            {
                return Self::replay_branch(&mut conn, stored, scope, "branch_created").await;
            }
        }
        // Reachability is decided before the transaction, on one pooled connection released
        // before the transaction begins, so no lock (source ref, branch, graph status) is held
        // while walking (ADR-0022). History only moves forward: a commit reachable from the
        // head read here stays reachable from any later head.
        let reachable = match &request.from_commit {
            None => None,
            Some(commit) => Some(
                self.reachable_from_source(scope, request, commit, limits)
                    .await,
            ),
        };
        let mut tx = Self::begin_scoped(&self.pool, scope, Operation::BranchCreate).await?;
        if let Some(stored) = Self::stored_result(&mut tx, scope, Operation::BranchCreate).await? {
            return Self::replay_branch(&mut tx, stored, scope, "branch_created").await;
        }
        Self::graph_must_be_active(&mut tx, scope).await?;
        // Lock order: ref, then branch (as accept does).
        let source_ref = sqlx::query(
            "SELECT head, version FROM refs WHERE graph_id = $1 AND branch = $2 FOR SHARE",
        )
        .bind(scope.graph.as_str())
        .bind(&request.source)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error)?;
        let source = Self::lock_branch(&mut tx, &scope.graph, &request.source, false).await?;
        let (Some(source_ref), Some(source)) = (source_ref, source) else {
            return Err(LedgerError::BranchNotFound(request.source.clone()));
        };
        if source.status != "active" {
            return Err(LedgerError::BranchDeleted(request.source.clone()));
        }
        let source_head: CommitId = source_ref
            .try_get::<String, _>("head")
            .map_err(db_error)?
            .parse()?;
        let source_version: i64 = source_ref.try_get("version").map_err(db_error)?;
        let point = match &request.from_commit {
            None => source_head.clone(),
            Some(commit) if *commit == source_head => source_head.clone(),
            Some(commit) => match reachable {
                // Decided against the source head of the pre-transaction read. It carries
                // over to the locked head only if every movement since was an audited
                // fast-forward (contiguous ref events from that head; migration 0009 checks
                // each one), so reachability only grew. Anything else — a raw import move
                // while the graph was not active — is refused for a retry, never trusted. A
                // commit that became reachable only after the check is refused as well.
                Some(Ok((head_then, version_then, true))) => {
                    if version_then != source_version
                        && !Self::moved_by_audited_fast_forwards(
                            &mut tx,
                            &scope.graph,
                            &request.source,
                            &head_then,
                            version_then,
                            source_version,
                        )
                        .await?
                    {
                        return Err(LedgerError::BranchPointUnreachable);
                    }
                    commit.clone()
                }
                Some(Ok(_)) => return Err(LedgerError::BranchPointUnreachable),
                // The source exists now but did not (or its graph was not active) when the
                // point was checked: nothing was checked against it; retryable.
                Some(Err(LedgerError::BranchNotFound(_))) => {
                    return Err(LedgerError::BranchPointUnreachable);
                }
                Some(Err(e)) => return Err(e),
                None => return Err(LedgerError::BranchPointUnreachable),
            },
        };
        let inserted = sqlx::query(
            "INSERT INTO refs (graph_id, branch, head, version, protected) VALUES ($1, $2, $3, 1, $4) \
             ON CONFLICT (graph_id, branch) DO NOTHING",
        )
        .bind(scope.graph.as_str())
        .bind(&request.name)
        .bind(point.to_string())
        .bind(request.policy.protected)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        if inserted.rows_affected() != 1 {
            return Err(LedgerError::BranchExists(request.name.clone()));
        }
        let actor = scope.principal.actor();
        sqlx::query(
            "INSERT INTO ref_events (graph_id, branch, old_head, new_head, old_version, new_version, \
             operation, tenant_id, principal_id, principal_type, on_behalf_of, reason, correlation_id) \
             VALUES ($1, $2, NULL, $3, NULL, 1, 'genesis', $4, $5, $6, $7, NULL, $8)",
        )
        .bind(scope.graph.as_str())
        .bind(&request.name)
        .bind(point.to_string())
        .bind(scope.principal.tenant_id.as_str())
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(scope.correlation_id.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        sqlx::query(
            "INSERT INTO branches (graph_id, branch, tenant_id, status, lifecycle_version, origin, \
             source_branch, source_commit, require_validation, require_distinct_reviewer) \
             VALUES ($1, $2, $3, 'active', 1, 'created', $4, $5, $6, $7)",
        )
        .bind(scope.graph.as_str())
        .bind(&request.name)
        .bind(scope.principal.tenant_id.as_str())
        .bind(&request.source)
        .bind(point.to_string())
        .bind(request.policy.require_validation)
        .bind(request.policy.require_distinct_reviewer)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        let event = Self::insert_branch_event(
            &mut tx,
            scope,
            &request.name,
            1,
            "created",
            "active",
            &point,
            1,
            Some((&request.source, &point)),
            None,
        )
        .await?;
        Self::record_branch_result(
            &mut tx,
            scope,
            Operation::BranchCreate,
            "branch_created",
            &event,
        )
        .await?;
        tx.commit().await.map_err(db_error)?;
        Ok(BranchOutcome {
            event,
            replayed: false,
        })
    }

    /// Delete (tombstone) an active branch; `main` can never be deleted (ADR-0022).
    pub async fn delete_branch(
        &self,
        request: &BranchLifecycleRequest,
    ) -> Result<BranchOutcome, LedgerError> {
        self.change_branch_status(
            request,
            Operation::BranchDelete,
            "active",
            "deleted",
            "deleted",
        )
        .await
    }

    /// Restore a deleted branch with the same head and version.
    pub async fn restore_branch(
        &self,
        request: &BranchLifecycleRequest,
    ) -> Result<BranchOutcome, LedgerError> {
        self.change_branch_status(
            request,
            Operation::BranchRestore,
            "deleted",
            "active",
            "restored",
        )
        .await
    }

    async fn change_branch_status(
        &self,
        request: &BranchLifecycleRequest,
        operation: Operation,
        from: &str,
        to: &str,
        event_operation: &str,
    ) -> Result<BranchOutcome, LedgerError> {
        let scope = &request.scope;
        validate_scope_fn(scope)?;
        validate_branch(&request.name)?;
        validate_reason(request.reason.as_deref())?;
        let result_kind = match operation {
            Operation::BranchDelete => "branch_deleted",
            _ => "branch_restored",
        };
        let mut tx = Self::begin_scoped(&self.pool, scope, operation).await?;
        if let Some(stored) = Self::stored_result(&mut tx, scope, operation).await? {
            return Self::replay_branch(&mut tx, stored, scope, result_kind).await;
        }
        Self::graph_must_be_active(&mut tx, scope).await?;
        if request.name == "main" {
            return Err(LedgerError::BranchPolicyViolation(
                "`main` is never deleted or restored".into(),
            ));
        }
        // Branch row only (never the ref): an acceptance holding the ref lock waits here or
        // sees the new status (ADR-0022 lock order).
        let row = sqlx::query(
            "SELECT b.status, b.lifecycle_version FROM branches b \
             WHERE b.graph_id = $1 AND b.branch = $2 FOR UPDATE",
        )
        .bind(scope.graph.as_str())
        .bind(&request.name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Err(LedgerError::BranchNotFound(request.name.clone()));
        };
        let status: String = row.try_get("status").map_err(db_error)?;
        let lifecycle_version: i64 = row.try_get("lifecycle_version").map_err(db_error)?;
        if status != from {
            return Err(LedgerError::BranchStateConflict(format!(
                "the branch is {status}; {event_operation} applies to a {from} branch"
            )));
        }
        let head_row =
            sqlx::query("SELECT head, version FROM refs WHERE graph_id = $1 AND branch = $2")
                .bind(scope.graph.as_str())
                .bind(&request.name)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_error)?;
        let head: CommitId = head_row
            .try_get::<String, _>("head")
            .map_err(db_error)?
            .parse()?;
        let version: i64 = head_row.try_get("version").map_err(db_error)?;
        sqlx::query(
            "UPDATE branches SET status = $3, lifecycle_version = lifecycle_version + 1, \
             updated_at = now() WHERE graph_id = $1 AND branch = $2",
        )
        .bind(scope.graph.as_str())
        .bind(&request.name)
        .bind(to)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        let event = Self::insert_branch_event(
            &mut tx,
            scope,
            &request.name,
            lifecycle_version + 1,
            event_operation,
            to,
            &head,
            version,
            None,
            request.reason.as_deref(),
        )
        .await?;
        Self::record_branch_result(&mut tx, scope, operation, result_kind, &event).await?;
        tx.commit().await.map_err(db_error)?;
        Ok(BranchOutcome {
            event,
            replayed: false,
        })
    }

    /// The graph must belong to the reading tenant (foreign and missing graphs are
    /// indistinguishable).
    pub(crate) async fn readable_graph(
        &self,
        tenant: &TenantId,
        graph: &GraphId,
    ) -> Result<(), LedgerError> {
        let owner: Option<String> =
            sqlx::query_scalar("SELECT tenant_id FROM graphs WHERE graph_id = $1")
                .bind(graph.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(db_error)?;
        if owner.as_deref() != Some(tenant.as_str()) {
            return Err(LedgerError::UnknownGraph(graph.to_string()));
        }
        Ok(())
    }

    /// One branch's current state (tenant-scoped).
    pub async fn branch(
        &self,
        tenant: &TenantId,
        graph: &GraphId,
        name: &str,
    ) -> Result<Option<BranchInfo>, LedgerError> {
        validate_branch(name)?;
        self.readable_graph(tenant, graph).await?;
        let row = sqlx::query(&format!(
            "{BRANCH_INFO_SELECT} WHERE b.graph_id = $1 AND b.branch = $2"
        ))
        .bind(graph.as_str())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        row.map(|r| info_from_row(&r)).transpose()
    }

    /// Every branch of a graph (tenant-scoped), ordered by name, at most `limit`.
    pub async fn branches(
        &self,
        tenant: &TenantId,
        graph: &GraphId,
        limit: i64,
    ) -> Result<Vec<BranchInfo>, LedgerError> {
        self.readable_graph(tenant, graph).await?;
        let rows = sqlx::query(&format!(
            "{BRANCH_INFO_SELECT} WHERE b.graph_id = $1 ORDER BY b.branch LIMIT $2"
        ))
        .bind(graph.as_str())
        .bind(limit.clamp(1, 10_000))
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.iter().map(info_from_row).collect()
    }

    /// A branch's latest `limit` lifecycle events (oldest first among them) and its latest
    /// `limit` head movements (newest first). `None` if the branch does not exist.
    pub async fn branch_history(
        &self,
        tenant: &TenantId,
        graph: &GraphId,
        name: &str,
        limit: i64,
    ) -> Result<Option<(Vec<BranchEvent>, Vec<RefMovement>)>, LedgerError> {
        validate_branch(name)?;
        self.readable_graph(tenant, graph).await?;
        let limit = limit.clamp(1, 10_000);
        let events = sqlx::query(&format!(
            "SELECT * FROM (SELECT {BRANCH_EVENT_COLUMNS}, lifecycle_version AS lv FROM branch_events \
             WHERE graph_id = $1 AND branch = $2 ORDER BY lifecycle_version DESC LIMIT $3) e ORDER BY lv"
        ))
        .bind(graph.as_str())
        .bind(name)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        if events.is_empty() {
            return Ok(None);
        }
        let movements = sqlx::query(
            "SELECT event_id, operation, old_head, new_head, old_version, new_version, principal_id, \
             principal_type, on_behalf_of, reason, recorded_at::text AS recorded_at FROM ref_events \
             WHERE graph_id = $1 AND branch = $2 ORDER BY new_version DESC LIMIT $3",
        )
        .bind(graph.as_str())
        .bind(name)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        let movements = movements
            .iter()
            .map(|r| {
                let old: Option<String> = r.try_get("old_head").map_err(db_error)?;
                let new: String = r.try_get("new_head").map_err(db_error)?;
                Ok(RefMovement {
                    event_id: r.try_get("event_id").map_err(db_error)?,
                    operation: r.try_get("operation").map_err(db_error)?,
                    old_head: old.map(|c| c.parse()).transpose()?,
                    new_head: new.parse()?,
                    old_version: r.try_get("old_version").map_err(db_error)?,
                    new_version: r.try_get("new_version").map_err(db_error)?,
                    principal_id: r.try_get("principal_id").map_err(db_error)?,
                    principal_type: r.try_get("principal_type").map_err(db_error)?,
                    on_behalf_of: r.try_get("on_behalf_of").map_err(db_error)?,
                    reason: r.try_get("reason").map_err(db_error)?,
                    recorded_at: r.try_get("recorded_at").map_err(db_error)?,
                })
            })
            .collect::<Result<Vec<_>, LedgerError>>()?;
        Ok(Some((
            events
                .iter()
                .map(event_from_row)
                .collect::<Result<_, _>>()?,
            movements,
        )))
    }

    /// Bounded first-parent commit history from `head` within the graph (tenant-scoped).
    pub async fn first_parent_history(
        &self,
        tenant: &TenantId,
        graph: &GraphId,
        head: &CommitId,
        max: usize,
        limits: TraversalLimits,
    ) -> Result<Vec<CommitId>, LedgerError> {
        self.readable_graph(tenant, graph).await?;
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        let provider = GraphParents {
            conn: Mutex::new(&mut *conn),
            graph: graph.clone(),
        };
        ledger_dag::first_parent_history(&provider, head, max, limits)
            .await
            .map_err(|e| match e {
                DagError::UnknownCommit(c) => LedgerError::NotFound(c.0),
                DagError::VisitLimit { visited } => {
                    LedgerError::ResourceLimit(format!("history exceeded {visited} commits"))
                }
                DagError::Deadline => {
                    LedgerError::ResourceLimit("history exceeded its time limit".into())
                }
                DagError::Cycle(c) => LedgerError::CorruptObject {
                    id: c.0,
                    reason: "commit cycle".into(),
                },
                DagError::Provider(e) => e,
            })
    }
}
