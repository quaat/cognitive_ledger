//! Validation persistence (ADR-0018, ADR-0019; Plan 0006 P2.1/P2.2).
//!
//! `ValidationRepository` stores what a validator concluded about an immutable candidate
//! without ever holding a database lock across the outbound validator call:
//!
//! 1. [`ValidationRepository::begin`] — one read-only transaction: idempotency lookup
//!    (replay a completed identical request, refuse a different request under the same
//!    key), tenant/graph/candidate binding, bounded reconstruction of the candidate state and
//!    its `sculpin-rdf-state/v1` digest. Returns a ticket the caller hands to the validator
//!    client together with the request.
//! 2. the caller calls the validator (HTTP adapter or a test fake) with the ticket's state;
//! 3. [`ValidationRepository::record`] — one write transaction under the idempotency lock:
//!    replay if a concurrent identical request finished first, verify the validator's
//!    context against the ticket (same graph, candidate and state digest), then insert the
//!    content-addressed context (idempotent, read back and compared), its virtual-context
//!    projection, the immutable record with its bounded violation summary, and the
//!    idempotency result. `recorded_at` is assigned here.
//!
//! Nothing here interprets semantics: identifiers are bounded and stored, outcomes are
//! stored, digests are compared. The repository shares the application pool so acceptance
//! (`WorkflowRepository::accept`) can verify a record inside its own transaction.

use crate::{
    db_error,
    postgres_workflow::{Operation, RequestScope, StoredResult, WorkflowRepository},
};
use ledger_core::{CommitId, ContentId, GraphId, LedgerError, LedgerTimestamp, TenantId};
use ledger_rdf::{Quad, state_digest};
use ledger_validation_protocol::{
    RequestedContext, SemanticContextId, SemanticExecutionContext, ValidationId, ValidationOutcome,
    ValidationRecord, ValidatorIdentity, VirtualContextRef,
};
use sqlx::{PgConnection, PgPool, Row};
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

/// What `begin` established about the candidate before the validator is called.
#[derive(Clone, Debug)]
pub struct ValidationTicket {
    pub graph: GraphId,
    pub knowledge_base_id: Option<String>,
    pub candidate: CommitId,
    pub state: BTreeSet<Quad>,
    pub state_digest: ContentId,
}

/// A recorded (or replayed) validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedValidation {
    pub validation_id: ValidationId,
    pub context_id: SemanticContextId,
    pub record: ValidationRecord,
    pub replayed: bool,
}

#[derive(Clone, Debug)]
pub enum ValidationBegin {
    /// An identical earlier request already completed; its record is returned.
    Replayed(RecordedValidation),
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
}

fn now() -> Result<LedgerTimestamp, LedgerError> {
    LedgerTimestamp::try_from_offset_date_time(OffsetDateTime::now_utc())
}

