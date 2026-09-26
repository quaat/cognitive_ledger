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
