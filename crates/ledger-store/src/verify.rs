//! Database invariant verification (Plan 0005 §20): inspect, never repair.
//!
//! Every check is a read-only SQL query over the ledger tables that must return zero
//! violating rows. The checks restate, as data facts, what the schema (FKs, triggers,
//! CHECKs) and the workflow are supposed to guarantee, so they detect corruption that
//! bypassed both (owner mistakes, restores, bugs). `ledger-admin verify` runs them with the
//! owner identity after every qualification slice, upgrade, restore or fault run.

use crate::db_error;
use ledger_core::LedgerError;
use sqlx::{PgPool, Row};

/// One invariant and the number of rows violating it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckResult {
    pub name: &'static str,
    pub violations: i64,
    /// Up to three offending identifiers, for the operator (never repaired).
    pub sample: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub checks: Vec<CheckResult>,
    pub counts: Vec<(&'static str, i64)>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.checks.iter().all(|c| c.violations == 0)
    }
}

/// (name, query returning one text column `id` per violating row)
const CHECKS: &[(&str, &str)] = &[
    (
        "objects are content-addressed (id = sha256(bytes))",
        "SELECT id FROM immutable_objects WHERE id <> 'sha256:' || encode(sha256(bytes), 'hex')",
    ),
    (
        "every indexed commit has its object",
        "SELECT c.id FROM commit_index c LEFT JOIN immutable_objects o ON o.id = c.id WHERE o.id IS NULL",
    ),
    (
        "every indexed commit's patch object exists",
        "SELECT c.id FROM commit_index c LEFT JOIN immutable_objects o ON o.id = c.patch_id WHERE o.id IS NULL",
    ),
    (
        "parent rows match the indexed parent count",
        "SELECT c.id FROM commit_index c \
         WHERE c.parent_count <> (SELECT count(*) FROM commit_parents p WHERE p.commit_id = c.id)",
    ),
    (
        "parents belong to the child's graph",
        "SELECT p.commit_id FROM commit_parents p \
         JOIN commit_index child ON child.id = p.commit_id \
         JOIN commit_index parent ON parent.id = p.parent_id \
         WHERE child.graph_id <> parent.graph_id",
    ),
    (
        "every parent is an indexed commit",
        "SELECT p.commit_id FROM commit_parents p LEFT JOIN commit_index i ON i.id = p.parent_id WHERE i.id IS NULL",
    ),
    (
        "every ref head is an indexed commit of its graph",
        "SELECT r.graph_id || '/' || r.branch FROM refs r \
         LEFT JOIN commit_index c ON c.id = r.head AND c.graph_id = r.graph_id WHERE c.id IS NULL",
    ),
    (
        "ref version equals its event count on active/archived graphs",
        "SELECT r.graph_id || '/' || r.branch FROM refs r JOIN graphs g ON g.graph_id = r.graph_id \
         WHERE g.status IN ('active', 'archived') \
           AND r.version <> (SELECT count(*) FROM ref_events e WHERE e.graph_id = r.graph_id AND e.branch = r.branch)",
    ),
    (
        "ref head equals its latest event's new head",
        "SELECT r.graph_id || '/' || r.branch FROM refs r JOIN graphs g ON g.graph_id = r.graph_id \
         WHERE g.status IN ('active', 'archived') AND r.version > 0 \
           AND r.head <> (SELECT e.new_head FROM ref_events e WHERE e.graph_id = r.graph_id AND e.branch = r.branch \
                          ORDER BY e.new_version DESC LIMIT 1)",
    ),
    (
        "ref events form one contiguous chain per ref",
        "SELECT e.graph_id || '/' || e.branch || '#' || e.new_version FROM ref_events e \
         WHERE e.new_version > 1 AND NOT EXISTS ( \
             SELECT 1 FROM ref_events prev WHERE prev.graph_id = e.graph_id AND prev.branch = e.branch \
               AND prev.new_version = e.new_version - 1 AND prev.new_head = e.old_head)",
    ),
    (
        "every advance is a fast-forward",
        "SELECT e.graph_id || '/' || e.branch || '#' || e.new_version FROM ref_events e \
         WHERE e.old_head IS NOT NULL AND NOT EXISTS ( \
             SELECT 1 FROM commit_parents p WHERE p.commit_id = e.new_head AND p.position = 0 AND p.parent_id = e.old_head)",
    ),
    (
        "every ref event has exactly one accepted decision",
        "SELECT e.event_id::text FROM ref_events e \
         WHERE (SELECT count(*) FROM decisions d WHERE d.ref_event_id = e.event_id AND d.decision = 'accepted') <> 1",
    ),
    (
        "every accepted decision has exactly one outbox row",
        "SELECT d.decision_id::text FROM decisions d WHERE d.decision = 'accepted' \
           AND (SELECT count(*) FROM projection_outbox o WHERE o.ref_event_id = d.ref_event_id) <> 1",
    ),
    (
        "every outbox row names its event's commit and version",
        "SELECT o.outbox_id::text FROM projection_outbox o JOIN ref_events e ON e.event_id = o.ref_event_id \
         WHERE o.commit_id <> e.new_head OR o.ref_version <> e.new_version OR o.graph_id <> e.graph_id OR o.branch <> e.branch",
    ),
    (
        "accepted decisions decide the commit their event installed",
        "SELECT d.decision_id::text FROM decisions d JOIN ref_events e ON e.event_id = d.ref_event_id \
         WHERE d.candidate_commit <> e.new_head",
    ),
    (
        "one terminal decision per candidate",
        "SELECT candidate_commit FROM decisions GROUP BY candidate_commit HAVING count(*) > 1",
    ),
    (
        "every decision's candidate was proposed on the same graph and branch",
        "SELECT d.decision_id::text FROM decisions d JOIN proposals p ON p.proposal_id = d.proposal_id \
         WHERE p.graph_id <> d.graph_id OR p.branch <> d.branch OR p.candidate_commit <> d.candidate_commit",
    ),
    (
        "audit rows agree with their graph's tenant",
        "SELECT 'proposals:' || p.proposal_id FROM proposals p JOIN graphs g ON g.graph_id = p.graph_id WHERE g.tenant_id <> p.tenant_id \
         UNION ALL SELECT 'ref_events:' || e.event_id FROM ref_events e JOIN graphs g ON g.graph_id = e.graph_id WHERE g.tenant_id <> e.tenant_id \
         UNION ALL SELECT 'decisions:' || d.decision_id FROM decisions d JOIN graphs g ON g.graph_id = d.graph_id WHERE g.tenant_id <> d.tenant_id \
         UNION ALL SELECT 'idempotency:' || i.idempotency_id FROM idempotency i JOIN graphs g ON g.graph_id = i.graph_id WHERE g.tenant_id <> i.tenant_id",
    ),
    (
        "idempotency results reference existing rows",
        "SELECT i.idempotency_id::text FROM idempotency i \
         WHERE (i.result_proposal_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM proposals p WHERE p.proposal_id = i.result_proposal_id)) \
            OR (i.result_decision_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM decisions d WHERE d.decision_id = i.result_decision_id)) \
            OR (i.result_commit IS NOT NULL AND NOT EXISTS (SELECT 1 FROM commit_index c WHERE c.id = i.result_commit))",
    ),
    (
        "idempotency scope is unique (NULL delegation is one value)",
        "SELECT tenant_id || '/' || graph_id || '/' || operation || '/' || idempotency_key FROM idempotency \
         GROUP BY tenant_id, graph_id, operation, idempotency_key, principal_id, principal_type, on_behalf_of \
         HAVING count(*) > 1",
    ),
    (
        "no v1 commit is indexed under a production graph",
        "SELECT c.id FROM commit_index c JOIN graphs g ON g.graph_id = c.graph_id \
         WHERE c.version = 1 AND g.status NOT IN ('bootstrap', 'importing')",
    ),
    (
        "every candidate proposal commit is indexed under its graph",
        "SELECT p.proposal_id::text FROM proposals p \
         LEFT JOIN commit_index c ON c.id = p.candidate_commit AND c.graph_id = p.graph_id WHERE c.id IS NULL",
    ),
    // Phase 2 (ADR-0018/0019)
    (
        "semantic execution contexts are content-addressed",
        "SELECT context_id FROM semantic_execution_contexts \
         WHERE context_id <> 'sha256:' || encode(sha256(canonical_bytes), 'hex')",
    ),
    (
        "validation records are content-addressed",
        "SELECT validation_id FROM validation_records \
         WHERE validation_id <> 'sha256:' || encode(sha256(canonical_bytes), 'hex')",
    ),
    (
        "validation records agree with their context's candidate and state digest",
        "SELECT r.validation_id FROM validation_records r JOIN semantic_execution_contexts c ON c.context_id = r.context_id \
         WHERE c.graph_id <> r.graph_id OR c.candidate_commit <> r.candidate_commit \
            OR c.candidate_state_digest <> r.candidate_state_digest",
    ),
    (
        "virtual context rows match their context's declared count",
        "SELECT c.context_id FROM semantic_execution_contexts c \
         WHERE c.virtual_context_count <> (SELECT count(*) FROM semantic_virtual_contexts v WHERE v.context_id = c.context_id)",
    ),
    (
        "result summaries are bounded by the record's reported count",
        "SELECT r.validation_id FROM validation_records r \
         WHERE (SELECT count(*) FROM validation_violations v WHERE v.validation_id = r.validation_id) > r.violation_count",
    ),
    (
        "validation records agree with their context's validator",
        "SELECT r.validation_id FROM validation_records r JOIN semantic_execution_contexts c ON c.context_id = r.context_id \
         WHERE c.validator_service_id <> r.validator_service_id OR c.validator_service_version <> r.validator_service_version \
            OR c.validator_configuration_version <> r.validator_configuration_version",
    ),
    (
        "every validation record was produced by at least one validate request (identical records are shared across keys)",
        "SELECT r.validation_id FROM validation_records r \
         WHERE (SELECT count(*) FROM idempotency i WHERE i.result_validation_id = r.validation_id AND i.operation = 'validate') < 1",
    ),
    (
        "accepted decisions cite only conforming validations",
        "SELECT d.decision_id::text FROM decisions d JOIN decision_validations dv ON dv.decision_id = d.decision_id \
         JOIN validation_records r ON r.validation_id = dv.validation_id \
         WHERE d.decision = 'accepted' AND r.outcome <> 'conforms'",
    ),
    (
        "decision validation_ids arrays equal the enforced relation",
        "SELECT d.decision_id::text FROM decisions d \
         WHERE (SELECT coalesce(array_agg(dv.validation_id ORDER BY dv.validation_id), '{}') \
                  FROM decision_validations dv WHERE dv.decision_id = d.decision_id) \
               <> (SELECT coalesce(array_agg(x ORDER BY x), '{}') FROM unnest(d.validation_ids) AS x)",
    ),
    (
        "validation audit rows agree with their graph's tenant",
        "SELECT 'validation_records:' || r.validation_id FROM validation_records r JOIN graphs g ON g.graph_id = r.graph_id WHERE g.tenant_id <> r.tenant_id \
         UNION ALL SELECT 'semantic_execution_contexts:' || c.context_id FROM semantic_execution_contexts c JOIN graphs g ON g.graph_id = c.graph_id WHERE g.tenant_id <> c.tenant_id",
    ),
    (
        "idempotency validation results reference existing records",
        "SELECT i.idempotency_id::text FROM idempotency i \
         WHERE i.result_validation_id IS NOT NULL \
           AND NOT EXISTS (SELECT 1 FROM validation_records r WHERE r.validation_id = i.result_validation_id)",
    ),
];

