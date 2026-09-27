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

// =========================================================================================
// Phase 2 (ADR-0018/0019): every validation check detects exactly its own tampering
// =========================================================================================

fn p2_validator() -> ledger_validation_protocol::ValidatorIdentity {
    ledger_validation_protocol::ValidatorIdentity {
        service_id: "urn:sculpin:service:validator".into(),
        service_version: "2026.09".into(),
        configuration_version: "cfg-1".into(),
    }
}

fn p2_context(
    graph: &GraphId,
    candidate: &ledger_core::CommitId,
    state: &ledger_core::ContentId,
    external: &str,
) -> ledger_validation_protocol::SemanticExecutionContext {
    use ledger_validation_protocol::{
        BaseKb, Ontology, Reasoning, SemanticExecutionContext, ShapeSet, VirtualContextRef,
    };
    SemanticExecutionContext {
        graph_id: graph.clone(),
        candidate_commit: candidate.clone(),
        candidate_state_digest: state.clone(),
        base_kb: BaseKb {
            kb_id: "urn:exodus:kb:verify".into(),
            revision: "kbrev-1".into(),
        },
        ontology: Some(Ontology {
            id: "urn:sculpin:ontology:core".into(),
            version: "O1".into(),
        }),
        shapes: ShapeSet {
            id: "urn:sculpin:shapes:core".into(),
            version: "S1".into(),
        },
        reasoning: Some(Reasoning {
            profile: "rdfs".into(),
            implementation: "sculpin-python-reasoner".into(),
            version: "0.9".into(),
        }),
        sources_revision: Some(format!("catalog-{external}")),
        virtual_contexts: vec![VirtualContextRef {
            dataset_id: "urn:sculpin:datasource:lab".into(),
            source_version: external.into(),
            object_refs: vec!["o1".into()],
            query_spec_digest: ledger_core::ContentId::for_bytes(b"q"),
            hydration_plan_digest: ledger_core::ContentId::for_bytes(b"h"),
        }],
        validator: p2_validator(),
    }
}

fn p2_outcome(conforming: bool) -> ledger_validation_protocol::ValidationOutcome {
    use ledger_validation_protocol::{OutcomeKind, ValidationOutcome, ViolationSummary};
    if conforming {
        ValidationOutcome::conforms()
    } else {
        ValidationOutcome {
            kind: OutcomeKind::Violations,
            violation_count: 2,
            violations: vec![ViolationSummary {
                severity: "Violation".into(),
                code: "sh:MinCountConstraintComponent".into(),
                message: "missing label".into(),
            }],
        }
    }
}

/// Validate `candidate` under key `key` with a scripted validator answer.
async fn p2_validate(
    store: &PostgresLedgerStore,
    graph: &GraphId,
    key: &str,
    candidate: &ledger_core::CommitId,
    external: &str,
    conforming: bool,
) -> ledger_store::RecordedValidation {
    let request = ledger_store::ValidateRequest {
        scope: scope(graph, key),
        candidate: candidate.clone(),
        requested: ledger_validation_protocol::RequestedContext::default(),
    };
    let ledger_store::ValidationBegin::Fresh(ticket) =
        store.validations().begin(&request).await.unwrap()
    else {
        panic!("{key}: no result yet");
    };
    store
        .validations()
        .record(
            &request,
            &ticket,
            ledger_store::ValidatorOutcome {
                context: p2_context(graph, candidate, ticket.state_digest(), external),
                outcome: p2_outcome(conforming),
                report_digest: ledger_core::ContentId::for_bytes(key.as_bytes()),
                report_reference: None,
            },
        )
        .await
        .unwrap()
}