impl ValidationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            limits: crate::ReconstructionLimits::DEVELOPMENT,
        }
    }

    pub fn with_limits(mut self, limits: crate::ReconstructionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Step 1 (read-only transaction). The candidate must be a prepared candidate of the
    /// caller's active graph; the graph must belong to the caller's tenant. A stored result
    /// under the same scope/key replays (same digest) or conflicts (different digest) before
    /// any reconstruction work.
    pub async fn begin(&self, request: &ValidateRequest) -> Result<ValidationBegin, LedgerError> {
        let scope = &request.scope;
        WorkflowRepository::validate_scope(scope)?;
        request.requested.validate()?;
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        if let Some(stored) =
            WorkflowRepository::stored_result(&mut tx, scope, Operation::Validate).await?
        {
            let replayed = Self::replay(&mut tx, stored, scope).await?;
            tx.rollback().await.map_err(db_error)?;
            return Ok(ValidationBegin::Replayed(replayed));
        }
        WorkflowRepository::graph_must_be_active(&mut tx, scope).await?;
        Self::candidate_is_prepared_here(&mut tx, &scope.graph, &request.candidate).await?;
        let knowledge_base_id: Option<String> =
            sqlx::query_scalar("SELECT knowledge_base_id FROM graphs WHERE graph_id = $1")
                .bind(scope.graph.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(db_error)?;
        let reconstructed =
            WorkflowRepository::state_at_on(&mut tx, &request.candidate, &self.limits).await?;
        tx.rollback().await.map_err(db_error)?;
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
    /// result atomically. A concurrent identical request that finished first is replayed.
    pub async fn record(
        &self,
        request: &ValidateRequest,
        ticket: &ValidationTicket,
        outcome: ValidatorOutcome,
    ) -> Result<RecordedValidation, LedgerError> {
        let scope = &request.scope;
        WorkflowRepository::validate_scope(scope)?;
        let context = outcome.context;
        if context.graph_id != scope.graph
            || context.candidate_commit != ticket.candidate
            || context.candidate_state_digest != ticket.state_digest
            || ticket.graph != scope.graph
            || ticket.candidate != request.candidate
        {
            return Err(LedgerError::ValidatorError(
                "the validator's context does not name the validated graph, candidate and state"
                    .into(),
            ));
        }
        let context_bytes = context.canonical_bytes()?;
        let context_id = context.id()?;
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

        let mut tx =
            WorkflowRepository::begin_scoped(&self.pool, scope, Operation::Validate).await?;
        if let Some(stored) =
            WorkflowRepository::stored_result(&mut tx, scope, Operation::Validate).await?
        {
            let replayed = Self::replay(&mut tx, stored, scope).await?;
            tx.rollback().await.map_err(db_error)?;
            return Ok(replayed);
        }
        WorkflowRepository::graph_must_be_active(&mut tx, scope).await?;
        Self::candidate_is_prepared_here(&mut tx, &scope.graph, &ticket.candidate).await?;
        Self::insert_context(&mut tx, scope, &context, &context_bytes, &context_id).await?;
        Self::insert_record(&mut tx, scope, &record, &record_bytes, &validation_id).await?;
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
        tx.commit().await.map_err(db_error)?;
        Ok(RecordedValidation {
            validation_id,
            context_id,
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
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
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
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.iter()
            .map(|row| {
                let id: String = row.try_get("validation_id").map_err(db_error)?;
                id.parse()
            })
            .collect()
    }

    async fn load_on(
        conn: &mut PgConnection,
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
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let record_bytes: Vec<u8> = row.try_get("record_bytes").map_err(db_error)?;
        let context_bytes: Vec<u8> = row.try_get("context_bytes").map_err(db_error)?;
        // Stored bytes are verified against their ids on every read, like objects.
        if ContentId::for_bytes(&record_bytes) != validation_id.0 {
            return Err(LedgerError::CorruptObject {
                id: validation_id.0.clone(),
                reason: "stored validation record bytes do not hash to the id".into(),
            });
        }
        let record = ValidationRecord::from_canonical_bytes(&record_bytes).map_err(|e| {
            LedgerError::CorruptObject {
                id: validation_id.0.clone(),
                reason: format!("stored validation record does not decode: {e}"),
            }
        })?;
        let context_id = &record.semantic_execution_context_id;
        if ContentId::for_bytes(&context_bytes) != context_id.0 {
            return Err(LedgerError::CorruptObject {
                id: context_id.0.clone(),
                reason: "stored semantic context bytes do not hash to the id".into(),
            });
        }
        let context =
            SemanticExecutionContext::from_canonical_bytes(&context_bytes).map_err(|e| {
                LedgerError::CorruptObject {
                    id: context_id.0.clone(),
                    reason: format!("stored semantic context does not decode: {e}"),
                }
            })?;
        Ok(Some((record, context)))
    }

    async fn replay(
        conn: &mut PgConnection,
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
        let (record, _) = Self::load_on(
            conn,
            &scope.principal.tenant_id,
            &scope.graph,
            &validation_id,
        )
        .await?
        .ok_or_else(|| LedgerError::Storage("validated result names a missing record".into()))?;
        Ok(RecordedValidation {
            context_id: record.semantic_execution_context_id.clone(),
            validation_id,
            record,
            replayed: true,
        })
    }

    /// The candidate must be an indexed commit of this graph that went through `prepare`
    /// (commits without a proposal are not workflow candidates). Reported as
    /// `LINEAGE_MISMATCH`, which discloses nothing about other graphs' commits.
    async fn candidate_is_prepared_here(
        conn: &mut PgConnection,
        graph: &GraphId,
        candidate: &CommitId,
    ) -> Result<(), LedgerError> {
        let indexed: Option<String> =
            sqlx::query_scalar("SELECT graph_id FROM commit_index WHERE id = $1")
                .bind(candidate.to_string())
                .fetch_optional(&mut *conn)
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
        .fetch_optional(&mut *conn)
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
    /// virtual-context projection is written only by the transaction that inserted the row.
    async fn insert_context(
        conn: &mut PgConnection,
        scope: &RequestScope,
        context: &SemanticExecutionContext,
        bytes: &[u8],
        id: &SemanticContextId,
    ) -> Result<(), LedgerError> {
        let virtual_contexts: Vec<VirtualContextRef> = context
            .canonical_virtual_contexts()?
            .iter()
            .map(|encoded| decode_virtual_context(encoded))
            .collect::<Result<_, _>>()?;
        let inserted = sqlx::query(
            "INSERT INTO semantic_execution_contexts (context_id, graph_id, tenant_id, candidate_commit, \
             candidate_state_digest, base_kb_id, base_kb_revision, ontology_id, ontology_version, shapes_id, \
             shapes_version, reasoning_profile, reasoning_implementation, reasoning_version, \
             validator_service_id, validator_service_version, validator_configuration_version, \
             virtual_context_count, canonical_bytes) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19) \
             ON CONFLICT (context_id) DO NOTHING",
        )
        .bind(id.to_string())
        .bind(scope.graph.as_str())
        .bind(scope.principal.tenant_id.as_str())
        .bind(context.candidate_commit.to_string())
        .bind(context.candidate_state_digest.to_string())
        .bind(&context.base_kb.kb_id)
        .bind(&context.base_kb.revision)
        .bind(context.ontology.as_ref().map(|o| o.id.clone()))
        .bind(context.ontology.as_ref().map(|o| o.version.clone()))
        .bind(&context.shapes.id)
        .bind(&context.shapes.version)
        .bind(&context.reasoning.profile)
        .bind(&context.reasoning.implementation)
        .bind(&context.reasoning.version)
        .bind(&context.validator.service_id)
        .bind(&context.validator.service_version)
        .bind(&context.validator.configuration_version)
        .bind(i32::try_from(virtual_contexts.len()).expect("bounded by protocol"))
        .bind(bytes)
        .execute(&mut *conn)
        .await
        .map_err(db_error)?;
        if inserted.rows_affected() == 1 {
            for (position, vc) in virtual_contexts.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO semantic_virtual_contexts (context_id, position, dataset_id, source_version, \
                     object_refs, query_spec_digest, hydration_plan_digest) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(id.to_string())
                .bind(i32::try_from(position).expect("bounded"))
                .bind(&vc.dataset_id)
                .bind(&vc.source_version)
                .bind(vc.canonical_object_refs())
                .bind(vc.query_spec_digest.to_string())
                .bind(vc.hydration_plan_digest.to_string())
                .execute(&mut *conn)
                .await
                .map_err(db_error)?;
            }
        }
        let row = sqlx::query(
            "SELECT canonical_bytes, graph_id, tenant_id FROM semantic_execution_contexts WHERE context_id = $1",
        )
        .bind(id.to_string())
        .fetch_one(&mut *conn)
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

    async fn insert_record(
        conn: &mut PgConnection,
        scope: &RequestScope,
        record: &ValidationRecord,
        bytes: &[u8],
        id: &ValidationId,
    ) -> Result<(), LedgerError> {
        let actor = scope.principal.actor();
        sqlx::query(
            "INSERT INTO validation_records (validation_id, graph_id, tenant_id, candidate_commit, \
             candidate_state_digest, context_id, validator_service_id, validator_service_version, \
             validator_configuration_version, outcome, violation_count, report_digest, report_reference, \
             recorded_at, principal_id, principal_type, on_behalf_of, correlation_id, canonical_bytes) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14::timestamptz, $15, $16, $17, $18, $19)",
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
        .execute(&mut *conn)
        .await
        .map_err(|e| match &e {
            // Same bytes, same microsecond, same everything: the record already exists (a
            // different key produced an identical record). Not corruption.
            sqlx::Error::Database(d) if d.is_unique_violation() => LedgerError::LineageMismatch(
                format!("validation record {id} already exists"),
            ),
            _ => db_error(e),
        })?;
        for (position, encoded) in record.outcome.canonical_violations()?.iter().enumerate() {
            let violation = decode_violation(encoded)?;
            sqlx::query(
                "INSERT INTO validation_violations (validation_id, position, severity, code, message) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(id.to_string())
            .bind(i32::try_from(position).expect("bounded"))
            .bind(&violation.0)
            .bind(&violation.1)
            .bind(&violation.2)
            .execute(&mut *conn)
            .await
            .map_err(db_error)?;
        }
        Ok(())
    }
}

/// Decode one canonical virtual-context element (the protocol crate exposes the encoder;
/// the relational projection needs the fields back in canonical order).
fn decode_virtual_context(encoded: &[u8]) -> Result<VirtualContextRef, LedgerError> {
    let mut rest = encoded;
    let dataset_id = read_field(&mut rest)?;
    let source_version = read_field(&mut rest)?;
    let count = read_u32(&mut rest)? as usize;
    let mut object_refs = Vec::with_capacity(count);
    for _ in 0..count {
        object_refs.push(read_field(&mut rest)?);
    }
    let query_spec_digest: ContentId = read_field(&mut rest)?.parse()?;
    let hydration_plan_digest: ContentId = read_field(&mut rest)?.parse()?;
    if !rest.is_empty() {
        return Err(LedgerError::InvalidValidation(
            "virtual context element has trailing bytes".into(),
        ));
    }
    Ok(VirtualContextRef {
        dataset_id,
        source_version,
        object_refs,
        query_spec_digest,
        hydration_plan_digest,
    })
}

fn decode_violation(encoded: &[u8]) -> Result<(String, String, String), LedgerError> {
    let mut rest = encoded;
    let severity = read_field(&mut rest)?;
    let code = read_field(&mut rest)?;
    let message = read_field(&mut rest)?;
    if !rest.is_empty() {
        return Err(LedgerError::InvalidValidation(
            "violation element has trailing bytes".into(),
        ));
    }
    Ok((severity, code, message))
}

fn read_u32(rest: &mut &[u8]) -> Result<u32, LedgerError> {
    if rest.len() < 4 {
        return Err(LedgerError::InvalidValidation("truncated count".into()));
    }
    let value = u32::from_be_bytes(rest[..4].try_into().expect("four bytes"));
    *rest = &rest[4..];
    Ok(value)
}

fn read_field(rest: &mut &[u8]) -> Result<String, LedgerError> {
    let len = read_u32(rest)? as usize;
    if rest.len() < len {
        return Err(LedgerError::InvalidValidation("truncated field".into()));
    }
    let value = std::str::from_utf8(&rest[..len])
        .map_err(|_| LedgerError::InvalidValidation("non-UTF-8 field".into()))?
        .to_owned();
    *rest = &rest[len..];
    Ok(value)
}

/// Verified facts about a validation record, as `WorkflowRepository::accept`/`reject` need
/// them inside their transaction (ADR-0019 predicates 1–5).
pub(crate) struct CitedValidation {
    pub(crate) validation_id: ValidationId,
    pub(crate) context_id: SemanticContextId,
    pub(crate) conforms: bool,
}

/// Predicates 1–3 of ADR-0019 on the caller's connection: the record exists for the caller's
/// graph and tenant (else `VALIDATION_NOT_FOUND`), names this candidate (else
/// `LINEAGE_MISMATCH`) and agrees with its context's state digest (else corruption).
pub(crate) async fn cited_validation(
    conn: &mut PgConnection,
    scope: &RequestScope,
    candidate: &CommitId,
    validation_id: &ValidationId,
) -> Result<CitedValidation, LedgerError> {
    let row = sqlx::query(
        "SELECT r.graph_id, r.tenant_id, r.candidate_commit, r.candidate_state_digest, r.context_id, r.outcome, \
                c.candidate_state_digest AS context_digest, c.candidate_commit AS context_candidate \
         FROM validation_records r JOIN semantic_execution_contexts c ON c.context_id = r.context_id \
         WHERE r.validation_id = $1",
    )
    .bind(validation_id.to_string())
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_error)?;
    let Some(row) = row else {
        return Err(LedgerError::ValidationNotFound);
    };
    let graph: String = row.try_get("graph_id").map_err(db_error)?;
    let tenant: String = row.try_get("tenant_id").map_err(db_error)?;
    if graph != scope.graph.as_str() || tenant != scope.principal.tenant_id.as_str() {
        return Err(LedgerError::ValidationNotFound);
    }
    let record_candidate: String = row.try_get("candidate_commit").map_err(db_error)?;
    if record_candidate != candidate.to_string() {
        return Err(LedgerError::LineageMismatch(format!(
            "validation {validation_id} is for another candidate"
        )));
    }
    let digest: String = row.try_get("candidate_state_digest").map_err(db_error)?;
    let context_digest: String = row.try_get("context_digest").map_err(db_error)?;
    let context_candidate: String = row.try_get("context_candidate").map_err(db_error)?;
    if digest != context_digest || context_candidate != record_candidate {
        return Err(LedgerError::CorruptObject {
            id: validation_id.0.clone(),
            reason: "validation record and its context disagree on candidate or state digest"
                .into(),
        });
    }
    let context_id: String = row.try_get("context_id").map_err(db_error)?;
    let outcome: String = row.try_get("outcome").map_err(db_error)?;
    Ok(CitedValidation {
        validation_id: validation_id.clone(),
        context_id: context_id.parse()?,
        conforms: outcome == "conforms",
    })
}

/// Used by tests and tooling: the identity a record would have; exposed so fakes can predict
/// what the ledger will store without a database.
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
