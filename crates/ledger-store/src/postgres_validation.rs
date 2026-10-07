//! Validation persistence (ADR-0018, ADR-0019; Plan 0006 P2.1/P2.2).
//!
//! `ValidationRepository` stores what a validator concluded about an immutable candidate
//! without ever holding a database lock across the outbound validator call:
//!
//! 1. [`ValidationRepository::begin`] — one read-only transaction: idempotency lookup
//!    (replay a completed identical request, refuse a different request under the same
//!    key), tenant/graph/candidate binding, bounded reconstruction of the candidate state and
//!    its `sculpin-rdf-state/v1` digest. Returns a ticket only this module can construct.
//! 2. the caller calls the validator (HTTP adapter or a test fake) with the ticket's state;
//! 3. [`ValidationRepository::record`] — one write transaction under the idempotency lock:
//!    replay if a concurrent identical request finished first, verify the validator's
//!    context against the ticket (same graph, candidate and state digest), then insert the
//!    content-addressed context and record (each idempotent: insert-or-nothing, read back,
//!    compare bytes), the context's virtual-context projection, the bounded result summary
//!    and the idempotency result. `recorded_at` is assigned here.
//!
//! Nothing here interprets semantics: identifiers are bounded and stored, verdicts are
//! stored, digests are compared. Acceptance decisions read the hashed canonical bytes, never
//! the relational projection columns.

use crate::FailPoint;
#[cfg(feature = "test-hooks")]
use crate::lifecycle::Statements;
use crate::{
    db_error,
    postgres_workflow::{Operation, RequestScope, StoredResult, WorkflowRepository},
};
use ledger_core::{CommitId, ContentId, GraphId, LedgerError, LedgerTimestamp, TenantId};
use ledger_rdf::{Quad, state_digest};
use ledger_validation_protocol::{
    RequestedContext, SemanticContextId, SemanticEnvironmentId, SemanticExecutionContext,
    ValidationId, ValidationOutcome, ValidationRecord, ValidatorIdentity,
};
use sqlx::{PgPool, Row};
use std::collections::BTreeSet;
use time::OffsetDateTime;

/// A request to validate a prepared candidate. `requested` are the caller's context hints;
/// they are request identity (ADR-0015 v2) and travel to the validator unchanged.
#[derive(Clone, Debug)]
pub struct ValidateRequest {
    pub scope: RequestScope,
    pub candidate: CommitId,
    pub requested: RequestedContext,
}

/// What `begin` established about the candidate before the validator is called. Only this
/// module constructs tickets, so `record` can trust the state digest it carries.
#[derive(Clone, Debug)]
pub struct ValidationTicket {
    graph: GraphId,
    knowledge_base_id: Option<String>,
    candidate: CommitId,
    state: BTreeSet<Quad>,
    state_digest: ContentId,
}

impl ValidationTicket {
    pub fn graph(&self) -> &GraphId {
        &self.graph
    }
    pub fn knowledge_base_id(&self) -> Option<&str> {
        self.knowledge_base_id.as_deref()
    }
    pub fn candidate(&self) -> &CommitId {
        &self.candidate
    }
    pub fn state(&self) -> &BTreeSet<Quad> {
        &self.state
    }
    pub fn state_digest(&self) -> &ContentId {
        &self.state_digest
    }
}

/// A recorded (or replayed) validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedValidation {
    pub validation_id: ValidationId,
    pub context_id: SemanticContextId,
    /// The candidate-independent environment the validation ran in (ADR-0019): what an
    /// accepting party names.
    pub environment_id: SemanticEnvironmentId,
    pub record: ValidationRecord,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub enum ValidationBegin {
    /// An identical earlier request already completed; its record is returned.
    Replayed(Box<RecordedValidation>),
    /// No result yet: call the validator with this ticket, then `record`.
    Fresh(ValidationTicket),
}

