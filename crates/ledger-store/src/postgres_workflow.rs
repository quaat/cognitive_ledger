//! Atomic workflow persistence (ADR-0013, Plan 0004 P1.3).
//!
//! `WorkflowRepository` turns "durable candidate + plain CAS HEAD" into one PostgreSQL
//! transaction per operation: idempotency, tenant/graph lifecycle, ref CAS with lineage
//! enforcement against the verified commit index, ref event, decision, projection outbox
//! and the idempotency result all commit together or not at all. The immutable candidate
//! is published by the same in-transaction helper `PostgresImmutableStore` uses, so a
//! prepare that crashes leaves no candidate and a retried prepare replays the exact
//! original `CommitId`.
//!
//! Idempotency: every workflow transaction first takes a transaction-scoped advisory lock
//! on its idempotency scope `(tenant, principal, graph, operation, key)`, then reads the
//! stored result. Identical concurrent requests therefore serialize deterministically: the
//! second one waits for the first to commit, sees its completed result and replays it (a
//! different digest is `IDEMPOTENCY_CONFLICT`). The result row is inserted inside the
//! transaction, so a visible row is always a completed result, and a rolled-back winner
//! leaves nothing behind. Nothing process-local is used for correctness.
//!
//! Trust boundary: `RequestScope.principal` is the authenticated principal (ADR-0011) and
//! `request_digest` is the API layer's digest of the canonical request; the repository
//! verifies graph ownership against the principal's tenant but cannot recompute the
//! digest. P1.3 acceptance runs under an explicit `ValidationPolicy::NoValidation`:
//! atomicity is verified, production protected semantic acceptance (Phase 2) is not
//! enabled, and no validation record is ever fabricated.

use crate::{PgGraphs, PostgresImmutableStore, V1Binding, storage};
use ledger_core::{
    AnyCommit, AuthenticatedPrincipal, CommitId, CommitV2, ContentId, GraphId, LedgerError,
    LedgerTimestamp, PatchId,
};
use ledger_rdf::{DeltaPolicy, Patch, apply_patch, effective_delta};
use sqlx::{PgConnection, PgPool, Row, postgres::PgPoolOptions};
use std::collections::BTreeSet;
use time::OffsetDateTime;

/// Bounds enforced before any work: they are also CHECK constraints in migration 0006,
/// but a typed refusal up front beats a rolled-back transaction and a 500.
pub const MAX_BRANCH_BYTES: usize = 128;
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
pub const MAX_REASON_BYTES: usize = 4096;

/// Who is asking, for which graph, and how the request is deduplicated. `tenant` is part
/// of the idempotency scope through the principal (ADR-0011).
#[derive(Clone, Debug)]
pub struct RequestScope {
    pub principal: AuthenticatedPrincipal,
    pub graph: GraphId,
    pub idempotency_key: String,
    /// Digest of the canonical request as the caller submitted it (computed by the API
    /// layer); two requests with the same key must carry the same digest.
    pub request_digest: ContentId,
}

#[derive(Clone, Debug)]
pub struct PrepareRequest {
    pub scope: RequestScope,
    pub branch: String,
    pub expected_head: Option<CommitId>,
    pub requested: Patch,
    pub activity: String,
    pub event_time: Option<LedgerTimestamp>,
    pub evidence_refs: Vec<String>,
    pub source_system: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Prepared {
    pub proposal_id: i64,
    pub candidate: CommitId,
    pub requested_patch: PatchId,
    pub effective_patch: PatchId,
    /// True when an earlier request with the same scope/key/digest produced this result.
    pub replayed: bool,
}

/// P1.3 has no semantic validation. The only policy is explicit about that; Phase 2 adds
/// policies that require immutable `ValidationRecord`s and fills `validation_ids`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationPolicy {
    NoValidation,
}

