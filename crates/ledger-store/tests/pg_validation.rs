//! Real-PostgreSQL evidence for Phase 2 validation persistence and acceptance binding
//! (ADR-0018, ADR-0019; Plan 0006 P2.1/P2.2): content-addressed contexts and records,
//! write-once rows, several records per candidate, idempotent replay and conflict, tenant
//! isolation, and every ADR-0019 acceptance predicate refusing with zero side effects.
#![cfg(feature = "postgres")]

use ledger_core::{
    AuthenticatedPrincipal, CommitId, ContentId, GraphId, LedgerError, PrincipalId, PrincipalType,
    TenantId,
};
use ledger_rdf::{Operation, OperationKind, Patch, state_digest};
use ledger_store::{
    AcceptRequest, GraphStatus, NewGraph, PostgresLedgerStore, PrepareRequest, RejectRequest,
    RequestScope, V1Binding, ValidateRequest, ValidationBegin, ValidationPolicy, ValidatorOutcome,
    verify,
};
use ledger_validation_protocol::{
    BaseKb, Ontology, OutcomeKind, Reasoning, RequestedContext, SemanticExecutionContext, ShapeSet,
    ValidationId, ValidationOutcome, ValidatorIdentity, ViolationSummary, VirtualContextRef,
};
use sqlx::Row;
use std::time::{SystemTime, UNIX_EPOCH};

const IGNORE: &str =
    "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL";

fn database_url() -> String {
    std::env::var("LEDGER_TEST_DATABASE_URL").expect("LEDGER_TEST_DATABASE_URL must be set")
}

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
}

async fn store() -> PostgresLedgerStore {
    PostgresLedgerStore::connect_and_migrate(&database_url(), V1Binding::Reject)
        .await
        .unwrap()
}

const TENANT: &str = "tenant-val";

async fn graph_for(store: &PostgresLedgerStore, tenant: &str) -> GraphId {
    let id = GraphId::new(unique("val")).unwrap();
    store
        .graphs()
        .create(&NewGraph {
            graph_id: id.clone(),
            tenant_id: TenantId::new(tenant).unwrap(),
            knowledge_base_id: Some("urn:exodus:kb:val".into()),
            purpose: None,
            status: GraphStatus::Active,
        })
        .await
        .unwrap();
    id
}

async fn graph(store: &PostgresLedgerStore) -> GraphId {
    graph_for(store, TENANT).await
}

fn principal_in(tenant: &str, id: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal {
        principal_id: PrincipalId::new(format!("urn:sculpin:agent:{id}")).unwrap(),
        principal_type: PrincipalType::Agent,
        tenant_id: TenantId::new(tenant).unwrap(),
        on_behalf_of: None,
    }
}

fn scope_in(tenant: &str, graph: &GraphId, key: &str, digest: &[u8]) -> RequestScope {
    RequestScope {
        principal: principal_in(tenant, "curator"),
        graph: graph.clone(),
        idempotency_key: key.to_owned(),
        request_digest: ContentId::for_bytes(digest),
        correlation_id: Some(format!("corr-{key}")),
    }
}

fn scope(graph: &GraphId, key: &str, digest: &[u8]) -> RequestScope {
    scope_in(TENANT, graph, key, digest)
}

fn add(value: &str) -> Operation {
    Operation {
        kind: OperationKind::Add,
        quad: format!("<urn:s:{value}> <urn:p> \"{value}\" .")
            .parse()
            .unwrap(),
    }
}

async fn prepare(
    store: &PostgresLedgerStore,
    graph: &GraphId,
    key: &str,
    expected_head: Option<CommitId>,
    value: &str,
) -> CommitId {
    prepare_in(store, TENANT, graph, key, expected_head, value).await
}

async fn prepare_in(
    store: &PostgresLedgerStore,
    tenant: &str,
    graph: &GraphId,
    key: &str,
    expected_head: Option<CommitId>,
    value: &str,
) -> CommitId {
    let request = PrepareRequest {
        scope: scope_in(
            tenant,
            graph,
            key,
            format!("prepare:{key}:{value}").as_bytes(),
        ),
        branch: "main".into(),
        expected_head,
        requested: Patch::new([add(value)]).unwrap(),
        activity: "cognitive-correction".into(),
        event_time: None,
        evidence_refs: vec![],
        source_system: None,
        message: key.into(),
    };
    store.workflows().prepare(&request).await.unwrap().candidate
}