/// The validator's answer as the caller hands it to `record`: the effective context
/// (already checked to name the ticket's candidate and digest by the protocol crate), the
/// outcome, and the report reference.
#[derive(Clone, Debug)]
pub struct ValidatorOutcome {
    pub context: SemanticExecutionContext,
    pub outcome: ValidationOutcome,
    pub report_digest: ContentId,
    pub report_reference: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ValidationRepository {
    pool: PgPool,
    limits: crate::ReconstructionLimits,
    /// Lifecycle limits (ADR-0026): transaction bound and acquire timeout.
    session: crate::DbSessionLimits,
    #[cfg(feature = "test-hooks")]
    failpoint: Option<FailPoint>,
    #[cfg(feature = "test-hooks")]
    pause: Option<crate::test_hooks::PauseHook>,
}

fn now() -> Result<LedgerTimestamp, LedgerError> {
    LedgerTimestamp::try_from_offset_date_time(OffsetDateTime::now_utc())
}

impl ValidationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            limits: crate::ReconstructionLimits::DEVELOPMENT,
            session: crate::DbSessionLimits::default(),
            #[cfg(feature = "test-hooks")]
            failpoint: None,
            #[cfg(feature = "test-hooks")]
            pause: None,
        }
    }

    pub fn with_limits(mut self, limits: crate::ReconstructionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Lifecycle limits for this repository's acquisitions and transactions (ADR-0026).
    pub fn with_session_limits(mut self, session: crate::DbSessionLimits) -> Self {
        self.session = session;
        self
    }

    /// Pause (or inject a slow statement into) the record transaction at the hook's point
    /// (feature `test-hooks` only).
    #[cfg(feature = "test-hooks")]
    pub fn with_pause_hook(mut self, hook: crate::test_hooks::PauseHook) -> Self {
        self.pause = Some(hook);
        self
    }

    /// This repository's pause hook as the transaction start-up hook (test-hooks builds).
    fn begin_hook(&self) -> crate::lifecycle::BeginHook<'_> {
        #[cfg(feature = "test-hooks")]
        {
            self.pause.as_ref()
        }
        #[cfg(not(feature = "test-hooks"))]
        {
            None
        }
    }

    #[cfg(feature = "test-hooks")]
    async fn pause_at(&self, point: crate::test_hooks::HookPoint) {
        if let Some(hook) = &self.pause {
            let _ = hook.at(point, None).await;
        }
    }

    #[cfg(feature = "test-hooks")]
    async fn hook_at(
        &self,
        point: crate::test_hooks::HookPoint,
        tx: &mut crate::lifecycle::BoundedTx,
    ) -> Result<(), LedgerError> {
        match &self.pause {
            Some(hook) => hook.at(point, Some(tx)).await,
            None => Ok(()),
        }
    }

    /// Abort `record`'s transaction at `point` (tests only): `AfterLineageValidation` after
    /// the context insert, `AfterDecision` after the record insert, `BeforeCommit` after the
    /// idempotency result. Feature `test-hooks` only.
    #[cfg(feature = "test-hooks")]
    pub fn with_failpoint(mut self, point: FailPoint) -> Self {
        self.failpoint = Some(point);
        self
    }

    #[cfg(feature = "test-hooks")]
    fn fail_at(&self, point: FailPoint) -> Result<(), LedgerError> {
        if self.failpoint == Some(point) {
            return Err(LedgerError::Storage(format!(
                "injected failure at {point:?}"
            )));
        }
        Ok(())
    }

    /// Step 1 (read-only transaction). The candidate must be a prepared candidate of the
    /// caller's active graph; the graph must belong to the caller's tenant. A stored result
    /// under the same scope/key replays (same digest) or conflicts (different digest) before
    /// any reconstruction work.
    pub async fn begin(&self, request: &ValidateRequest) -> Result<ValidationBegin, LedgerError> {
        let limits = self.limits;
        self.begin_with_limits(request, &limits).await
    }

    /// The idempotent-replay lookup alone (no locks held, no reconstruction): lets callers
    /// answer a completed request before spending admission slots or calling a validator.
    pub async fn replayed(
        &self,
        request: &ValidateRequest,
    ) -> Result<Option<RecordedValidation>, LedgerError> {
        let scope = &request.scope;
        WorkflowRepository::validate_scope(scope)?;
        let mut conn = crate::lifecycle::acquire(&self.pool, self.session.acquire_timeout).await?;
        match WorkflowRepository::stored_result(&mut conn, scope, Operation::Validate).await? {
            Some(stored) => Ok(Some(Self::replay(&mut conn, stored, scope).await?)),
            None => Ok(None),
        }
    }

    /// `begin` under tighter reconstruction limits (the API caps the state at what it may
    /// ship to the validator, so an oversized candidate is refused while reconstructing).
    pub async fn begin_with_limits(
        &self,
        request: &ValidateRequest,
        limits: &crate::ReconstructionLimits,
    ) -> Result<ValidationBegin, LedgerError> {
        let scope = &request.scope;
        WorkflowRepository::validate_scope(scope)?;
        request.requested.validate()?;
        let mut tx = crate::lifecycle::begin(&self.pool, &self.session, self.begin_hook()).await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(tx.stmt("the isolation level")?)
            .await
            .map_err(db_error)?;
        if let Some(stored) =
            WorkflowRepository::stored_result(&mut tx, scope, Operation::Validate).await?
        {
            let replayed = Self::replay(&mut tx, stored, scope).await?;
            tx.rollback().await?;
            return Ok(ValidationBegin::Replayed(Box::new(replayed)));
        }
        tx.check_deadline("the graph lock")?;
        WorkflowRepository::graph_must_be_active(&mut tx, scope).await?;
        Self::candidate_is_prepared_here(&mut tx, &scope.graph, &request.candidate).await?;
        let knowledge_base_id: Option<String> =
            sqlx::query_scalar("SELECT knowledge_base_id FROM graphs WHERE graph_id = $1")
                .bind(scope.graph.as_str())
                .fetch_one(tx.stmt("the graphs lookup")?)
                .await
                .map_err(db_error)?;
        tx.check_deadline("the candidate-state reconstruction")?;
        let reconstructed = WorkflowRepository::state_at_on_windowed(
            &mut tx,
            &request.candidate,
            limits,
            crate::RetrievalWindows::DEFAULT,
        )
        .await?;
        tx.rollback().await?;
        let digest = state_digest(&reconstructed.state);
        Ok(ValidationBegin::Fresh(ValidationTicket {
            graph: scope.graph.clone(),
            knowledge_base_id,
            candidate: request.candidate.clone(),
            state: reconstructed.state,
            state_digest: digest,
        }))
    }

    /// Step 3 (write transaction). Verifies the validator's context names the ticket's graph,
    /// candidate and state digest, then persists context, record, summary and the idempotency
    /// result atomically. A concurrent identical request that finished first is replayed; an
    /// identical record produced under another key is shared, not duplicated.
    pub async fn record(
        &self,
        request: &ValidateRequest,
        ticket: &ValidationTicket,
        outcome: ValidatorOutcome,
    ) -> Result<RecordedValidation, LedgerError> {
        let scope = &request.scope;
        WorkflowRepository::validate_scope(scope)?;
        if ticket.graph != scope.graph || ticket.candidate != request.candidate {
            return Err(LedgerError::Storage(
                "validation ticket does not belong to this request".into(),
            ));
        }
        let context = outcome.context;
        if context.graph_id != scope.graph
            || context.candidate_commit != ticket.candidate
            || context.candidate_state_digest != ticket.state_digest
        {
            return Err(LedgerError::ValidatorError(
                "the validator's context does not name the validated graph, candidate and state"
                    .into(),
            ));
        }
        let context_bytes = context.canonical_bytes()?;
        let context_id = context.id()?;
        let environment_id = context.environment_id()?;
        let record = ValidationRecord {
            graph_id: scope.graph.clone(),
            candidate_commit: ticket.candidate.clone(),
            candidate_state_digest: ticket.state_digest.clone(),
            semantic_execution_context_id: context_id.clone(),
            validator: context.validator.clone(),
            outcome: outcome.outcome,
            recorded_at: now()?,
            report_digest: outcome.report_digest,
            report_reference: outcome.report_reference,
        };
        let record_bytes = record.canonical_bytes()?;
        let validation_id = record.id()?;

        let mut tx = WorkflowRepository::begin_scoped(
            &self.pool,
            &self.session,
            scope,
            Operation::Validate,
            self.begin_hook(),
        )
        .await?;
        if let Some(stored) =
            WorkflowRepository::stored_result(&mut tx, scope, Operation::Validate).await?
        {
            let replayed = Self::replay(&mut tx, stored, scope).await?;
            tx.rollback().await?;
            return Ok(replayed);
        }
        #[cfg(feature = "test-hooks")]
        self.hook_at(crate::test_hooks::HookPoint::AfterReplayCheck, &mut tx)
            .await?;
        tx.check_deadline("the graph lock and the context insert")?;
        WorkflowRepository::graph_must_be_active(&mut tx, scope).await?;
        Self::candidate_is_prepared_here(&mut tx, &scope.graph, &ticket.candidate).await?;
        Self::insert_context(&mut tx, scope, &context_bytes, &context_id).await?;
        #[cfg(feature = "test-hooks")]
        self.fail_at(FailPoint::AfterLineageValidation)?;
        tx.check_deadline("the validation record")?;
        Self::insert_record(&mut tx, scope, &record_bytes, &validation_id).await?;
        #[cfg(feature = "test-hooks")]
        self.fail_at(FailPoint::AfterDecision)?;
        tx.check_deadline("the idempotency result")?;
        WorkflowRepository::record_result(
            &mut tx,
            scope,
            Operation::Validate,
            "validated",
            Some(&ticket.candidate),
            None,
            None,
            None,
            Some(&validation_id.0),
        )
        .await?;
        #[cfg(feature = "test-hooks")]
        self.fail_at(FailPoint::BeforeCommit)?;
        #[cfg(feature = "test-hooks")]
        self.hook_at(crate::test_hooks::HookPoint::BeforeCommit, &mut tx)
            .await?;
        tx.commit().await?;
        #[cfg(feature = "test-hooks")]
        self.pause_at(crate::test_hooks::HookPoint::AfterCommit)
            .await;
        Ok(RecordedValidation {
            validation_id,
            context_id,
            environment_id,
            record,
            replayed: false,
        })
    }

    /// Load one record with its context, scoped to the caller's tenant and graph. A record
    /// of another graph or tenant is `None`, indistinguishable from nonexistent.
    pub async fn load(
        &self,
        tenant: &TenantId,
        graph: &GraphId,
        validation_id: &ValidationId,
    ) -> Result<Option<(ValidationRecord, SemanticExecutionContext)>, LedgerError> {
        let mut conn = crate::lifecycle::acquire(&self.pool, self.session.acquire_timeout).await?;
        Self::load_on(&mut conn, tenant, graph, validation_id).await
    }

    /// Validation ids recorded for a candidate of the caller's graph, oldest first.
    pub async fn list_for_candidate(
        &self,
        tenant: &TenantId,
        graph: &GraphId,
        candidate: &CommitId,
    ) -> Result<Vec<ValidationId>, LedgerError> {
        let rows = sqlx::query(
            "SELECT validation_id FROM validation_records \
             WHERE graph_id = $1 AND tenant_id = $2 AND candidate_commit = $3 \
             ORDER BY created_at, validation_id",
        )
        .bind(graph.as_str())
        .bind(tenant.as_str())
        .bind(candidate.to_string())
        .fetch_all(
            crate::lifecycle::acquire(&self.pool, self.session.acquire_timeout)
                .await?
                .stmt("the validation_records lookup")?,
        )
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let id: String = row.try_get("validation_id").map_err(db_error)?;
                id.parse()
            })
            .collect()
    }

    /// Read and verify a record and its context from their hashed canonical bytes (never
    /// from the relational projection). `None` when no record with that id exists for the
    /// tenant/graph pair.
    pub(crate) async fn load_on(
        conn: &mut dyn Statements,
        tenant: &TenantId,
        graph: &GraphId,
        validation_id: &ValidationId,
    ) -> Result<Option<(ValidationRecord, SemanticExecutionContext)>, LedgerError> {
        let row = sqlx::query(
            "SELECT r.canonical_bytes AS record_bytes, c.canonical_bytes AS context_bytes \
             FROM validation_records r JOIN semantic_execution_contexts c ON c.context_id = r.context_id \
             WHERE r.validation_id = $1 AND r.graph_id = $2 AND r.tenant_id = $3",
        )
        .bind(validation_id.to_string())
        .bind(graph.as_str())
        .bind(tenant.as_str())
        .fetch_optional(conn.stmt("the validation_records lookup")?)
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let record_bytes: Vec<u8> = row.try_get("record_bytes").map_err(db_error)?;
        let context_bytes: Vec<u8> = row.try_get("context_bytes").map_err(db_error)?;
        let (record, context) = decode_stored(validation_id, &record_bytes, &context_bytes)?;
        if &record.graph_id != graph {
            return Err(LedgerError::CorruptObject {
                id: validation_id.0.clone(),
                reason: "stored validation bytes name another graph than their row".into(),
            });
        }
        Ok(Some((record, context)))
    }

    async fn replay(
        conn: &mut dyn Statements,
        stored: StoredResult,
        scope: &RequestScope,
    ) -> Result<RecordedValidation, LedgerError> {
        WorkflowRepository::check_digest(&stored, scope)?;
        if stored.result_kind != "validated" {
            return Err(LedgerError::Storage(
                "idempotency row is not a validation".into(),
            ));
        }
        let validation_id: ValidationId = stored
            .result_validation_id
            .as_deref()
            .ok_or_else(|| LedgerError::Storage("validated result without record id".into()))?
            .parse()?;
        let (record, context) = Self::load_on(
            conn,
            &scope.principal.tenant_id,
            &scope.graph,
            &validation_id,
        )
        .await?
        .ok_or_else(|| LedgerError::Storage("validated result names a missing record".into()))?;
        Ok(RecordedValidation {
            context_id: record.semantic_execution_context_id.clone(),
            environment_id: context.environment_id()?,
            validation_id,
            record,
            replayed: true,
        })
    }

    /// The candidate must be an indexed commit of this graph that went through `prepare`
    /// (commits without a proposal are not workflow candidates). Reported as
    /// `LINEAGE_MISMATCH`, which discloses nothing about other graphs' commits.
    async fn candidate_is_prepared_here(
        conn: &mut dyn Statements,
        graph: &GraphId,
        candidate: &CommitId,
    ) -> Result<(), LedgerError> {
        let indexed: Option<String> =
            sqlx::query_scalar("SELECT graph_id FROM commit_index WHERE id = $1")
                .bind(candidate.to_string())
                .fetch_optional(conn.stmt("the commit_index lookup")?)
                .await
                .map_err(db_error)?;
        if indexed.as_deref() != Some(graph.as_str()) {
            return Err(LedgerError::LineageMismatch(format!(
                "candidate {candidate} is not an indexed commit of this graph"
            )));
        }
        let proposed: Option<i64> = sqlx::query_scalar(
            "SELECT proposal_id FROM proposals WHERE candidate_commit = $1 AND graph_id = $2",
        )
        .bind(candidate.to_string())
        .bind(graph.as_str())
        .fetch_optional(conn.stmt("the proposals lookup")?)
        .await
        .map_err(db_error)?;
        if proposed.is_none() {
            return Err(LedgerError::LineageMismatch(format!(
                "candidate {candidate} has no proposal; only prepared candidates are validated"
            )));
        }
        Ok(())
    }

    /// Content-addressed, idempotent publication of a context: insert-or-nothing, then read
    /// back and compare (a different row under the same id is corruption, never success). The
    /// projection columns come from the decoded canonical bytes; the virtual-context
    /// projection is written only by the transaction that inserted the row.
    async fn insert_context(
        conn: &mut dyn Statements,
        scope: &RequestScope,
        bytes: &[u8],
        id: &SemanticContextId,
    ) -> Result<(), LedgerError> {
        let canonical = SemanticExecutionContext::from_canonical_bytes(bytes)?;
        let reasoning = canonical.reasoning.as_ref();
        let inserted = sqlx::query(
            "INSERT INTO semantic_execution_contexts (context_id, graph_id, tenant_id, candidate_commit, \
             candidate_state_digest, base_kb_id, base_kb_revision, ontology_id, ontology_version, shapes_id, \
             shapes_version, reasoning_profile, reasoning_implementation, reasoning_version, \
             validator_service_id, validator_service_version, validator_configuration_version, \
             virtual_context_count, canonical_bytes, sources_revision) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20) \
             ON CONFLICT (context_id) DO NOTHING",
        )
        .bind(id.to_string())
        .bind(scope.graph.as_str())
        .bind(scope.principal.tenant_id.as_str())
        .bind(canonical.candidate_commit.to_string())
        .bind(canonical.candidate_state_digest.to_string())
        .bind(&canonical.base_kb.kb_id)
        .bind(&canonical.base_kb.revision)
        .bind(canonical.ontology.as_ref().map(|o| o.id.clone()))
        .bind(canonical.ontology.as_ref().map(|o| o.version.clone()))
        .bind(&canonical.shapes.id)
        .bind(&canonical.shapes.version)
        .bind(reasoning.map(|r| r.profile.clone()))
        .bind(reasoning.map(|r| r.implementation.clone()))
        .bind(reasoning.map(|r| r.version.clone()))
        .bind(&canonical.validator.service_id)
        .bind(&canonical.validator.service_version)
        .bind(&canonical.validator.configuration_version)
        .bind(i32::try_from(canonical.virtual_contexts.len()).expect("bounded by protocol"))
        .bind(bytes)
        .bind(canonical.sources_revision.as_deref())
        .execute(conn.stmt("the semantic_execution_contexts insert")?)
        .await
        .map_err(db_error)?;
        if inserted.rows_affected() == 1 {
            for (position, vc) in canonical.virtual_contexts.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO semantic_virtual_contexts (context_id, position, dataset_id, source_version, \
                     object_refs, query_spec_digest, hydration_plan_digest) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(id.to_string())
                .bind(i32::try_from(position).expect("bounded"))
                .bind(&vc.dataset_id)
                .bind(&vc.source_version)
                .bind(&vc.object_refs)
                .bind(vc.query_spec_digest.to_string())
                .bind(vc.hydration_plan_digest.to_string())
                .execute(conn.stmt("the semantic_virtual_contexts insert")?)
                .await
                .map_err(db_error)?;
            }
        }
        let row = sqlx::query(
            "SELECT canonical_bytes, graph_id, tenant_id FROM semantic_execution_contexts WHERE context_id = $1",
        )
        .bind(id.to_string())
        .fetch_one(conn.stmt("the semantic_execution_contexts lookup")?)
        .await
        .map_err(db_error)?;
        let stored: Vec<u8> = row.try_get("canonical_bytes").map_err(db_error)?;
        let graph: String = row.try_get("graph_id").map_err(db_error)?;
        let tenant: String = row.try_get("tenant_id").map_err(db_error)?;
        if stored != bytes
            || graph != scope.graph.as_str()
            || tenant != scope.principal.tenant_id.as_str()
        {
            return Err(LedgerError::ObjectCollision(id.0.clone()));
        }
        Ok(())
    }

    /// Content-addressed, idempotent publication of a record. Two requests under different
    /// keys that produce byte-identical records (same microsecond, same verdict) share one
    /// row; the requester columns keep the first writer (audit), the identity is unaffected.
    async fn insert_record(
        conn: &mut dyn Statements,
        scope: &RequestScope,
        bytes: &[u8],
        id: &ValidationId,
    ) -> Result<(), LedgerError> {
        let record = ValidationRecord::from_canonical_bytes(bytes)?;
        let actor = scope.principal.actor();
        let inserted = sqlx::query(
            "INSERT INTO validation_records (validation_id, graph_id, tenant_id, candidate_commit, \
             candidate_state_digest, context_id, validator_service_id, validator_service_version, \
             validator_configuration_version, outcome, violation_count, report_digest, report_reference, \
             recorded_at, principal_id, principal_type, on_behalf_of, correlation_id, canonical_bytes) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14::timestamptz, $15, $16, $17, $18, $19) \
             ON CONFLICT (validation_id) DO NOTHING",
        )
        .bind(id.to_string())
        .bind(scope.graph.as_str())
        .bind(scope.principal.tenant_id.as_str())
        .bind(record.candidate_commit.to_string())
        .bind(record.candidate_state_digest.to_string())
        .bind(record.semantic_execution_context_id.to_string())
        .bind(&record.validator.service_id)
        .bind(&record.validator.service_version)
        .bind(&record.validator.configuration_version)
        .bind(record.outcome.kind.as_str())
        .bind(i32::try_from(record.outcome.violation_count).map_err(|_| {
            LedgerError::InvalidValidation("violation_count exceeds the storable range".into())
        })?)
        .bind(record.report_digest.to_string())
        .bind(record.report_reference.as_deref())
        .bind(record.recorded_at.canonical())
        .bind(actor.principal_id.as_str())
        .bind(actor.principal_type.as_str())
        .bind(actor.on_behalf_of.as_ref().map(|p| p.as_str().to_owned()))
        .bind(scope.correlation_id.as_deref())
        .bind(bytes)
        .execute(conn.stmt("the validation_records insert")?)
        .await
        .map_err(db_error)?;
        if inserted.rows_affected() == 1 {
            for (position, violation) in record.outcome.violations.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO validation_violations (validation_id, position, severity, code, message) \
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(id.to_string())
                .bind(i32::try_from(position).expect("bounded"))
                .bind(&violation.severity)
                .bind(&violation.code)
                .bind(&violation.message)
                .execute(conn.stmt("the validation_violations insert")?)
                .await
                .map_err(db_error)?;
            }
            return Ok(());
        }
        // Same rule as contexts: an existing row must agree in bytes *and* in the graph and
        // tenant it is bound to, so a mismatching projection is a collision, not a later FK
        // failure.
        let row = sqlx::query(
            "SELECT canonical_bytes, graph_id, tenant_id FROM validation_records WHERE validation_id = $1",
        )
        .bind(id.to_string())
        .fetch_one(conn.stmt("the validation_records lookup")?)
        .await
        .map_err(db_error)?;
        let stored: Vec<u8> = row.try_get("canonical_bytes").map_err(db_error)?;
        let graph: String = row.try_get("graph_id").map_err(db_error)?;
        let tenant: String = row.try_get("tenant_id").map_err(db_error)?;
        if stored != bytes
            || graph != scope.graph.as_str()
            || tenant != scope.principal.tenant_id.as_str()
        {
            return Err(LedgerError::ObjectCollision(id.0.clone()));
        }
        Ok(())
    }
}