/// Names of every SQL check (a check with a query) in the verifier's table, in order.
fn sql_check_names(report: &verify::Report) -> Vec<&'static str> {
    report
        .checks
        .iter()
        .map(|c| c.name)
        .filter(|n| verify::query_for(n).is_some())
        .collect()
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn each_phase2_check_detects_exactly_its_own_tampering() {
    let (admin, pool, db) = fresh_database("verify_p2").await;
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
    let store = PostgresLedgerStore::from_pool_migrated(pool.clone(), V1Binding::Reject)
        .with_validation_trust(
            ledger_store::ValidationTrustPolicy::single("urn:sculpin:service:validator").unwrap(),
        );
    let wf = store.workflows();
    let prepare = |key: &'static str, head: Option<ledger_core::CommitId>, v: &'static str| {
        let wf = wf.clone();
        let g = g.clone();
        async move {
            wf.prepare(&PrepareRequest {
                scope: scope(&g, key),
                branch: "main".into(),
                expected_head: head,
                requested: patch(&format!("<urn:s> <urn:p> \"{v}\" .")),
                activity: "a".into(),
                event_time: None,
                evidence_refs: vec![],
                source_system: None,
                message: "m".into(),
            })
            .await
            .unwrap()
            .candidate
        }
    };
    // History: c1 carries a violating record (bad, 1 summary of 2) and a conforming record
    // (good) under two contexts; c1 is accepted citing good.
    let c1 = prepare("p1", None, "1").await;
    let bad = p2_validate(&store, &g, "v-bad", &c1, "D-A", false).await;
    let good = p2_validate(&store, &g, "v-good", &c1, "D-B", true).await;
    let accepted = wf
        .accept(&AcceptRequest {
            scope: scope(&g, "a1"),
            branch: "main".into(),
            expected_head: None,
            candidate: c1.clone(),
            reason: None,
            validation: ValidationPolicy::Validated {
                validation_id: good.validation_id.clone(),
                semantic_environment_id: good.environment_id.clone(),
            },
        })
        .await
        .unwrap();

    let clean = verify::run(&pool).await.unwrap();
    assert!(clean.is_clean(), "{:?}", failing(&clean));
    let counts: std::collections::BTreeMap<_, _> = clean.counts.iter().cloned().collect();
    assert_eq!(counts["semantic_execution_contexts"], 2);
    assert_eq!(counts["semantic_virtual_contexts"], 2);
    assert_eq!(counts["validation_records"], 2);
    assert_eq!(counts["validation_violations"], 1);
    assert_eq!(counts["decision_validations"], 1);
    let sql_checks = sql_check_names(&clean);
    let before = clean.counts.clone();

    // Inside one rolled-back owner transaction: apply `tamper`, then evaluate every SQL check
    // on the transaction's view; exactly `expect` must report violations.
    let bypass = |tamper: Vec<String>, expect: &'static str| {
        let pool = pool.clone();
        let sql_checks = sql_checks.clone();
        async move {
            assert!(sql_checks.contains(&expect), "{expect} is a SQL check");
            let mut tx = pool.begin().await.unwrap();
            for sql in &tamper {
                sqlx::query(sql)
                    .execute(&mut *tx)
                    .await
                    .unwrap_or_else(|e| panic!("{sql}: {e}"));
            }
            let mut fired = Vec::new();
            for name in &sql_checks {
                let n: i64 = sqlx::query(&format!(
                    "SELECT count(*) AS n FROM ({}) v",
                    verify_query(name)
                ))
                .fetch_one(&mut *tx)
                .await
                .unwrap()
                .try_get("n")
                .unwrap();
                if n > 0 {
                    fired.push(*name);
                }
            }
            assert_eq!(fired, vec![expect], "tampering {tamper:?}");
            tx.rollback().await.unwrap();
        }
    };
    let bad_id = bad.validation_id.to_string();
    let good_id = good.validation_id.to_string();
    let bad_ctx = bad.context_id.to_string();
    let forged = format!("sha256:{}", "f".repeat(64));
    let zero = format!("sha256:{}", "0".repeat(64));

    bypass(
        vec![
            "ALTER TABLE semantic_execution_contexts DISABLE TRIGGER semantic_execution_contexts_write_once".into(),
            "ALTER TABLE semantic_execution_contexts DROP CONSTRAINT sec_content_addressed".into(),
            format!("UPDATE semantic_execution_contexts SET canonical_bytes = canonical_bytes || '\\x00'::bytea WHERE context_id = '{bad_ctx}'"),
        ],
        "semantic execution contexts are content-addressed",
    )
    .await;
    bypass(
        vec![
            "ALTER TABLE validation_records DISABLE TRIGGER validation_records_write_once".into(),
            "ALTER TABLE validation_records DROP CONSTRAINT vr_content_addressed".into(),
            format!("UPDATE validation_records SET canonical_bytes = canonical_bytes || '\\x00'::bytea WHERE validation_id = '{bad_id}'"),
        ],
        "validation records are content-addressed",
    )
    .await;
    bypass(
        vec![
            "ALTER TABLE validation_records DISABLE TRIGGER validation_records_write_once".into(),
            "ALTER TABLE validation_records DROP CONSTRAINT vr_context_fk".into(),
            format!("UPDATE validation_records SET candidate_state_digest = '{zero}' WHERE validation_id = '{bad_id}'"),
        ],
        "validation records agree with their context's candidate and state digest",
    )
    .await;
    bypass(
        vec![format!(
            "INSERT INTO semantic_virtual_contexts (context_id, position, dataset_id, source_version, object_refs, \
             query_spec_digest, hydration_plan_digest) \
             SELECT context_id, 1, dataset_id, 'D-extra', object_refs, query_spec_digest, hydration_plan_digest \
             FROM semantic_virtual_contexts WHERE context_id = '{bad_ctx}' AND position = 0"
        )],
        "virtual context rows match their context's declared count",
    )
    .await;
    bypass(
        vec![format!(
            "INSERT INTO validation_violations (validation_id, position, severity, code, message) \
             VALUES ('{bad_id}', 1, 'Violation', 'x', 'm'), ('{bad_id}', 2, 'Violation', 'y', 'm')"
        )],
        "result summaries are bounded by the record's reported count",
    )
    .await;
    bypass(
        vec![
            "ALTER TABLE validation_records DISABLE TRIGGER validation_records_write_once".into(),
            "ALTER TABLE validation_records DROP CONSTRAINT vr_context_fk".into(),
            format!("UPDATE validation_records SET validator_configuration_version = 'cfg-forged' WHERE validation_id = '{bad_id}'"),
        ],
        "validation records agree with their context's validator",
    )
    .await;
    bypass(
        vec![
            "ALTER TABLE idempotency DISABLE TRIGGER idempotency_write_once".into(),
            format!("DELETE FROM idempotency WHERE operation = 'validate' AND result_validation_id = '{bad_id}'"),
        ],
        "every validation record was produced by at least one validate request (identical records are shared across keys)",
    )
    .await;
    bypass(
        vec![
            "ALTER TABLE validation_records DISABLE TRIGGER validation_records_write_once".into(),
            format!("UPDATE validation_records SET outcome = 'violations', violation_count = 1 WHERE validation_id = '{good_id}'"),
        ],
        "accepted decisions cite only conforming validations",
    )
    .await;
    bypass(
        vec![
            "ALTER TABLE decisions DISABLE TRIGGER decisions_write_once".into(),
            format!(
                "UPDATE decisions SET validation_ids = '{{}}' WHERE decision_id = {}",
                accepted.decision_id
            ),
        ],
        "decision validation_ids arrays equal the enforced relation",
    )
    .await;
    bypass(
        vec![
            "ALTER TABLE validation_records DISABLE TRIGGER validation_records_write_once".into(),
            "ALTER TABLE validation_records DROP CONSTRAINT vr_graph_tenant_fk".into(),
            format!("UPDATE validation_records SET tenant_id = 'tenant-forged' WHERE validation_id = '{bad_id}'"),
        ],
        "validation audit rows agree with their graph's tenant",
    )
    .await;
    bypass(
        vec![
            "ALTER TABLE idempotency DROP CONSTRAINT idempotency_validation_fk".into(),
            format!(
                "INSERT INTO idempotency (tenant_id, principal_id, principal_type, on_behalf_of, graph_id, operation, \
                 idempotency_key, request_digest, result_kind, result_commit, result_ref_version, result_decision_id, \
                 result_proposal_id, result_validation_id) \
                 SELECT tenant_id, principal_id, principal_type, on_behalf_of, graph_id, operation, 'forged-key', \
                        request_digest, result_kind, result_commit, result_ref_version, result_decision_id, \
                        result_proposal_id, '{forged}' \
                 FROM idempotency WHERE operation = 'validate' AND result_validation_id = '{bad_id}'"
            ),
        ],
        "idempotency validation results reference existing records",
    )
    .await;
    // Every Phase-2 SQL check was exercised above (kept in sync with the verifier's table).
    let phase2 = sql_checks
        .iter()
        .skip_while(|n| **n != "semantic execution contexts are content-addressed")
        .count();
    assert_eq!(
        phase2, 11,
        "a Phase-2 SQL check was added without a tampering case"
    );
    let after = verify::run(&pool).await.unwrap();
    assert!(after.is_clean(), "{:?}", failing(&after));
    assert_eq!(
        before, after.counts,
        "rolled-back tampering changed nothing"
    );

    // The two Rust byte checks read through the pool, so their tampering is committed (in
    // this throwaway database) and then restored. Each tampering is invisible to every SQL
    // check: exactly the byte check fires.
    const RECORD_BYTES: &str =
        "validation records decode from their bytes and their columns agree with them";
    const CONTEXT_BYTES: &str =
        "semantic contexts decode from their bytes and their columns agree with them";
    let committed = |sql: Vec<String>| {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            for s in &sql {
                sqlx::query(s)
                    .execute(&mut *tx)
                    .await
                    .unwrap_or_else(|e| panic!("{s}: {e}"));
            }
            tx.commit().await.unwrap();
        }
    };
    committed(vec![
        "ALTER TABLE semantic_execution_contexts DISABLE TRIGGER semantic_execution_contexts_write_once".into(),
        format!("UPDATE semantic_execution_contexts SET base_kb_revision = 'kbrev-forged' WHERE context_id = '{bad_ctx}'"),
        "ALTER TABLE semantic_execution_contexts ENABLE TRIGGER semantic_execution_contexts_write_once".into(),
    ])
    .await;
    let report = verify::run(&pool).await.unwrap();
    assert_eq!(failing(&report), vec![CONTEXT_BYTES]);
    let check = report
        .checks
        .iter()
        .find(|c| c.name == CONTEXT_BYTES)
        .unwrap();
    assert_eq!(
        (check.violations, check.sample.clone()),
        (1, vec![bad_ctx.clone()])
    );
    committed(vec![
        "ALTER TABLE semantic_execution_contexts DISABLE TRIGGER semantic_execution_contexts_write_once".into(),
        format!("UPDATE semantic_execution_contexts SET base_kb_revision = 'kbrev-1' WHERE context_id = '{bad_ctx}'"),
        "ALTER TABLE semantic_execution_contexts ENABLE TRIGGER semantic_execution_contexts_write_once".into(),
    ])
    .await;
    let restored = verify::run(&pool).await.unwrap();
    assert!(restored.is_clean(), "{:?}", failing(&restored));

    // A record whose relational `outcome` says 'conforms' while its hashed bytes say
    // violations: only the byte check sees it, and acceptance decides on the bytes.
    let c2 = prepare("p2", Some(c1.clone()), "2").await;
    let bad2 = p2_validate(&store, &g, "v-bad2", &c2, "D-A", false).await;
    let bad2_id = bad2.validation_id.to_string();
    committed(vec![
        "ALTER TABLE validation_records DISABLE TRIGGER validation_records_write_once".into(),
        format!(
            "UPDATE validation_records SET outcome = 'conforms' WHERE validation_id = '{bad2_id}'"
        ),
        "ALTER TABLE validation_records ENABLE TRIGGER validation_records_write_once".into(),
    ])
    .await;
    let outcome_column: String =
        sqlx::query_scalar("SELECT outcome FROM validation_records WHERE validation_id = $1")
            .bind(&bad2_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outcome_column, "conforms");
    let report = verify::run(&pool).await.unwrap();
    assert_eq!(failing(&report), vec![RECORD_BYTES]);
    let check = report
        .checks
        .iter()
        .find(|c| c.name == RECORD_BYTES)
        .unwrap();
    assert_eq!(
        (check.violations, check.sample.clone()),
        (1, vec![bad2_id.clone()])
    );
    let refs_before = store.ref_head(&g, "main").await.unwrap();
    let decisions_before: i64 = sqlx::query_scalar("SELECT count(*) FROM decisions")
        .fetch_one(&pool)
        .await
        .unwrap();
    let refused = wf
        .accept(&AcceptRequest {
            scope: scope(&g, "a2"),
            branch: "main".into(),
            expected_head: Some(c1.clone()),
            candidate: c2.clone(),
            reason: None,
            validation: ValidationPolicy::Validated {
                validation_id: bad2.validation_id.clone(),
                semantic_environment_id: bad2.environment_id.clone(),
            },
        })
        .await;
    assert!(
        matches!(refused, Err(ledger_core::LedgerError::ValidationRejected)),
        "acceptance must decide on the hashed bytes, not the outcome column: {refused:?}"
    );
    assert_eq!(store.ref_head(&g, "main").await.unwrap(), refs_before);
    let decisions_after: i64 = sqlx::query_scalar("SELECT count(*) FROM decisions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(decisions_before, decisions_after);
    committed(vec![
        "ALTER TABLE validation_records DISABLE TRIGGER validation_records_write_once".into(),
        format!(
            "UPDATE validation_records SET outcome = 'violations' WHERE validation_id = '{bad2_id}'"
        ),
        "ALTER TABLE validation_records ENABLE TRIGGER validation_records_write_once".into(),
    ])
    .await;
    let restored = verify::run(&pool).await.unwrap();
    assert!(restored.is_clean(), "{:?}", failing(&restored));

    // Detail rows are compared element by element with the decoded bytes: a forged summary
    // message or a forged hydrated source version is seen by exactly the byte check.
    let message: String = sqlx::query_scalar(
        "SELECT message FROM validation_violations WHERE validation_id = $1 AND position = 0",
    )
    .bind(&bad2_id)
    .fetch_one(&pool)
    .await
    .expect("the violating record has a summary");
    committed(vec![
        "ALTER TABLE validation_violations DISABLE TRIGGER validation_violations_write_once".into(),
        format!("UPDATE validation_violations SET message = 'forged' WHERE validation_id = '{bad2_id}' AND position = 0"),
        "ALTER TABLE validation_violations ENABLE TRIGGER validation_violations_write_once".into(),
    ])
    .await;
    let report = verify::run(&pool).await.unwrap();
    assert_eq!(failing(&report), vec![RECORD_BYTES]);
    committed(vec![
        "ALTER TABLE validation_violations DISABLE TRIGGER validation_violations_write_once".into(),
        format!(
            "UPDATE validation_violations SET message = '{}' WHERE validation_id = '{bad2_id}' AND position = 0",
            message.replace('\'', "''")
        ),
        "ALTER TABLE validation_violations ENABLE TRIGGER validation_violations_write_once".into(),
    ])
    .await;
    let version: String = sqlx::query_scalar(
        "SELECT source_version FROM semantic_virtual_contexts WHERE context_id = $1 AND position = 0",
    )
    .bind(&bad_ctx)
    .fetch_one(&pool)
    .await
    .expect("the context hydrated a virtual context");
    committed(vec![
        "ALTER TABLE semantic_virtual_contexts DISABLE TRIGGER semantic_virtual_contexts_write_once".into(),
        format!("UPDATE semantic_virtual_contexts SET source_version = 'forged' WHERE context_id = '{bad_ctx}' AND position = 0"),
        "ALTER TABLE semantic_virtual_contexts ENABLE TRIGGER semantic_virtual_contexts_write_once".into(),
    ])
    .await;
    let report = verify::run(&pool).await.unwrap();
    assert_eq!(failing(&report), vec![CONTEXT_BYTES]);
    committed(vec![
        "ALTER TABLE semantic_virtual_contexts DISABLE TRIGGER semantic_virtual_contexts_write_once".into(),
        format!(
            "UPDATE semantic_virtual_contexts SET source_version = '{}' WHERE context_id = '{bad_ctx}' AND position = 0",
            version.replace('\'', "''")
        ),
        "ALTER TABLE semantic_virtual_contexts ENABLE TRIGGER semantic_virtual_contexts_write_once".into(),
    ])
    .await;
    let restored = verify::run(&pool).await.unwrap();
    assert!(restored.is_clean(), "{:?}", failing(&restored));

    // Further detail-row forgeries, each inside a committed-then-reverted change: every one is
    // seen by exactly its byte check (and nothing else).
    let summary_row = format!("validation_id = '{bad2_id}' AND position = 0");
    let virtual_row = format!("context_id = '{bad_ctx}' AND position = 0");
    let cases: Vec<(&str, &str, String, String, &str)> = vec![
        (
            "validation_violations",
            "severity",
            summary_row.clone(),
            "'Forged'".into(),
            RECORD_BYTES,
        ),
        (
            "validation_violations",
            "code",
            summary_row.clone(),
            "'forged:code'".into(),
            RECORD_BYTES,
        ),
        (
            "validation_violations",
            "position",
            summary_row.clone(),
            "7".into(),
            RECORD_BYTES,
        ),
        (
            "validation_records",
            "recorded_at",
            format!("validation_id = '{bad2_id}'"),
            "recorded_at + interval '1 second'".into(),
            RECORD_BYTES,
        ),
        (
            "semantic_virtual_contexts",
            "object_refs",
            virtual_row.clone(),
            "ARRAY['s3://forged']".into(),
            CONTEXT_BYTES,
        ),
        (
            "semantic_virtual_contexts",
            "query_spec_digest",
            virtual_row.clone(),
            format!("'sha256:{}'", "e".repeat(64)),
            CONTEXT_BYTES,
        ),
        (
            "semantic_virtual_contexts",
            "hydration_plan_digest",
            virtual_row.clone(),
            format!("'sha256:{}'", "e".repeat(64)),
            CONTEXT_BYTES,
        ),
    ];
    for (table, column, row, forged, expect) in cases {
        let original: String = sqlx::query_scalar(&format!(
            "SELECT quote_literal({column}::text) || '::' || pg_typeof({column})::text FROM {table} WHERE {row}"
        ))
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|e| panic!("{table}.{column}: {e}"));
        let trigger = format!("{table}_write_once");
        let set = |value: &str| {
            vec![
                format!("ALTER TABLE {table} DISABLE TRIGGER {trigger}"),
                format!("UPDATE {table} SET {column} = {value} WHERE {row}"),
                format!("ALTER TABLE {table} ENABLE TRIGGER {trigger}"),
            ]
        };
        committed(set(&forged)).await;
        let report = verify::run(&pool).await.unwrap();
        assert_eq!(failing(&report), vec![expect], "{table}.{column}");
        let moved_row = if column == "position" {
            row.replace("position = 0", &format!("position = {forged}"))
        } else {
            row.clone()
        };
        committed(vec![
            format!("ALTER TABLE {table} DISABLE TRIGGER {trigger}"),
            format!("UPDATE {table} SET {column} = {original} WHERE {moved_row}"),
            format!("ALTER TABLE {table} ENABLE TRIGGER {trigger}"),
        ])
        .await;
        let restored = verify::run(&pool).await.unwrap();
        assert!(
            restored.is_clean(),
            "{table}.{column}: {:?}",
            failing(&restored)
        );
    }
    // The context's validator versions are compared with its bytes too. For a referenced
    // context the composite FK `vr_context_fk` already forbids the change, so the owner drops
    // it (as a compromised owner could), tampers, and restores the exact FK afterwards; the
    // record/context validator agreement check may fire as well.
    let fk: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'vr_context_fk'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    committed(vec![
        "ALTER TABLE validation_records DROP CONSTRAINT vr_context_fk".into(),
    ])
    .await;
    for column in [
        "validator_service_version",
        "validator_configuration_version",
    ] {
        let original: String = sqlx::query_scalar(&format!(
            "SELECT {column} FROM semantic_execution_contexts WHERE context_id = $1"
        ))
        .bind(&bad_ctx)
        .fetch_one(&pool)
        .await
        .unwrap();
        let set = |value: &str| {
            vec![
                "ALTER TABLE semantic_execution_contexts DISABLE TRIGGER semantic_execution_contexts_write_once".to_owned(),
                format!("UPDATE semantic_execution_contexts SET {column} = '{}' WHERE context_id = '{bad_ctx}'", value.replace('\'', "''")),
                "ALTER TABLE semantic_execution_contexts ENABLE TRIGGER semantic_execution_contexts_write_once".to_owned(),
            ]
        };
        committed(set("forged-version")).await;
        let report = verify::run(&pool).await.unwrap();
        assert!(
            failing(&report).contains(&CONTEXT_BYTES),
            "{column}: {:?}",
            failing(&report)
        );
        committed(set(&original)).await;
        let restored = verify::run(&pool).await.unwrap();
        assert!(restored.is_clean(), "{column}: {:?}", failing(&restored));
    }
    // While the FK is down: a record whose context disappeared is an orphan the byte check
    // reports (a faulty restore or owner error), not a row it silently skips.
    committed(vec![
        format!("CREATE TABLE saved_sec AS SELECT * FROM semantic_execution_contexts WHERE context_id = '{bad_ctx}'"),
        format!("CREATE TABLE saved_svc AS SELECT * FROM semantic_virtual_contexts WHERE context_id = '{bad_ctx}'"),
        "ALTER TABLE semantic_virtual_contexts DISABLE TRIGGER semantic_virtual_contexts_write_once".into(),
        "ALTER TABLE semantic_execution_contexts DISABLE TRIGGER semantic_execution_contexts_write_once".into(),
        format!("DELETE FROM semantic_virtual_contexts WHERE context_id = '{bad_ctx}'"),
        format!("DELETE FROM semantic_execution_contexts WHERE context_id = '{bad_ctx}'"),
        "ALTER TABLE semantic_execution_contexts ENABLE TRIGGER semantic_execution_contexts_write_once".into(),
        "ALTER TABLE semantic_virtual_contexts ENABLE TRIGGER semantic_virtual_contexts_write_once".into(),
    ])
    .await;
    let report = verify::run(&pool).await.unwrap();
    assert!(
        failing(&report).contains(&RECORD_BYTES),
        "an orphaned record must be reported: {:?}",
        failing(&report)
    );
    committed(vec![
        "INSERT INTO semantic_execution_contexts SELECT * FROM saved_sec".into(),
        "INSERT INTO semantic_virtual_contexts SELECT * FROM saved_svc".into(),
        "DROP TABLE saved_sec".into(),
        "DROP TABLE saved_svc".into(),
    ])
    .await;
    committed(vec![format!(
        "ALTER TABLE validation_records ADD CONSTRAINT vr_context_fk {fk}"
    )])
    .await;
    // A missing detail row is seen too (the count check sees it as well for contexts).
    let saved: (i32, String, String, String) = sqlx::query_as(
        "SELECT position, severity, code, message FROM validation_violations \
         WHERE validation_id = $1 AND position = 0",
    )
    .bind(&bad2_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    committed(vec![
        "ALTER TABLE validation_violations DISABLE TRIGGER validation_violations_write_once".into(),
        format!("DELETE FROM validation_violations WHERE {summary_row}"),
        "ALTER TABLE validation_violations ENABLE TRIGGER validation_violations_write_once".into(),
    ])
    .await;
    let report = verify::run(&pool).await.unwrap();
    assert!(
        failing(&report).contains(&RECORD_BYTES),
        "{:?}",
        failing(&report)
    );
    sqlx::query(
        "INSERT INTO validation_violations (validation_id, position, severity, code, message) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&bad2_id)
    .bind(saved.0)
    .bind(&saved.1)
    .bind(&saved.2)
    .bind(&saved.3)
    .execute(&pool)
    .await
    .unwrap();
    let restored = verify::run(&pool).await.unwrap();
    assert!(restored.is_clean(), "{:?}", failing(&restored));

    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .execute(&admin)
        .await
        .unwrap();
}
