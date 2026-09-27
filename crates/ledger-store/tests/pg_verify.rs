//! `ledger_store::verify` (Plan 0005 §20): a clean workflow history reports no violations;
//! each tampering that bypasses the schema controls (owner disabling a guard) is detected
//! by the matching check; verification never modifies anything.
#![cfg(feature = "postgres")]

use ledger_core::{AuthenticatedPrincipal, GraphId, PrincipalId, PrincipalType, TenantId};
use ledger_rdf::{Operation, OperationKind, Patch, Quad};
use ledger_store::{
    AcceptRequest, GraphStatus, NewGraph, PgGraphs, PostgresLedgerStore, PrepareRequest,
    RejectRequest, RequestScope, V1Binding, ValidationPolicy, schema, verify,
};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

fn database_url() -> String {
    std::env::var("LEDGER_TEST_DATABASE_URL").expect("LEDGER_TEST_DATABASE_URL must be set")
}

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos % 1_000_000_000)
}

async fn fresh_database(prefix: &str) -> (PgPool, PgPool, String) {
    let base = database_url();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    let db = unique(prefix);
    sqlx::query(&format!("CREATE DATABASE {db}"))
        .execute(&admin)
        .await
        .unwrap();
    let (head, query) = base
        .split_once('?')
        .map_or((base.as_str(), ""), |(h, q)| (h, q));
    let slash = head.rfind('/').unwrap();
    let url = if query.is_empty() {
        format!("{}/{db}", &head[..slash])
    } else {
        format!("{}/{db}?{query}", &head[..slash])
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    schema::migrate_all(&pool).await.unwrap();
    (admin, pool, db)
}

fn principal() -> AuthenticatedPrincipal {
    AuthenticatedPrincipal {
        principal_id: PrincipalId::new("urn:sculpin:agent:verify").unwrap(),
        principal_type: PrincipalType::Agent,
        tenant_id: TenantId::new("tenant-v").unwrap(),
        on_behalf_of: None,
    }
}

fn scope(graph: &GraphId, key: &str) -> RequestScope {
    RequestScope {
        principal: principal(),
        graph: graph.clone(),
        idempotency_key: key.into(),
        request_digest: ledger_core::ContentId::for_bytes(key.as_bytes()),
        correlation_id: None,
    }
}

fn patch(quad: &str) -> Patch {
    Patch::new([Operation {
        kind: OperationKind::Add,
        quad: Quad::from_str(quad).unwrap(),
    }])
    .unwrap()
}

fn failing(report: &verify::Report) -> Vec<&'static str> {
    report
        .checks
        .iter()
        .filter(|c| c.violations > 0)
        .map(|c| c.name)
        .collect()
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn clean_history_verifies_and_each_bypass_is_detected() {
    let (admin, pool, db) = fresh_database("verify").await;
    let g = GraphId::new(unique("g").replace('_', "-")).unwrap();
    PgGraphs::new(pool.clone())
        .create(&NewGraph {
            graph_id: g.clone(),
            tenant_id: TenantId::new("tenant-v").unwrap(),
            knowledge_base_id: None,
            purpose: None,
            status: GraphStatus::Active,
        })
        .await
        .unwrap();
    let store = PostgresLedgerStore::from_pool_migrated(pool.clone(), V1Binding::Reject);
    let wf = store.workflows();
    let mut head = None;
    for i in 0..3 {
        let p = wf
            .prepare(&PrepareRequest {
                scope: scope(&g, &format!("p{i}")),
                branch: "main".into(),
                expected_head: head.clone(),
                requested: patch(&format!("<urn:s> <urn:p> \"{i}\" .")),
                activity: "a".into(),
                event_time: None,
                evidence_refs: vec![],
                source_system: None,
                message: "m".into(),
            })
            .await
            .unwrap();
        wf.accept(&AcceptRequest {
            scope: scope(&g, &format!("a{i}")),
            branch: "main".into(),
            expected_head: head.clone(),
            candidate: p.candidate.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await
        .unwrap();
        head = Some(p.candidate);
    }
    let rejected = wf
        .prepare(&PrepareRequest {
            scope: scope(&g, "pr"),
            branch: "main".into(),
            expected_head: head.clone(),
            requested: patch("<urn:s> <urn:p> \"r\" ."),
            activity: "a".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: "m".into(),
        })
        .await
        .unwrap();
    wf.reject(&RejectRequest {
        scope: scope(&g, "rr"),
        branch: "main".into(),
        candidate: rejected.candidate.clone(),
        reason: "no".into(),
        validation_id: None,
    })
    .await
    .unwrap();

    let clean = verify::run(&pool).await.unwrap();
    assert!(clean.is_clean(), "{:?}", failing(&clean));
    assert!(clean.checks.len() >= 20);
    let counts: std::collections::BTreeMap<_, _> = clean.counts.iter().cloned().collect();
    assert_eq!(counts["ref_events"], 3);
    assert_eq!(counts["decisions"], 4);
    assert_eq!(counts["projection_outbox"], 3);

    // Each bypass below needs the owner to disable a guard; verify must still catch it, and
    // running verify must change nothing (counts identical before and after).
    let snapshot = |pool: PgPool| async move { verify::run(&pool).await.unwrap().counts };
    let before = snapshot(pool.clone()).await;
    let bypass = |sql: &'static str, expect: &'static str| {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            for guard in [
                "ALTER TABLE refs DISABLE TRIGGER ALL",
                "ALTER TABLE ref_events DISABLE TRIGGER ALL",
                "ALTER TABLE decisions DISABLE TRIGGER ALL",
                "ALTER TABLE projection_outbox DISABLE TRIGGER ALL",
                "ALTER TABLE commit_parents DISABLE TRIGGER ALL",
                "ALTER TABLE commit_index DISABLE TRIGGER ALL",
                "ALTER TABLE idempotency DISABLE TRIGGER ALL",
            ] {
                sqlx::query(guard).execute(&mut *tx).await.unwrap();
            }
            sqlx::query(sql).execute(&mut *tx).await.unwrap();
            // Inside the transaction: verify sees the tampering …
            let report_sql = format!("SELECT count(*) AS n FROM ({}) v", verify_query(expect));
            let n: i64 = sqlx::query(&report_sql)
                .fetch_one(&mut *tx)
                .await
                .unwrap()
                .try_get("n")
                .unwrap();
            assert!(n > 0, "{expect}: tampering {sql:?} not detected");
            // … and is rolled back so the database stays clean.
            tx.rollback().await.unwrap();
        }
    };
    bypass(
        "UPDATE refs SET version = version + 1",
        "ref version equals its event count on active/archived graphs",
    )
    .await;
    bypass(
        "DELETE FROM projection_outbox",
        "every accepted decision has exactly one outbox row",
    )
    .await;
    bypass(
        "DELETE FROM decisions WHERE decision = 'accepted'",
        "every ref event has exactly one accepted decision",
    )
    .await;
    bypass(
        "UPDATE commit_index SET parent_count = parent_count + 1",
        "parent rows match the indexed parent count",
    )
    .await;
    bypass(
        "DELETE FROM ref_events WHERE new_version = 2",
        "ref events form one contiguous chain per ref",
    )
    .await;
    bypass(
        "UPDATE idempotency SET result_proposal_id = 999999 WHERE result_proposal_id IS NOT NULL",
        "idempotency results reference existing rows",
    )
    .await;
    let after = snapshot(pool.clone()).await;
    assert_eq!(
        before, after,
        "verification and rolled-back tampering changed nothing"
    );
    assert!(verify::run(&pool).await.unwrap().is_clean());
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .execute(&admin)
        .await
        .unwrap();
}

/// The SQL of the named check (kept in sync with the module's table by this lookup).
fn verify_query(name: &str) -> &'static str {
    verify::query_for(name).expect("check exists")
}