#[derive(Clone, Debug)]
pub struct AcceptRequest {
    pub scope: RequestScope,
    pub branch: String,
    pub expected_head: Option<CommitId>,
    pub candidate: CommitId,
    pub reason: Option<String>,
    pub validation: ValidationPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Accepted {
    pub decision_id: i64,
    pub ref_event_id: i64,
    pub outbox_id: i64,
    pub ref_version: i64,
    pub head: CommitId,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub struct RejectRequest {
    pub scope: RequestScope,
    pub branch: String,
    pub candidate: CommitId,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rejected {
    pub decision_id: i64,
    pub replayed: bool,
}

/// Deterministic fault injection: abort the transaction at a named point. Test-only in
/// intent; production constructs the repository without one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailPoint {
    AfterIdempotencyCheck,
    AfterLineageValidation,
    AfterRefUpdate,
    AfterRefEvent,
    /// After the decision row (accept/reject) or the proposal row (prepare).
    AfterDecision,
    AfterOutbox,
    BeforeCommit,
}

#[derive(Clone, Debug)]
pub struct WorkflowRepository {
    pool: PgPool,
    immutable: PostgresImmutableStore,
    failpoint: Option<FailPoint>,
}

/// Composition root: one pool shared by the immutable store, the graph authority and the
/// workflow repository, so atomic workflow code can share a transaction with the
/// publication helpers instead of every component owning an opaque pool.
#[derive(Clone, Debug)]
pub struct PostgresLedgerStore {
    pool: PgPool,
    immutable: PostgresImmutableStore,
    graphs: PgGraphs,
    workflows: WorkflowRepository,
}

impl PostgresLedgerStore {
    /// Connect, run all migrations, and compose the components over one pool.
    pub async fn connect(database_url: &str, v1_binding: V1Binding) -> Result<Self, LedgerError> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(storage)?;
        crate::schema::migrate_all(&pool).await?;
        Ok(Self::from_pool_migrated(pool, v1_binding))
    }

    pub fn from_pool_migrated(pool: PgPool, v1_binding: V1Binding) -> Self {
        let immutable = PostgresImmutableStore::from_pool_migrated(pool.clone(), v1_binding);
        Self {
            graphs: PgGraphs::new(pool.clone()),
            workflows: WorkflowRepository::new(pool.clone(), immutable.clone()),
            immutable,
            pool,
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
    pub fn immutable(&self) -> &PostgresImmutableStore {
        &self.immutable
    }
    pub fn graphs(&self) -> &PgGraphs {
        &self.graphs
    }
    pub fn workflows(&self) -> &WorkflowRepository {
        &self.workflows
    }
}

/// What the idempotency table remembers about a completed request.
struct StoredResult {
    request_digest: String,
    result_kind: String,
    result_commit: Option<String>,
    result_ref_version: Option<i64>,
    result_decision_id: Option<i64>,
    result_proposal_id: Option<i64>,
}

#[derive(Clone, Copy)]
enum Operation {
    Prepare,
    Accept,
    Reject,
}

impl Operation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Accept => "accept",
            Self::Reject => "reject",
        }
    }
}

/// A proposal row as accept/reject need it: the ref it was prepared for.
struct ProposalBinding {
    proposal_id: i64,
    graph_id: String,
    branch: String,
    expected_head: Option<String>,
}

fn now() -> Result<LedgerTimestamp, LedgerError> {
    LedgerTimestamp::try_from_offset_date_time(OffsetDateTime::now_utc())
}

fn validate_branch(branch: &str) -> Result<(), LedgerError> {
    if branch.is_empty() || branch.len() > MAX_BRANCH_BYTES {
        return Err(LedgerError::InvalidIdentifier {
            field: "branch",
            reason: format!("must be 1..={MAX_BRANCH_BYTES} bytes"),
        });
    }
    if !branch
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
    {
        return Err(LedgerError::InvalidIdentifier {
            field: "branch",
            reason: "must match [A-Za-z0-9._/-]".into(),
        });
    }
    Ok(())
}

fn validate_scope(scope: &RequestScope) -> Result<(), LedgerError> {
    let key = &scope.idempotency_key;
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES || key.chars().any(char::is_control)
    {
        return Err(LedgerError::InvalidIdentifier {
            field: "idempotency_key",
            reason: format!(
                "must be 1..={MAX_IDEMPOTENCY_KEY_BYTES} bytes without control characters"
            ),
        });
    }
    Ok(())
}

fn validate_reason(reason: Option<&str>) -> Result<(), LedgerError> {
    if let Some(reason) = reason
        && reason.len() > MAX_REASON_BYTES
    {
        return Err(LedgerError::InvalidIdentifier {
            field: "reason",
            reason: format!("exceeds {MAX_REASON_BYTES} bytes"),
        });
    }
    Ok(())
}

/// A unique-violation on a decision index means the candidate/proposal was decided
/// concurrently: a lineage conflict, not storage corruption.
fn map_decision_insert(error: sqlx::Error, candidate: &CommitId) -> LedgerError {
    match &error {
        sqlx::Error::Database(e) if e.is_unique_violation() => LedgerError::LineageMismatch(
            format!("candidate {candidate} already has a terminal decision"),
        ),
        _ => storage(error),
    }
}

impl WorkflowRepository {
    pub fn new(pool: PgPool, immutable: PostgresImmutableStore) -> Self {
        Self {
            pool,
            immutable,
            failpoint: None,
        }
    }

    /// Abort every operation's transaction at `point` (tests only).
    pub fn with_failpoint(mut self, point: FailPoint) -> Self {
        self.failpoint = Some(point);
        self
    }

    fn fail_at(&self, point: FailPoint) -> Result<(), LedgerError> {
        if self.failpoint == Some(point) {
            return Err(LedgerError::Storage(format!(
                "injected failure at {point:?}"
            )));
        }
        Ok(())
    }