const COUNTED: &[&str] = &[
    "graphs",
    "refs",
    "immutable_objects",
    "commit_index",
    "commit_parents",
    "proposals",
    "ref_events",
    "decisions",
    "projection_outbox",
    "idempotency",
    "semantic_execution_contexts",
    "semantic_virtual_contexts",
    "validation_records",
    "validation_violations",
    "decision_validations",
];

/// The SQL behind a named check (tests run it inside a rolled-back tampering transaction).
pub fn query_for(name: &str) -> Option<&'static str> {
    CHECKS.iter().find(|(n, _)| *n == name).map(|(_, q)| *q)
}

/// Run every check. Read-only; never modifies anything.
pub async fn run(pool: &PgPool) -> Result<Report, LedgerError> {
    let mut report = Report::default();
    for (name, sql) in CHECKS {
        let count_sql = format!("SELECT count(*) AS n FROM ({sql}) v");
        let n: i64 = sqlx::query(&count_sql)
            .fetch_one(pool)
            .await
            .map_err(db_error)?
            .try_get("n")
            .map_err(db_error)?;
        let sample = if n > 0 {
            sqlx::query(&format!("SELECT v.* FROM ({sql}) v LIMIT 3"))
                .fetch_all(pool)
                .await
                .map_err(db_error)?
                .iter()
                .filter_map(|r| r.try_get::<String, _>(0).ok())
                .collect()
        } else {
            Vec::new()
        };
        report.checks.push(CheckResult {
            name,
            violations: n,
            sample,
        });
    }
    report.checks.extend(verify_validation_bytes(pool).await?);
    for table in COUNTED {
        let n: i64 = sqlx::query(&format!("SELECT count(*) AS n FROM {table}"))
            .fetch_one(pool)
            .await
            .map_err(db_error)?
            .try_get("n")
            .map_err(db_error)?;
        report.counts.push((table, n));
    }
    Ok(report)
}