/// Verify stored record/context bytes against their ids and decode them strictly.
pub(crate) fn decode_stored(
    validation_id: &ValidationId,
    record_bytes: &[u8],
    context_bytes: &[u8],
) -> Result<(ValidationRecord, SemanticExecutionContext), LedgerError> {
    if ContentId::for_bytes(record_bytes) != validation_id.0 {
        return Err(LedgerError::CorruptObject {
            id: validation_id.0.clone(),
            reason: "stored validation record bytes do not hash to the id".into(),
        });
    }
    let record = ValidationRecord::from_canonical_bytes(record_bytes).map_err(|e| {
        LedgerError::CorruptObject {
            id: validation_id.0.clone(),
            reason: format!("stored validation record does not decode: {e}"),
        }
    })?;
    let context_id = &record.semantic_execution_context_id;
    if ContentId::for_bytes(context_bytes) != context_id.0 {
        return Err(LedgerError::CorruptObject {
            id: context_id.0.clone(),
            reason: "stored semantic context bytes do not hash to the id".into(),
        });
    }
    let context = SemanticExecutionContext::from_canonical_bytes(context_bytes).map_err(|e| {
        LedgerError::CorruptObject {
            id: context_id.0.clone(),
            reason: format!("stored semantic context does not decode: {e}"),
        }
    })?;
    if context.candidate_commit != record.candidate_commit
        || context.candidate_state_digest != record.candidate_state_digest
        || context.graph_id != record.graph_id
        || context.validator != record.validator
    {
        return Err(LedgerError::CorruptObject {
            id: validation_id.0.clone(),
            reason: "validation record and its context disagree".into(),
        });
    }
    Ok((record, context))
}