fn digest(s: &str) -> ContentId {
    ContentId::for_bytes(s.as_bytes())
}

fn validator() -> ValidatorIdentity {
    ValidatorIdentity {
        service_id: "urn:sculpin:service:validator".into(),
        service_version: "2026.09".into(),
        configuration_version: "cfg-1".into(),
    }
}

/// A deterministic "validator": the effective context it reports for a ticket, with the
/// ontology version and external-source version as knobs.
fn context_for(
    graph: &GraphId,
    candidate: &CommitId,
    state: &ContentId,
    ontology_version: &str,
    external_version: &str,
) -> SemanticExecutionContext {
    SemanticExecutionContext {
        graph_id: graph.clone(),
        candidate_commit: candidate.clone(),
        candidate_state_digest: state.clone(),
        base_kb: BaseKb {
            kb_id: "urn:exodus:kb:val".into(),
            revision: "kbrev-1".into(),
        },
        ontology: Some(Ontology {
            id: "urn:sculpin:ontology:core".into(),
            version: ontology_version.into(),
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
        virtual_contexts: vec![VirtualContextRef {
            dataset_id: "urn:sculpin:datasource:lab".into(),
            source_version: external_version.into(),
            object_refs: vec!["o1".into()],
            query_spec_digest: digest("q"),
            hydration_plan_digest: digest("h"),
        }],
        validator: validator(),
    }
}

fn conforms(context: SemanticExecutionContext) -> ValidatorOutcome {
    ValidatorOutcome {
        context,
        outcome: ValidationOutcome::conforms(),
        report_digest: digest("report-ok"),
        report_reference: None,
    }
}

fn violates(context: SemanticExecutionContext) -> ValidatorOutcome {
    ValidatorOutcome {
        context,
        outcome: ValidationOutcome {
            kind: OutcomeKind::Violations,
            violation_count: 2,
            violations: vec![ViolationSummary {
                severity: "Violation".into(),
                code: "sh:MinCountConstraintComponent".into(),
                message: "missing label".into(),
            }],
        },
        report_digest: digest("report-bad"),
        report_reference: Some("urn:sculpin:report:1".into()),
    }
}

fn validate_request(
    graph: &GraphId,
    key: &str,
    candidate: &CommitId,
    hints: RequestedContext,
) -> ValidateRequest {
    let mut digest_bytes = format!("validate:{candidate}:").into_bytes();
    hints.encode_into(&mut digest_bytes).unwrap();
    ValidateRequest {
        scope: scope(graph, key, &digest_bytes),
        candidate: candidate.clone(),
        requested: hints,
    }
}

/// Run the two-step validation with a scripted validator outcome.
async fn validate_with(
    store: &PostgresLedgerStore,
    graph: &GraphId,
    key: &str,
    candidate: &CommitId,
    hints: RequestedContext,
    outcome: impl FnOnce(&GraphId, &CommitId, &ContentId) -> ValidatorOutcome,
) -> Result<ledger_store::RecordedValidation, LedgerError> {
    let request = validate_request(graph, key, candidate, hints);
    match store.validations().begin(&request).await? {
        ValidationBegin::Replayed(recorded) => Ok(*recorded),
        ValidationBegin::Fresh(ticket) => {
            let produced = outcome(graph, candidate, ticket.state_digest());
            store
                .validations()
                .record(&request, &ticket, produced)
                .await
        }
    }
}

fn accept_request(
    graph: &GraphId,
    key: &str,
    expected_head: Option<CommitId>,
    candidate: &CommitId,
    validation: ValidationPolicy,
) -> AcceptRequest {
    AcceptRequest {
        scope: scope(
            graph,
            key,
            format!("accept:{key}:{candidate}:{validation:?}").as_bytes(),
        ),
        branch: "main".into(),
        expected_head,
        candidate: candidate.clone(),
        reason: Some("reviewed".into()),
        validation,
    }
}

async fn ref_head(store: &PostgresLedgerStore, graph: &GraphId) -> Option<(CommitId, i64)> {
    store.ref_head(graph, "main").await.unwrap()
}

async fn count(store: &PostgresLedgerStore, sql: &str, graph: &GraphId) -> i64 {
    sqlx::query(sql)
        .bind(graph.as_str())
        .fetch_one(store.pool())
        .await
        .unwrap()
        .get::<i64, _>(0)
}

async fn snapshot(store: &PostgresLedgerStore, graph: &GraphId) -> Vec<i64> {
    let mut out = Vec::new();
    for sql in [
        "SELECT count(*) FROM ref_events WHERE graph_id = $1",
        "SELECT count(*) FROM decisions WHERE graph_id = $1",
        "SELECT count(*) FROM projection_outbox WHERE graph_id = $1",
        "SELECT count(*) FROM idempotency WHERE graph_id = $1 AND operation = 'accept'",
        "SELECT count(*) FROM decision_validations WHERE graph_id = $1",
        "SELECT coalesce(max(version), 0) FROM refs WHERE graph_id = $1",
    ] {
        out.push(count(store, sql, graph).await);
    }
    out
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn contexts_and_records_are_content_addressed_write_once_and_many_per_candidate() {
    let _ = IGNORE;
    let store = store().await;
    let g = graph(&store).await;
    let c1 = prepare(&store, &g, "p1", None, "a").await;

    // The ticket's digest is the digest of the reconstructed candidate state.
    let request = validate_request(&g, "v-probe", &c1, RequestedContext::default());
    let ValidationBegin::Fresh(ticket) = store.validations().begin(&request).await.unwrap() else {
        panic!("no result yet");
    };
    let reconstructed = store
        .workflows()
        .reconstruct(&c1, &ledger_store::ReconstructionLimits::DEVELOPMENT)
        .await
        .unwrap();
    assert_eq!(ticket.state_digest().clone(), state_digest(&reconstructed));
    assert_eq!(ticket.state(), &reconstructed);
    assert_eq!(ticket.knowledge_base_id(), Some("urn:exodus:kb:val"));

    // Two validations of one candidate under two contexts (external source A and B): both
    // records coexist immutably, referencing distinct contexts.
    let a = validate_with(
        &store,
        &g,
        "v-a",
        &c1,
        RequestedContext::default(),
        |g, c, s| conforms(context_for(g, c, s, "O1", "D-A")),
    )
    .await
    .unwrap();
    let b = validate_with(
        &store,
        &g,
        "v-b",
        &c1,
        RequestedContext::default(),
        |g, c, s| violates(context_for(g, c, s, "O1", "D-B")),
    )
    .await
    .unwrap();
    assert!(!a.replayed && !b.replayed);
    assert_ne!(a.validation_id, b.validation_id);
    assert_ne!(
        a.context_id, b.context_id,
        "another external version is another context"
    );
    assert!(a.record.outcome.is_conforming());
    assert_eq!(b.record.outcome.violation_count, 2);
    let tenant = TenantId::new(TENANT).unwrap();
    let listed = store
        .validations()
        .list_for_candidate(&tenant, &g, &c1)
        .await
        .unwrap();
    assert_eq!(
        listed,
        vec![a.validation_id.clone(), b.validation_id.clone()]
    );
    let (loaded, context) = store
        .validations()
        .load(&tenant, &g, &b.validation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.id().unwrap(), b.validation_id);
    assert_eq!(context.id().unwrap(), b.context_id);
    assert_eq!(context.virtual_contexts[0].source_version, "D-B");

    // Database facts: ids hash the stored bytes; summary rows and virtual-context rows exist.
    let rows = sqlx::query(
        "SELECT (validation_id = 'sha256:' || encode(sha256(canonical_bytes), 'hex')) AS ok, \
                (SELECT count(*) FROM validation_violations v WHERE v.validation_id = r.validation_id) AS summary \
         FROM validation_records r WHERE r.graph_id = $1 ORDER BY created_at",
    )
    .bind(g.as_str())
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.get::<bool, _>("ok")));
    assert_eq!(rows[1].get::<i64, _>("summary"), 1);
    let vcs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM semantic_virtual_contexts v JOIN semantic_execution_contexts c ON c.context_id = v.context_id WHERE c.graph_id = $1",
    )
    .bind(g.as_str())
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(vcs, 2);

    // Write-once: the owner cannot update or delete a record or a context.
    for sql in [
        "UPDATE validation_records SET outcome = 'conforms', violation_count = 0 WHERE validation_id = $1",
        "DELETE FROM validation_records WHERE validation_id = $1",
        "UPDATE semantic_execution_contexts SET base_kb_revision = 'x' WHERE context_id = $1",
    ] {
        let id = if sql.contains("semantic_execution_contexts") {
            b.context_id.to_string()
        } else {
            b.validation_id.to_string()
        };
        let result = sqlx::query(sql).bind(id).execute(store.pool()).await;
        assert!(result.is_err(), "{sql} must be refused");
    }
    // Content-address CHECK: a mislabelled record is refused by the database.
    let forged = sqlx::query(
        "INSERT INTO validation_records (validation_id, graph_id, tenant_id, candidate_commit, candidate_state_digest, \
         context_id, validator_service_id, validator_service_version, validator_configuration_version, outcome, \
         violation_count, report_digest, recorded_at, principal_id, principal_type, canonical_bytes) \
         SELECT $1, graph_id, tenant_id, candidate_commit, candidate_state_digest, context_id, validator_service_id, \
                validator_service_version, validator_configuration_version, outcome, violation_count, report_digest, \
                recorded_at, principal_id, principal_type, canonical_bytes FROM validation_records WHERE validation_id = $2",
    )
    .bind(format!("sha256:{}", "f".repeat(64)))
    .bind(a.validation_id.to_string())
    .execute(store.pool())
    .await;
    match forged {
        Err(sqlx::Error::Database(e)) => assert_eq!(e.code().as_deref(), Some("23514")),
        other => panic!("forged record must be refused: {other:?}"),
    }
    let report = verify::run(store.pool()).await.unwrap();
    assert!(report.is_clean(), "{:?}", report.checks);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn validation_is_idempotent_by_scope_key_and_digest() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = prepare(&store, &g, "p1", None, "a").await;
    let first = validate_with(
        &store,
        &g,
        "v-1",
        &c1,
        RequestedContext::default(),
        |g, c, s| conforms(context_for(g, c, s, "O1", "D-A")),
    )
    .await
    .unwrap();
    // Same key, same request: replayed with the identical record, even though the validator
    // would now answer differently.
    let again = validate_with(
        &store,
        &g,
        "v-1",
        &c1,
        RequestedContext::default(),
        |g, c, s| violates(context_for(g, c, s, "O9", "D-Z")),
    )
    .await
    .unwrap();
    assert!(again.replayed);
    assert_eq!(again.validation_id, first.validation_id);
    assert_eq!(again.record, first.record);
    // Same key, different hints (different request digest): conflict, no validator call.
    let hints = RequestedContext {
        reasoning_profile: Some("owl-rl".into()),
        ..RequestedContext::default()
    };
    let request = validate_request(&g, "v-1", &c1, hints);
    assert!(matches!(
        store.validations().begin(&request).await,
        Err(LedgerError::IdempotencyConflict)
    ));
    // Two validators racing on the same key: both complete; exactly one record is created
    // and both observe the same identity.
    let c2 = prepare(&store, &g, "p2", None, "b").await;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for n in 0..2 {
        let store = PostgresLedgerStore::connect_and_migrate(&database_url(), V1Binding::Reject)
            .await
            .unwrap();
        let g = g.clone();
        let c2 = c2.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            let request = validate_request(&g, "v-race", &c2, RequestedContext::default());
            let ValidationBegin::Fresh(ticket) = store.validations().begin(&request).await.unwrap()
            else {
                panic!("fresh");
            };
            barrier.wait().await;
            let produced = conforms(context_for(
                &g,
                &c2,
                ticket.state_digest(),
                "O1",
                &format!("D-{n}"),
            ));
            store
                .validations()
                .record(&request, &ticket, produced)
                .await
                .unwrap()
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }
    assert_eq!(results[0].validation_id, results[1].validation_id);
    assert_eq!(results.iter().filter(|r| !r.replayed).count(), 1);
    let records: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM validation_records WHERE graph_id = $1 AND candidate_commit = $2",
    )
    .bind(g.as_str())
    .bind(c2.to_string())
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(records, 1);
    let idem: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM idempotency WHERE graph_id = $1 AND operation = 'validate'",
    )
    .bind(g.as_str())
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(idem, 2, "v-1 and v-race");
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn validation_refuses_unprepared_foreign_and_mismatching_candidates() {
    let store = store().await;
    let g = graph(&store).await;
    let other_tenant_graph = graph_for(&store, "tenant-other").await;
    let c1 = prepare(&store, &g, "p1", None, "a").await;
    // A commit that never went through prepare (another graph's candidate) is refused.
    let foreign = prepare_in(&store, "tenant-other", &other_tenant_graph, "p1", None, "z").await;
    let request = validate_request(&g, "v-foreign", &foreign, RequestedContext::default());
    assert!(matches!(
        store.validations().begin(&request).await,
        Err(LedgerError::LineageMismatch(_))
    ));
    // A foreign tenant cannot validate this graph's candidate (graph reported unknown).
    let mut cross = validate_request(&g, "v-cross", &c1, RequestedContext::default());
    cross.scope.principal = principal_in("tenant-other", "intruder");
    assert!(matches!(
        store.validations().begin(&cross).await,
        Err(LedgerError::UnknownGraph(_))
    ));
    // A validator response naming another candidate or another state digest is refused and
    // nothing is recorded.
    let request = validate_request(&g, "v-mismatch", &c1, RequestedContext::default());
    let ValidationBegin::Fresh(ticket) = store.validations().begin(&request).await.unwrap() else {
        panic!("fresh");
    };
    let wrong_candidate = conforms(context_for(&g, &foreign, ticket.state_digest(), "O1", "D"));
    assert!(matches!(
        store
            .validations()
            .record(&request, &ticket, wrong_candidate)
            .await,
        Err(LedgerError::ValidatorError(_))
    ));
    let wrong_digest = conforms(context_for(&g, &c1, &digest("not the state"), "O1", "D"));
    assert!(matches!(
        store
            .validations()
            .record(&request, &ticket, wrong_digest)
            .await,
        Err(LedgerError::ValidatorError(_))
    ));
    let records: i64 =
        sqlx::query_scalar("SELECT count(*) FROM validation_records WHERE graph_id = $1")
            .bind(g.as_str())
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(records, 0);
    // Reads are tenant/graph scoped: a record of graph g is invisible from another graph or
    // tenant (None, same as nonexistent).
    let recorded = validate_with(
        &store,
        &g,
        "v-ok",
        &c1,
        RequestedContext::default(),
        |g, c, s| conforms(context_for(g, c, s, "O1", "D")),
    )
    .await
    .unwrap();
    assert!(
        store
            .validations()
            .load(
                &TenantId::new("tenant-other").unwrap(),
                &other_tenant_graph,
                &recorded.validation_id
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .validations()
            .load(
                &TenantId::new("tenant-other").unwrap(),
                &g,
                &recorded.validation_id
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn acceptance_is_bound_to_a_conforming_validation_of_the_named_context() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = prepare(&store, &g, "p1", None, "a").await;
    let bad = validate_with(
        &store,
        &g,
        "v-bad",
        &c1,
        RequestedContext::default(),
        |g, c, s| violates(context_for(g, c, s, "O1", "D")),
    )
    .await
    .unwrap();
    let good = validate_with(
        &store,
        &g,
        "v-good",
        &c1,
        RequestedContext::default(),
        |g, c, s| conforms(context_for(g, c, s, "O1", "D")),
    )
    .await
    .unwrap();
    let other_graph = graph(&store).await;
    let other_candidate = prepare(&store, &other_graph, "p1", None, "q").await;
    let other = validate_with(
        &store,
        &other_graph,
        "v-o",
        &other_candidate,
        RequestedContext::default(),
        |g, c, s| conforms(context_for(g, c, s, "O1", "D")),
    )
    .await
    .unwrap();
    let c2 = prepare(&store, &g, "p2", None, "b").await;
    let c2_good = validate_with(
        &store,
        &g,
        "v-c2",
        &c2,
        RequestedContext::default(),
        |g, c, s| conforms(context_for(g, c, s, "O1", "D")),
    )
    .await
    .unwrap();
    // The context id the reviewer would name after an ontology change (O2): computed from
    // the frozen layout, exactly as Sculpin would.
    // The environment the orchestrator names after an ontology change (O2): computed from the
    // frozen layout without any candidate, exactly as Sculpin would publish it.
    let stale_context = context_for(&g, &c1, &good.record.candidate_state_digest, "O2", "D")
        .environment_id()
        .unwrap();
    assert_ne!(stale_context, good.environment_id);

    let before = snapshot(&store, &g).await;
    type Case = (&'static str, ValidationPolicy, fn(&LedgerError) -> bool);
    let cases: Vec<Case> = vec![
        ("required", ValidationPolicy::Required, |e| {
            matches!(e, LedgerError::ValidationRequired)
        }),
        (
            "unknown",
            ValidationPolicy::Validated {
                validation_id: ValidationId(digest("nope")),
                semantic_environment_id: good.environment_id.clone(),
            },
            |e| matches!(e, LedgerError::ValidationNotFound),
        ),
        (
            "foreign graph",
            ValidationPolicy::Validated {
                validation_id: other.validation_id.clone(),
                semantic_environment_id: other.environment_id.clone(),
            },
            |e| matches!(e, LedgerError::ValidationNotFound),
        ),
        (
            "other candidate",
            ValidationPolicy::Validated {
                validation_id: c2_good.validation_id.clone(),
                semantic_environment_id: c2_good.environment_id.clone(),
            },
            |e| matches!(e, LedgerError::LineageMismatch(_)),
        ),
        (
            "violations",
            ValidationPolicy::Validated {
                validation_id: bad.validation_id.clone(),
                semantic_environment_id: bad.environment_id.clone(),
            },
            |e| matches!(e, LedgerError::ValidationRejected),
        ),
        (
            "stale context",
            ValidationPolicy::Validated {
                validation_id: good.validation_id.clone(),
                semantic_environment_id: stale_context,
            },
            |e| matches!(e, LedgerError::ValidationStale(_)),
        ),
    ];
    for (name, policy, expect) in cases {
        let request = accept_request(&g, &format!("a-{name}"), None, &c1, policy);
        let error = store.workflows().accept(&request).await.unwrap_err();
        assert!(expect(&error), "{name}: {error}");
        assert_eq!(ref_head(&store, &g).await, None, "{name}: ref moved");
        assert_eq!(snapshot(&store, &g).await, before, "{name}: side effects");
    }
    // The immutable records are all still there.
    let records: i64 =
        sqlx::query_scalar("SELECT count(*) FROM validation_records WHERE graph_id = $1")
            .bind(g.as_str())
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(records, 3);

    // The matching pair accepts atomically, links the decision to the validation, and
    // replays under the same key.
    let request = accept_request(
        &g,
        "a-ok",
        None,
        &c1,
        ValidationPolicy::Validated {
            validation_id: good.validation_id.clone(),
            semantic_environment_id: good.environment_id.clone(),
        },
    );
    let accepted = store.workflows().accept(&request).await.unwrap();
    assert!(!accepted.replayed);
    assert_eq!(ref_head(&store, &g).await, Some((c1.clone(), 1)));
    let row = sqlx::query(
        "SELECT d.validation_ids, (SELECT count(*) FROM decision_validations dv WHERE dv.decision_id = d.decision_id) AS linked, \
                (SELECT dv.validation_id FROM decision_validations dv WHERE dv.decision_id = d.decision_id) AS linked_id \
         FROM decisions d WHERE d.decision_id = $1",
    )
    .bind(accepted.decision_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(
        row.get::<Vec<String>, _>("validation_ids"),
        vec![good.validation_id.to_string()]
    );
    assert_eq!(row.get::<i64, _>("linked"), 1);
    assert_eq!(
        row.get::<String, _>("linked_id"),
        good.validation_id.to_string()
    );
    let replay = store.workflows().accept(&request).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.decision_id, accepted.decision_id);
    assert_eq!(ref_head(&store, &g).await, Some((c1.clone(), 1)));

    // The database refuses a decision_validations row that crosses candidates or graphs,
    // whoever writes it (owner here).
    let cross = sqlx::query(
        "INSERT INTO decision_validations (decision_id, validation_id, graph_id, candidate_commit) VALUES ($1, $2, $3, $4)",
    )
    .bind(accepted.decision_id)
    .bind(c2_good.validation_id.to_string())
    .bind(g.as_str())
    .bind(c1.to_string())
    .execute(store.pool())
    .await;
    match cross {
        Err(sqlx::Error::Database(e)) => assert_eq!(e.code().as_deref(), Some("23503")),
        other => panic!("cross-candidate link must be refused: {other:?}"),
    }
    let report = verify::run(store.pool()).await.unwrap();
    assert!(report.is_clean(), "{:?}", report.checks);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn revalidation_after_rejection_and_head_race_and_rejected_decisions_cite_validations() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = prepare(&store, &g, "p1", None, "a").await;
    // V1 (context A) non-conforming → the reviewer records a rejection citing it? No: the
    // candidate stays undecided so it can be revalidated; V2 (context B) conforms → accept.
    let v1 = validate_with(
        &store,
        &g,
        "v1",
        &c1,
        RequestedContext::default(),
        |g, c, s| violates(context_for(g, c, s, "O1", "D-A")),
    )
    .await
    .unwrap();
    let v2 = validate_with(
        &store,
        &g,
        "v2",
        &c1,
        RequestedContext::default(),
        |g, c, s| conforms(context_for(g, c, s, "O1", "D-B")),
    )
    .await
    .unwrap();
    assert_ne!(v1.context_id, v2.context_id);
    let with_v1 = accept_request(
        &g,
        "a1",
        None,
        &c1,
        ValidationPolicy::Validated {
            validation_id: v1.validation_id.clone(),
            semantic_environment_id: v1.environment_id.clone(),
        },
    );
    assert!(matches!(
        store.workflows().accept(&with_v1).await,
        Err(LedgerError::ValidationRejected)
    ));
    let with_v2 = accept_request(
        &g,
        "a2",
        None,
        &c1,
        ValidationPolicy::Validated {
            validation_id: v2.validation_id.clone(),
            semantic_environment_id: v2.environment_id.clone(),
        },
    );
    let accepted = store.workflows().accept(&with_v2).await.unwrap();
    assert_eq!(accepted.head, c1);

    // Two candidates on top of c1; one accepts, the other (validated and conforming) loses
    // the HEAD race with HEAD_CHANGED and no side effects.
    let c2 = prepare(&store, &g, "p2", Some(c1.clone()), "b").await;
    let c3 = prepare(&store, &g, "p3", Some(c1.clone()), "c").await;
    let v_c2 = validate_with(
        &store,
        &g,
        "vc2",
        &c2,
        RequestedContext::default(),
        |g, c, s| conforms(context_for(g, c, s, "O1", "D-B")),
    )
    .await
    .unwrap();
    let v_c3 = validate_with(
        &store,
        &g,
        "vc3",
        &c3,
        RequestedContext::default(),
        |g, c, s| conforms(context_for(g, c, s, "O1", "D-B")),
    )
    .await
    .unwrap();
    store
        .workflows()
        .accept(&accept_request(
            &g,
            "a-c2",
            Some(c1.clone()),
            &c2,
            ValidationPolicy::Validated {
                validation_id: v_c2.validation_id.clone(),
                semantic_environment_id: v_c2.environment_id.clone(),
            },
        ))
        .await
        .unwrap();
    let before = snapshot(&store, &g).await;
    let lost = store
        .workflows()
        .accept(&accept_request(
            &g,
            "a-c3",
            Some(c1.clone()),
            &c3,
            ValidationPolicy::Validated {
                validation_id: v_c3.validation_id.clone(),
                semantic_environment_id: v_c3.environment_id.clone(),
            },
        ))
        .await;
    assert!(matches!(lost, Err(LedgerError::HeadChanged { .. })));
    assert_eq!(snapshot(&store, &g).await, before);
    assert_eq!(ref_head(&store, &g).await, Some((c2.clone(), 2)));

    // The superseded candidate is rejected citing its (conforming) validation: the decision
    // is auditable and the record stays; a rejection citing another candidate's validation is
    // refused.
    let wrong = RejectRequest {
        scope: scope(&g, "r-wrong", b"reject-wrong"),
        branch: "main".into(),
        candidate: c3.clone(),
        reason: "superseded".into(),
        validation_id: Some(v_c2.validation_id.clone()),
    };
    assert!(matches!(
        store.workflows().reject(&wrong).await,
        Err(LedgerError::LineageMismatch(_))
    ));
    let rejected = store
        .workflows()
        .reject(&RejectRequest {
            scope: scope(&g, "r-c3", b"reject-c3"),
            branch: "main".into(),
            candidate: c3.clone(),
            reason: "superseded by c2".into(),
            validation_id: Some(v_c3.validation_id.clone()),
        })
        .await
        .unwrap();
    let linked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM decision_validations WHERE decision_id = $1")
            .bind(rejected.decision_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(linked, 1);
    let records: i64 =
        sqlx::query_scalar("SELECT count(*) FROM validation_records WHERE graph_id = $1")
            .bind(g.as_str())
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(records, 4, "no record was deleted");
    let report = verify::run(store.pool()).await.unwrap();
    assert!(report.is_clean(), "{:?}", report.checks);
}