/// Rust-side checks the SQL cannot express: every validation record and context decodes from
/// its hashed canonical bytes, and every relational projection column — including each
/// summary and virtual-context detail row at its position — agrees with the bytes
/// (acceptance decides on the bytes; the columns are for queries and audit).
async fn verify_validation_bytes(pool: &PgPool) -> Result<Vec<CheckResult>, LedgerError> {
    use ledger_validation_protocol::{SemanticExecutionContext, ValidationId};
    use std::collections::HashMap;
    // One snapshot for every query below: detail rows, records and contexts are compared with
    // each other, so a validation committed between two statements (a live server) must be
    // seen in all of them or in none.
    let mut tx = pool.begin().await.map_err(db_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    // Detail rows, grouped by parent and ordered by position (one query per table).
    let mut summaries: HashMap<String, Vec<(i32, String, String, String)>> = HashMap::new();
    for row in sqlx::query(
        "SELECT validation_id, position, severity, code, message FROM validation_violations \
         ORDER BY validation_id, position",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_error)?
    {
        summaries
            .entry(row.try_get("validation_id").map_err(db_error)?)
            .or_default()
            .push((
                row.try_get("position").map_err(db_error)?,
                row.try_get("severity").map_err(db_error)?,
                row.try_get("code").map_err(db_error)?,
                row.try_get("message").map_err(db_error)?,
            ));
    }
    type VirtualRow = (i32, String, String, Vec<String>, String, String);
    let mut virtuals: HashMap<String, Vec<VirtualRow>> = HashMap::new();
    for row in sqlx::query(
        "SELECT context_id, position, dataset_id, source_version, object_refs, query_spec_digest, \
                hydration_plan_digest FROM semantic_virtual_contexts ORDER BY context_id, position",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_error)?
    {
        virtuals
            .entry(row.try_get("context_id").map_err(db_error)?)
            .or_default()
            .push((
                row.try_get("position").map_err(db_error)?,
                row.try_get("dataset_id").map_err(db_error)?,
                row.try_get("source_version").map_err(db_error)?,
                row.try_get("object_refs").map_err(db_error)?,
                row.try_get("query_spec_digest").map_err(db_error)?,
                row.try_get("hydration_plan_digest").map_err(db_error)?,
            ));
    }
    let mut record_bad = Vec::new();
    let rows = sqlx::query(
        "SELECT r.validation_id, r.graph_id, r.candidate_commit, r.candidate_state_digest, r.context_id, \
                r.validator_service_id, r.validator_service_version, r.validator_configuration_version, \
                r.outcome, r.violation_count, r.report_digest, r.report_reference, \
                to_char(r.recorded_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS recorded_at, \
                r.canonical_bytes AS record_bytes, c.canonical_bytes AS context_bytes, \
                (SELECT count(*) FROM validation_violations v WHERE v.validation_id = r.validation_id) AS summaries \
         FROM validation_records r JOIN semantic_execution_contexts c ON c.context_id = r.context_id",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_error)?;
    for row in &rows {
        let id: String = row.try_get("validation_id").map_err(db_error)?;
        let agree = (|| -> Result<bool, LedgerError> {
            let vid: ValidationId = id.parse()?;
            let record_bytes: Vec<u8> = row.try_get("record_bytes").map_err(db_error)?;
            let context_bytes: Vec<u8> = row.try_get("context_bytes").map_err(db_error)?;
            let (record, _) =
                crate::postgres_validation::decode_stored(&vid, &record_bytes, &context_bytes)?;
            let s = |c: &str| row.try_get::<String, _>(c).map_err(db_error);
            Ok(record.graph_id.as_str() == s("graph_id")?
                && record.candidate_commit.to_string() == s("candidate_commit")?
                && record.candidate_state_digest.to_string() == s("candidate_state_digest")?
                && record.semantic_execution_context_id.to_string() == s("context_id")?
                && record.validator.service_id == s("validator_service_id")?
                && record.validator.service_version == s("validator_service_version")?
                && record.validator.configuration_version == s("validator_configuration_version")?
                && record.outcome.kind.as_str() == s("outcome")?
                && i64::from(record.outcome.violation_count)
                    == i64::from(row.try_get::<i32, _>("violation_count").map_err(db_error)?)
                && record.report_digest.to_string() == s("report_digest")?
                && ledger_core::LedgerTimestamp::parse_rfc3339(&s("recorded_at")?)? == record.recorded_at
                && record.report_reference
                    == row
                        .try_get::<Option<String>, _>("report_reference")
                        .map_err(db_error)?
                && record.outcome.violations.len() as i64
                    == row.try_get::<i64, _>("summaries").map_err(db_error)?
                // every summary row is the decoded entry at its position (0..n, no gaps)
                && summaries.get(&id).map_or(0, Vec::len) == record.outcome.violations.len()
                && summaries.get(&id).is_none_or(|rows| {
                    rows.iter().zip(&record.outcome.violations).enumerate().all(
                        |(i, ((position, severity, code, message), v))| {
                            usize::try_from(*position).is_ok_and(|p| p == i)
                                && *severity == v.severity
                                && *code == v.code
                                && *message == v.message
                        },
                    )
                }))
        })();
        if !matches!(agree, Ok(true)) {
            record_bad.push(id);
        }
    }
    let mut context_bad = Vec::new();
    let rows = sqlx::query(
        "SELECT context_id, graph_id, candidate_commit, candidate_state_digest, base_kb_id, base_kb_revision, \
                ontology_id, ontology_version, shapes_id, shapes_version, reasoning_profile, \
                reasoning_implementation, reasoning_version, sources_revision, validator_service_id, \
                validator_service_version, validator_configuration_version, virtual_context_count, \
                canonical_bytes FROM semantic_execution_contexts",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_error)?;
    for row in &rows {
        let id: String = row.try_get("context_id").map_err(db_error)?;
        let agree = (|| -> Result<bool, LedgerError> {
            let bytes: Vec<u8> = row.try_get("canonical_bytes").map_err(db_error)?;
            if ledger_core::ContentId::for_bytes(&bytes).to_string() != id {
                return Ok(false);
            }
            let c = SemanticExecutionContext::from_canonical_bytes(&bytes)?;
            let s = |col: &str| row.try_get::<String, _>(col).map_err(db_error);
            let o = |col: &str| row.try_get::<Option<String>, _>(col).map_err(db_error);
            Ok(c.graph_id.as_str() == s("graph_id")?
                && c.candidate_commit.to_string() == s("candidate_commit")?
                && c.candidate_state_digest.to_string() == s("candidate_state_digest")?
                && c.base_kb.kb_id == s("base_kb_id")?
                && c.base_kb.revision == s("base_kb_revision")?
                && c.ontology.as_ref().map(|x| x.id.clone()) == o("ontology_id")?
                && c.ontology.as_ref().map(|x| x.version.clone()) == o("ontology_version")?
                && c.shapes.id == s("shapes_id")?
                && c.shapes.version == s("shapes_version")?
                && c.reasoning.as_ref().map(|x| x.profile.clone()) == o("reasoning_profile")?
                && c.reasoning.as_ref().map(|x| x.implementation.clone())
                    == o("reasoning_implementation")?
                && c.reasoning.as_ref().map(|x| x.version.clone()) == o("reasoning_version")?
                && c.sources_revision == o("sources_revision")?
                && c.validator.service_id == s("validator_service_id")?
                && c.validator.service_version == s("validator_service_version")?
                && c.validator.configuration_version == s("validator_configuration_version")?
                && c.virtual_contexts.len() as i64
                    == i64::from(
                        row.try_get::<i32, _>("virtual_context_count")
                            .map_err(db_error)?,
                    )
                // every virtual-context row is the decoded element at its position
                && virtuals.get(&id).map_or(0, Vec::len) == c.virtual_contexts.len()
                && virtuals.get(&id).is_none_or(|rows| {
                    rows.iter().zip(&c.virtual_contexts).enumerate().all(
                        |(i, ((position, dataset, version, refs, query, plan), vc))| {
                            usize::try_from(*position).is_ok_and(|p| p == i)
                                && *dataset == vc.dataset_id
                                && *version == vc.source_version
                                && *refs == vc.object_refs
                                && *query == vc.query_spec_digest.to_string()
                                && *plan == vc.hydration_plan_digest.to_string()
                        },
                    )
                }))
        })();
        if !matches!(agree, Ok(true)) {
            context_bad.push(id);
        }
    }
    tx.rollback().await.map_err(db_error)?;
    let result = |name: &'static str, bad: Vec<String>| CheckResult {
        name,
        violations: bad.len() as i64,
        sample: bad.into_iter().take(3).collect(),
    };
    Ok(vec![
        result(
            "validation records decode from their bytes and their columns agree with them",
            record_bad,
        ),
        result(
            "semantic contexts decode from their bytes and their columns agree with them",
            context_bad,
        ),
    ])
}