    /// Open the workflow transaction: READ COMMITTED pinned, then the per-scope advisory
    /// lock so identical concurrent requests serialize before anything is read.
    async fn begin(
        &self,
        scope: &RequestScope,
        operation: Operation,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, LedgerError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        let lock_key = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            scope.principal.tenant_id.as_str(),
            scope.principal.principal_id.as_str(),
            scope.graph.as_str(),
            operation.as_str(),
            scope.idempotency_key
        );
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(lock_key)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        Ok(tx)
    }

    async fn stored_result(
        conn: &mut PgConnection,
        scope: &RequestScope,
        operation: Operation,
    ) -> Result<Option<StoredResult>, LedgerError> {
        let row = sqlx::query(
            "SELECT request_digest, result_kind, result_commit, result_ref_version, \
             result_decision_id, result_proposal_id FROM idempotency \
             WHERE tenant_id = $1 AND principal_id = $2 AND graph_id = $3 AND operation = $4 \
             AND idempotency_key = $5",
        )
        .bind(scope.principal.tenant_id.as_str())
        .bind(scope.principal.principal_id.as_str())
        .bind(scope.graph.as_str())
        .bind(operation.as_str())
        .bind(&scope.idempotency_key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
        row.map(|row| {
            Ok(StoredResult {
                request_digest: row.try_get("request_digest").map_err(storage)?,
                result_kind: row.try_get("result_kind").map_err(storage)?,
                result_commit: row.try_get("result_commit").map_err(storage)?,
                result_ref_version: row.try_get("result_ref_version").map_err(storage)?,
                result_decision_id: row.try_get("result_decision_id").map_err(storage)?,
                result_proposal_id: row.try_get("result_proposal_id").map_err(storage)?,
            })
        })
        .transpose()
    }

    /// Insert the completed result; the advisory lock guarantees this transaction owns the
    /// key, so a conflict here is an invariant violation rather than a race.
    #[allow(clippy::too_many_arguments)]
    async fn record_result(
        conn: &mut PgConnection,
        scope: &RequestScope,
        operation: Operation,
        result_kind: &str,
        result_commit: Option<&CommitId>,
        result_ref_version: Option<i64>,
        result_decision_id: Option<i64>,
        result_proposal_id: Option<i64>,
    ) -> Result<(), LedgerError> {
        sqlx::query(
            "INSERT INTO idempotency (tenant_id, principal_id, graph_id, operation, idempotency_key, \
             request_digest, result_kind, result_commit, result_ref_version, result_decision_id, \
             result_proposal_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(scope.principal.tenant_id.as_str())
        .bind(scope.principal.principal_id.as_str())
        .bind(scope.graph.as_str())
        .bind(operation.as_str())
        .bind(&scope.idempotency_key)
        .bind(scope.request_digest.to_string())
        .bind(result_kind)
        .bind(result_commit.map(ToString::to_string))
        .bind(result_ref_version)
        .bind(result_decision_id)
        .bind(result_proposal_id)
        .execute(&mut *conn)
        .await
        .map_err(storage)?;
        Ok(())
    }

    fn check_digest(stored: &StoredResult, scope: &RequestScope) -> Result<(), LedgerError> {
        if stored.request_digest != scope.request_digest.to_string() {
            return Err(LedgerError::IdempotencyConflict);
        }
        Ok(())
    }

    /// The graph must exist, belong to the caller's tenant (a foreign or missing graph is
    /// reported identically as `UnknownGraph`, so nothing about other tenants leaks), and
    /// be `active` for normal workflow operations.
    async fn graph_must_be_active(
        conn: &mut PgConnection,
        scope: &RequestScope,
    ) -> Result<(), LedgerError> {
        // FOR SHARE: a lifecycle transition committed concurrently cannot slip between
        // this check and the workflow's writes.
        let row = sqlx::query("SELECT tenant_id, status FROM graphs WHERE graph_id = $1 FOR SHARE")
            .bind(scope.graph.as_str())
            .fetch_optional(&mut *conn)
            .await
            .map_err(storage)?;
        let Some(row) = row else {
            return Err(LedgerError::UnknownGraph(scope.graph.to_string()));
        };
        let tenant: String = row.try_get("tenant_id").map_err(storage)?;
        if tenant != scope.principal.tenant_id.as_str() {
            return Err(LedgerError::UnknownGraph(scope.graph.to_string()));
        }
        let status: String = row.try_get("status").map_err(storage)?;
        if status != "active" {
            return Err(LedgerError::GraphNotActive {
                graph: scope.graph.to_string(),
                status,
            });
        }
        Ok(())
    }

    /// The proposal a candidate came from. Accept and reject require one: a commit that
    /// never went through `prepare` (and therefore through effective-delta reduction) is
    /// not a workflow candidate.
    async fn proposal_for(
        conn: &mut PgConnection,
        candidate: &CommitId,
    ) -> Result<Option<ProposalBinding>, LedgerError> {
        let row = sqlx::query(
            "SELECT proposal_id, graph_id, branch, expected_head FROM proposals \
             WHERE candidate_commit = $1",
        )
        .bind(candidate.to_string())
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
        row.map(|row| {
            Ok(ProposalBinding {
                proposal_id: row.try_get("proposal_id").map_err(storage)?,
                graph_id: row.try_get("graph_id").map_err(storage)?,
                branch: row.try_get("branch").map_err(storage)?,
                expected_head: row.try_get("expected_head").map_err(storage)?,
            })
        })
        .transpose()
    }

    /// The candidate must be an indexed commit of the caller's graph with a proposal bound
    /// to exactly this ref and expected head, and carry no terminal decision yet.
    async fn bound_undecided_proposal(
        conn: &mut PgConnection,
        scope: &RequestScope,
        branch: &str,
        expected_head: Option<&CommitId>,
        candidate: &CommitId,
    ) -> Result<ProposalBinding, LedgerError> {
        let indexed_graph: Option<String> =
            sqlx::query("SELECT graph_id FROM commit_index WHERE id = $1")
                .bind(candidate.to_string())
                .fetch_optional(&mut *conn)
                .await
                .map_err(storage)?
                .map(|row| row.try_get("graph_id").map_err(storage))
                .transpose()?;
        match indexed_graph.as_deref() {
            Some(g) if g == scope.graph.as_str() => {}
            Some(_) => {
                return Err(LedgerError::LineageMismatch(format!(
                    "candidate {candidate} belongs to another graph"
                )));
            }
            None => {
                return Err(LedgerError::LineageMismatch(format!(
                    "candidate {candidate} is not an indexed commit"
                )));
            }
        }
        let Some(proposal) = Self::proposal_for(conn, candidate).await? else {
            return Err(LedgerError::LineageMismatch(format!(
                "candidate {candidate} has no proposal; only prepared candidates are decided"
            )));
        };
        // A decided candidate is refused for that reason first: it is the more specific
        // fact, and it holds regardless of which ref the caller names.
        let decided =
            sqlx::query("SELECT decision FROM decisions WHERE candidate_commit = $1 LIMIT 1")
                .bind(candidate.to_string())
                .fetch_optional(&mut *conn)
                .await
                .map_err(storage)?;
        if let Some(row) = decided {
            let decision: String = row.try_get("decision").map_err(storage)?;
            return Err(LedgerError::LineageMismatch(format!(
                "candidate {candidate} already has a terminal decision ({decision})"
            )));
        }
        if proposal.graph_id != scope.graph.as_str()
            || proposal.branch != branch
            || proposal.expected_head.as_deref()
                != expected_head.map(ToString::to_string).as_deref()
        {
            return Err(LedgerError::LineageMismatch(format!(
                "candidate {candidate} was prepared for a different ref or expected head"
            )));
        }
        Ok(proposal)
    }

    /// Read the current head and version of a ref, optionally locking the row.
    async fn read_ref(
        conn: &mut PgConnection,
        graph: &GraphId,
        branch: &str,
        lock: bool,
    ) -> Result<Option<(CommitId, i64, bool)>, LedgerError> {
        let sql = if lock {
            "SELECT head, version, protected FROM refs WHERE graph_id = $1 AND branch = $2 FOR UPDATE"
        } else {
            "SELECT head, version, protected FROM refs WHERE graph_id = $1 AND branch = $2"
        };
        let row = sqlx::query(sql)
            .bind(graph.as_str())
            .bind(branch)
            .fetch_optional(&mut *conn)
            .await
            .map_err(storage)?;
        row.map(|row| {
            let head: String = row.try_get("head").map_err(storage)?;
            Ok((
                head.parse()?,
                row.try_get("version").map_err(storage)?,
                row.try_get("protected").map_err(storage)?,
            ))
        })
        .transpose()
    }

    /// Reconstruct the state at `head` on the caller's connection (the workflow
    /// transaction), so a prepare never holds one pool connection while waiting for
    /// another. Bytes are digest-verified exactly as `PostgresImmutableStore` does.
    async fn state_at_on(
        conn: &mut PgConnection,
        head: &CommitId,
    ) -> Result<BTreeSet<ledger_rdf::Quad>, LedgerError> {
        async fn object(
            conn: &mut PgConnection,
            id: &ContentId,
        ) -> Result<Option<Vec<u8>>, LedgerError> {
            let row = sqlx::query("SELECT bytes FROM immutable_objects WHERE id = $1")
                .bind(id.to_string())
                .fetch_optional(&mut *conn)
                .await
                .map_err(storage)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let bytes: Vec<u8> = row.try_get("bytes").map_err(storage)?;
            if &ContentId::for_bytes(&bytes) != id {
                return Err(LedgerError::CorruptObject {
                    id: id.clone(),
                    reason: "stored bytes do not hash to id".into(),
                });
            }
            Ok(Some(bytes))
        }
        let mut chain = Vec::new();
        let mut cursor = Some(head.clone());
        let mut seen = std::collections::HashSet::new();
        while let Some(current) = cursor {
            if !seen.insert(current.clone()) {
                return Err(LedgerError::CorruptObject {
                    id: current.0,
                    reason: "commit cycle".into(),
                });
            }
            let bytes = object(conn, &current.0)
                .await?
                .ok_or_else(|| LedgerError::NotFound(current.0.clone()))?;
            let commit = crate::decode_commit_object(&current.0, &bytes)?
                .ok_or_else(|| LedgerError::NotFound(current.0.clone()))?;
            cursor = commit.parents().first().cloned();
            chain.push(commit);
        }
        let mut state = BTreeSet::new();
        for commit in chain.iter().rev() {
            let bytes = object(conn, &commit.patch().0)
                .await?
                .ok_or_else(|| LedgerError::NotFound(commit.patch().0.clone()))?;
            let patch = crate::validate_patch_bytes(commit.patch(), &bytes)?;
            apply_patch(&mut state, &patch);
        }
        Ok(state)
    }

    /// Prepare a candidate: resolve the base, reduce the request to its effective delta,
    /// publish the requested patch (audit), the effective patch, and the v2 candidate, and
    /// record the proposal — all atomically with the idempotency result.
    pub async fn prepare(&self, request: &PrepareRequest) -> Result<Prepared, LedgerError> {
        let scope = &request.scope;
        validate_scope(scope)?;
        validate_branch(&request.branch)?;
        let mut tx = self.begin(scope, Operation::Prepare).await?;
        if let Some(stored) = Self::stored_result(&mut tx, scope, Operation::Prepare).await? {
            return Self::replay_prepared(&mut tx, stored, scope).await;
        }
        self.fail_at(FailPoint::AfterIdempotencyCheck)?;
        Self::graph_must_be_active(&mut tx, scope).await?;
        // Prepare never moves the ref, so it reads without locking; a stale head is
        // caught here and, if the ref moves later, again at accept.
        let current = Self::read_ref(&mut tx, &scope.graph, &request.branch, false).await?;
        let (current_head, protected) = match &current {
            Some((head, _, protected)) => (Some(head.clone()), *protected),
            None => (None, true),
        };
        if current_head != request.expected_head {
            return Err(LedgerError::HeadChanged {
                expected: request.expected_head.clone(),
                actual: current_head,
            });
        }
        // Base state is immutable content, read on this transaction's connection.
        let base = match &request.expected_head {
            Some(head) => Self::state_at_on(&mut tx, head).await?,
            None => BTreeSet::new(),
        };
        let policy = if protected {
            DeltaPolicy::Strict
        } else {
            DeltaPolicy::Permissive
        };
        let effective =
            effective_delta(&base, &request.requested, policy).map_err(|e| match e {
                ledger_rdf::DeltaError::BaseMismatch(quad) => {
                    LedgerError::BaseMismatch(quad.to_string())
                }
                ledger_rdf::DeltaError::NoEffectiveChange => LedgerError::NoEffectiveChange,
            })?;
        self.fail_at(FailPoint::AfterLineageValidation)?;

        // Content: the requested patch is kept for audit; the effective patch is what the
        // candidate commits to (ADR-0008).
        let requested_id = request.requested.id();
        let effective_id = effective.id();
        crate::postgres_immutable::publish_object(
            &mut tx,
            &requested_id.0,
            &request.requested.canonical_bytes(),
        )
        .await?;
        crate::postgres_immutable::publish_object(
            &mut tx,
            &effective_id.0,
            &effective.canonical_bytes(),
        )
        .await?;
        let candidate = AnyCommit::V2(CommitV2 {
            graph_id: scope.graph.clone(),
            parents: request.expected_head.iter().cloned().collect(),
            patch: effective_id.clone(),
            actor: scope.principal.actor(),
            activity: request.activity.clone(),
            event_time: request.event_time,
            recorded_at: now()?,
            evidence_refs: request.evidence_refs.clone(),
            source_system: request.source_system.clone(),
            message: request.message.clone(),
        });
        // The effective patch is a canonical `Patch` by construction.
        let candidate_id = self
            .immutable
            .publish_commit_in(&mut tx, &candidate)
            .await?;
        let actor = scope.principal.actor();
        let row = sqlx::query(
            "INSERT INTO proposals (graph_id, branch, tenant_id, principal_id, principal_type, \
             on_behalf_of, expected_head, requested_patch_id, effective_patch_id, candidate_commit) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING proposal_id",
        )
        .bind(scope.graph.as_str())
        .bind(&request.branch)
        .bind(scope.principal.tenant_id.as_str())
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(request.expected_head.as_ref().map(ToString::to_string))
        .bind(requested_id.to_string())
        .bind(effective_id.to_string())
        .bind(candidate_id.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match &e {
            // Identical content, actor and microsecond: the same candidate id already has a
            // proposal (a different key). Report it as a lineage conflict, not corruption.
            sqlx::Error::Database(d) if d.is_unique_violation() => LedgerError::LineageMismatch(
                format!("candidate {candidate_id} already has a proposal"),
            ),
            _ => storage(e),
        })?;
        let proposal_id: i64 = row.try_get("proposal_id").map_err(storage)?;
        self.fail_at(FailPoint::AfterDecision)?;
        Self::record_result(
            &mut tx,
            scope,
            Operation::Prepare,
            "prepared",
            Some(&candidate_id),
            None,
            None,
            Some(proposal_id),
        )
        .await?;
        self.fail_at(FailPoint::BeforeCommit)?;
        tx.commit().await.map_err(storage)?;
        Ok(Prepared {
            proposal_id,
            candidate: candidate_id,
            requested_patch: requested_id,
            effective_patch: effective_id,
            replayed: false,
        })
    }

    async fn replay_prepared(
        conn: &mut PgConnection,
        stored: StoredResult,
        scope: &RequestScope,
    ) -> Result<Prepared, LedgerError> {
        Self::check_digest(&stored, scope)?;
        if stored.result_kind != "prepared" {
            return Err(LedgerError::Storage(
                "idempotency row is not a prepare".into(),
            ));
        }
        let candidate: CommitId = stored
            .result_commit
            .as_deref()
            .ok_or_else(|| LedgerError::Storage("prepared result without candidate".into()))?
            .parse()?;
        let proposal_id = stored
            .result_proposal_id
            .ok_or_else(|| LedgerError::Storage("prepared result without proposal".into()))?;
        let row = sqlx::query(
            "SELECT requested_patch_id, effective_patch_id FROM proposals WHERE proposal_id = $1",
        )
        .bind(proposal_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(storage)?;
        let requested: String = row.try_get("requested_patch_id").map_err(storage)?;
        let effective: String = row.try_get("effective_patch_id").map_err(storage)?;
        Ok(Prepared {
            proposal_id,
            candidate,
            requested_patch: PatchId(requested.parse()?),
            effective_patch: PatchId(effective.parse()?),
            replayed: true,
        })
    }

    /// Accept a prepared candidate onto a ref: ONE transaction with idempotency, tenant and
    /// graph lifecycle, proposal binding, ref lock + lineage against the verified index,
    /// ref advance with version bump, ref event, accepted decision, projection outbox row
    /// and the idempotency result.
    pub async fn accept(&self, request: &AcceptRequest) -> Result<Accepted, LedgerError> {
        let scope = &request.scope;
        validate_scope(scope)?;
        validate_branch(&request.branch)?;
        validate_reason(request.reason.as_deref())?;
        let ValidationPolicy::NoValidation = request.validation;
        let mut tx = self.begin(scope, Operation::Accept).await?;
        if let Some(stored) = Self::stored_result(&mut tx, scope, Operation::Accept).await? {
            return Self::replay_accepted(&mut tx, stored, scope).await;
        }
        self.fail_at(FailPoint::AfterIdempotencyCheck)?;
        Self::graph_must_be_active(&mut tx, scope).await?;

        // Lock the ref row (if it exists) so competing advances serialize on it.
        let current = Self::read_ref(&mut tx, &scope.graph, &request.branch, true).await?;
        let current_head = current.as_ref().map(|(head, _, _)| head.clone());
        if current_head != request.expected_head {
            return Err(LedgerError::HeadChanged {
                expected: request.expected_head.clone(),
                actual: current_head,
            });
        }
        let proposal = Self::bound_undecided_proposal(
            &mut tx,
            scope,
            &request.branch,
            request.expected_head.as_ref(),
            &request.candidate,
        )
        .await?;

        // Lineage from the verified index.
        let candidate_row = sqlx::query("SELECT parent_count FROM commit_index WHERE id = $1")
            .bind(request.candidate.to_string())
            .fetch_one(&mut *tx)
            .await
            .map_err(storage)?;
        let parent_count: i16 = candidate_row.try_get("parent_count").map_err(storage)?;
        let first_parent: Option<String> = sqlx::query(
            "SELECT parent_id FROM commit_parents WHERE commit_id = $1 AND position = 0",
        )
        .bind(request.candidate.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| row.try_get("parent_id").map_err(storage))
        .transpose()?;
        if parent_count > 1 {
            return Err(LedgerError::LineageMismatch(
                "merge commits are not accepted before Phase 5".into(),
            ));
        }
        let operation = match (&current, &request.expected_head) {
            (None, None) => {
                if parent_count != 0 {
                    return Err(LedgerError::LineageMismatch(
                        "genesis candidate must have no parents".into(),
                    ));
                }
                "genesis"
            }
            (Some(_), Some(expected)) => {
                if parent_count == 0
                    || first_parent.as_deref() != Some(expected.to_string().as_str())
                {
                    return Err(LedgerError::LineageMismatch(format!(
                        "candidate {} does not have the current HEAD {expected} as its first parent",
                        request.candidate
                    )));
                }
                "advance"
            }
            _ => unreachable!("head equality was checked above"),
        };
        self.fail_at(FailPoint::AfterLineageValidation)?;

        // Ref movement with version bump.
        let (old_version, new_version) = match &current {
            None => {
                let inserted = sqlx::query(
                    "INSERT INTO refs (graph_id, branch, head, version) VALUES ($1, $2, $3, 1) \
                     ON CONFLICT (graph_id, branch) DO NOTHING",
                )
                .bind(scope.graph.as_str())
                .bind(&request.branch)
                .bind(request.candidate.to_string())
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
                if inserted.rows_affected() != 1 {
                    // A concurrent genesis with a *different* key won (an identical key
                    // would have replayed under the advisory lock): report its head.
                    let actual = Self::read_ref(&mut tx, &scope.graph, &request.branch, false)
                        .await?
                        .map(|(head, _, _)| head);
                    return Err(LedgerError::HeadChanged {
                        expected: None,
                        actual,
                    });
                }
                (None, 1i64)
            }
            Some((head, version, _)) => {
                let updated = sqlx::query(
                    "UPDATE refs SET head = $3, version = version + 1, updated_at = now() \
                     WHERE graph_id = $1 AND branch = $2 AND head = $4",
                )
                .bind(scope.graph.as_str())
                .bind(&request.branch)
                .bind(request.candidate.to_string())
                .bind(head.to_string())
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
                if updated.rows_affected() != 1 {
                    return Err(LedgerError::Storage(
                        "locked ref row disappeared during advance".into(),
                    ));
                }
                (Some(*version), version + 1)
            }
        };
        self.fail_at(FailPoint::AfterRefUpdate)?;

        let actor = scope.principal.actor();
        let event_row = sqlx::query(
            "INSERT INTO ref_events (graph_id, branch, old_head, new_head, old_version, new_version, \
             operation, tenant_id, principal_id, principal_type, on_behalf_of, reason) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING event_id",
        )
        .bind(scope.graph.as_str())
        .bind(&request.branch)
        .bind(current_head.as_ref().map(ToString::to_string))
        .bind(request.candidate.to_string())
        .bind(old_version)
        .bind(new_version)
        .bind(operation)
        .bind(scope.principal.tenant_id.as_str())
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(request.reason.as_deref())
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        let ref_event_id: i64 = event_row.try_get("event_id").map_err(storage)?;
        self.fail_at(FailPoint::AfterRefEvent)?;

        let decision_row = sqlx::query(
            "INSERT INTO decisions (proposal_id, graph_id, branch, candidate_commit, decision, \
             tenant_id, principal_id, principal_type, on_behalf_of, reason, validation_ids, ref_event_id) \
             VALUES ($1, $2, $3, $4, 'accepted', $5, $6, $7, $8, $9, '{}', $10) RETURNING decision_id",
        )
        .bind(proposal.proposal_id)
        .bind(scope.graph.as_str())
        .bind(&request.branch)
        .bind(request.candidate.to_string())
        .bind(scope.principal.tenant_id.as_str())
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(request.reason.as_deref())
        .bind(ref_event_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_decision_insert(e, &request.candidate))?;
        let decision_id: i64 = decision_row.try_get("decision_id").map_err(storage)?;
        self.fail_at(FailPoint::AfterDecision)?;

        let outbox_row = sqlx::query(
            "INSERT INTO projection_outbox (graph_id, branch, commit_id, ref_version, event_kind, ref_event_id) \
             VALUES ($1, $2, $3, $4, 'ref_advanced', $5) RETURNING outbox_id",
        )
        .bind(scope.graph.as_str())
        .bind(&request.branch)
        .bind(request.candidate.to_string())
        .bind(new_version)
        .bind(ref_event_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        let outbox_id: i64 = outbox_row.try_get("outbox_id").map_err(storage)?;
        self.fail_at(FailPoint::AfterOutbox)?;

        Self::record_result(
            &mut tx,
            scope,
            Operation::Accept,
            "accepted",
            Some(&request.candidate),
            Some(new_version),
            Some(decision_id),
            Some(proposal.proposal_id),
        )
        .await?;
        self.fail_at(FailPoint::BeforeCommit)?;
        tx.commit().await.map_err(storage)?;
        Ok(Accepted {
            decision_id,
            ref_event_id,
            outbox_id,
            ref_version: new_version,
            head: request.candidate.clone(),
            replayed: false,
        })
    }

    async fn replay_accepted(
        conn: &mut PgConnection,
        stored: StoredResult,
        scope: &RequestScope,
    ) -> Result<Accepted, LedgerError> {
        Self::check_digest(&stored, scope)?;
        if stored.result_kind != "accepted" {
            return Err(LedgerError::Storage(
                "idempotency row is not an accept".into(),
            ));
        }
        let head: CommitId = stored
            .result_commit
            .as_deref()
            .ok_or_else(|| LedgerError::Storage("accepted result without head".into()))?
            .parse()?;
        let decision_id = stored
            .result_decision_id
            .ok_or_else(|| LedgerError::Storage("accepted result without decision".into()))?;
        let ref_version = stored
            .result_ref_version
            .ok_or_else(|| LedgerError::Storage("accepted result without version".into()))?;
        let row = sqlx::query(
            "SELECT d.ref_event_id, o.outbox_id FROM decisions d \
             JOIN projection_outbox o ON o.ref_event_id = d.ref_event_id WHERE d.decision_id = $1",
        )
        .bind(decision_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(storage)?;
        Ok(Accepted {
            decision_id,
            ref_event_id: row.try_get("ref_event_id").map_err(storage)?,
            outbox_id: row.try_get("outbox_id").map_err(storage)?,
            ref_version,
            head,
            replayed: true,
        })
    }

    /// Record a rejection atomically with its idempotency result. The ref does not move
    /// and no projection event is written. The candidate must be a prepared, undecided
    /// proposal for this ref; the graph must be active (a rejection is a workflow
    /// decision like acceptance).
    pub async fn reject(&self, request: &RejectRequest) -> Result<Rejected, LedgerError> {
        let scope = &request.scope;
        validate_scope(scope)?;
        validate_branch(&request.branch)?;
        validate_reason(Some(&request.reason))?;
        let mut tx = self.begin(scope, Operation::Reject).await?;
        if let Some(stored) = Self::stored_result(&mut tx, scope, Operation::Reject).await? {
            return Self::replay_rejected(stored, scope);
        }
        self.fail_at(FailPoint::AfterIdempotencyCheck)?;
        Self::graph_must_be_active(&mut tx, scope).await?;
        // Rejection does not depend on the current head, only on the proposal's own
        // binding to this ref.
        let proposal = Self::proposal_for(&mut tx, &request.candidate).await?;
        let Some(proposal) = proposal else {
            return Err(LedgerError::LineageMismatch(format!(
                "candidate {} has no proposal; only prepared candidates are decided",
                request.candidate
            )));
        };
        if proposal.graph_id != scope.graph.as_str() || proposal.branch != request.branch {
            return Err(LedgerError::LineageMismatch(format!(
                "candidate {} was prepared for a different ref",
                request.candidate
            )));
        }
        let expected_head: Option<CommitId> = proposal
            .expected_head
            .as_deref()
            .map(str::parse)
            .transpose()?;
        Self::bound_undecided_proposal(
            &mut tx,
            scope,
            &request.branch,
            expected_head.as_ref(),
            &request.candidate,
        )
        .await?;
        let actor = scope.principal.actor();
        let decision_row = sqlx::query(
            "INSERT INTO decisions (proposal_id, graph_id, branch, candidate_commit, decision, \
             tenant_id, principal_id, principal_type, on_behalf_of, reason, validation_ids) \
             VALUES ($1, $2, $3, $4, 'rejected', $5, $6, $7, $8, $9, '{}') RETURNING decision_id",
        )
        .bind(proposal.proposal_id)
        .bind(scope.graph.as_str())
        .bind(&request.branch)
        .bind(request.candidate.to_string())
        .bind(scope.principal.tenant_id.as_str())
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(&request.reason)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_decision_insert(e, &request.candidate))?;
        let decision_id: i64 = decision_row.try_get("decision_id").map_err(storage)?;
        self.fail_at(FailPoint::AfterDecision)?;
        Self::record_result(
            &mut tx,
            scope,
            Operation::Reject,
            "rejected",
            Some(&request.candidate),
            None,
            Some(decision_id),
            Some(proposal.proposal_id),
        )
        .await?;
        self.fail_at(FailPoint::BeforeCommit)?;
        tx.commit().await.map_err(storage)?;
        Ok(Rejected {
            decision_id,
            replayed: false,
        })
    }

    fn replay_rejected(
        stored: StoredResult,
        scope: &RequestScope,
    ) -> Result<Rejected, LedgerError> {
        Self::check_digest(&stored, scope)?;
        if stored.result_kind != "rejected" {
            return Err(LedgerError::Storage(
                "idempotency row is not a reject".into(),
            ));
        }
        Ok(Rejected {
            decision_id: stored
                .result_decision_id
                .ok_or_else(|| LedgerError::Storage("rejected result without decision".into()))?,
            replayed: true,
        })
    }

    /// Explicitly mark a proposal of the caller's graph superseded: its expected head is
    /// no longer the ref head and it has no terminal decision. No automatic supersession
    /// happens in P1.3.
    pub async fn mark_superseded(
        &self,
        principal: &AuthenticatedPrincipal,
        graph: &GraphId,
        proposal_id: i64,
        reason: &str,
    ) -> Result<i64, LedgerError> {
        validate_reason(Some(reason))?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        let owner = sqlx::query("SELECT tenant_id, status FROM graphs WHERE graph_id = $1")
            .bind(graph.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
        let Some(owner) = owner else {
            return Err(LedgerError::UnknownGraph(graph.to_string()));
        };
        let tenant: String = owner.try_get("tenant_id").map_err(storage)?;
        if tenant != principal.tenant_id.as_str() {
            return Err(LedgerError::UnknownGraph(graph.to_string()));
        }
        let status: String = owner.try_get("status").map_err(storage)?;
        if status != "active" {
            return Err(LedgerError::GraphNotActive {
                graph: graph.to_string(),
                status,
            });
        }
        let row = sqlx::query(
            "SELECT p.graph_id, p.branch, p.candidate_commit, p.expected_head, r.head \
             FROM proposals p LEFT JOIN refs r ON r.graph_id = p.graph_id AND r.branch = p.branch \
             WHERE p.proposal_id = $1 AND p.graph_id = $2 FOR NO KEY UPDATE OF p",
        )
        .bind(proposal_id)
        .bind(graph.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(row) = row else {
            return Err(LedgerError::LineageMismatch(
                "unknown proposal for this graph".into(),
            ));
        };
        let branch: String = row.try_get("branch").map_err(storage)?;
        let candidate: String = row.try_get("candidate_commit").map_err(storage)?;
        let expected_head: Option<String> = row.try_get("expected_head").map_err(storage)?;
        let current_head: Option<String> = row.try_get("head").map_err(storage)?;
        let candidate_id: CommitId = candidate.parse()?;
        let decided =
            sqlx::query("SELECT decision FROM decisions WHERE candidate_commit = $1 LIMIT 1")
                .bind(&candidate)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
        if let Some(row) = decided {
            let decision: String = row.try_get("decision").map_err(storage)?;
            return Err(LedgerError::LineageMismatch(format!(
                "candidate {candidate_id} already has a terminal decision ({decision})"
            )));
        }
        if expected_head == current_head {
            return Err(LedgerError::LineageMismatch(
                "proposal is still current; it is not superseded".into(),
            ));
        }
        let actor = principal.actor();
        let decision_row = sqlx::query(
            "INSERT INTO decisions (proposal_id, graph_id, branch, candidate_commit, decision, \
             tenant_id, principal_id, principal_type, on_behalf_of, reason, validation_ids) \
             VALUES ($1, $2, $3, $4, 'superseded', $5, $6, $7, $8, $9, '{}') RETURNING decision_id",
        )
        .bind(proposal_id)
        .bind(graph.as_str())
        .bind(&branch)
        .bind(&candidate)
        .bind(principal.tenant_id.as_str())
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(reason)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_decision_insert(e, &candidate_id))?;
        let decision_id: i64 = decision_row.try_get("decision_id").map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(decision_id)
    }
}