/// Verified facts about a validation record, as `WorkflowRepository::accept`/`reject` need
/// them inside their transaction (ADR-0019 predicates 1–5), derived from the hashed bytes.
pub(crate) struct CitedValidation {
    pub(crate) validation_id: ValidationId,
    pub(crate) environment_id: SemanticEnvironmentId,
    pub(crate) conforms: bool,
    pub(crate) validator_service_id: String,
    /// The candidate state digest the validator validated (`sculpin-rdf-state/v1`).
    pub(crate) candidate_state_digest: ledger_core::ContentId,
}

/// Predicates 1–3 of ADR-0019 on the caller's connection: the record exists for the caller's
/// graph and tenant (else `VALIDATION_NOT_FOUND`), names this candidate (else
/// `LINEAGE_MISMATCH`) and agrees with its context (else corruption). The verdict and the
/// environment come from the verified canonical bytes.
pub(crate) async fn cited_validation(
    conn: &mut dyn Statements,
    scope: &RequestScope,
    candidate: &CommitId,
    validation_id: &ValidationId,
) -> Result<CitedValidation, LedgerError> {
    let Some((record, context)) = ValidationRepository::load_on(
        conn,
        &scope.principal.tenant_id,
        &scope.graph,
        validation_id,
    )
    .await?
    else {
        return Err(LedgerError::ValidationNotFound);
    };
    if &record.candidate_commit != candidate {
        return Err(LedgerError::LineageMismatch(format!(
            "validation {validation_id} is for another candidate"
        )));
    }
    Ok(CitedValidation {
        validation_id: validation_id.clone(),
        environment_id: context.environment_id()?,
        conforms: record.outcome.is_conforming(),
        validator_service_id: record.validator.service_id.clone(),
        candidate_state_digest: record.candidate_state_digest.clone(),
    })
}

/// Used by tests and tooling: the validator identity a context will carry.
pub fn validator_identity(
    service_id: &str,
    service_version: &str,
    configuration_version: &str,
) -> ValidatorIdentity {
    ValidatorIdentity {
        service_id: service_id.to_owned(),
        service_version: service_version.to_owned(),
        configuration_version: configuration_version.to_owned(),
    }
}
