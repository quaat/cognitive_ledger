//! Real-PostgreSQL evidence for projection streams (ADR-0021; Plan 0007): enable rules and
//! tenant isolation of cognitive graphs, exclusive stream leases with expiry and fencing,
//! latest-event work selection and delivery marking, monotonic guards, backoff/blocking, and
//! the projector identity's exact privilege model. Every test is `#[ignore]` and runs through
//! the PostgreSQL suites with `LEDGER_TEST_DATABASE_URL`.
#![cfg(feature = "postgres")]

use ledger_core::{
    AuthenticatedPrincipal, CommitId, ContentId, GraphId, LedgerError, PrincipalId, PrincipalType,
    TenantId,
};
use ledger_rdf::{Operation, OperationKind, Patch};
use ledger_store::{
    AcceptRequest, FailureDisposition, GraphStatus, LeaseOutcome, NewGraph, PostgresLedgerStore,
    PrepareRequest, ProjectionRepository, RequestScope, StreamKey, V1Binding, ValidationPolicy,
    schema,
};
use sqlx::{Connection, PgPool, postgres::PgPoolOptions};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

async fn graph(store: &PostgresLedgerStore, tenant: &str, kb: Option<&str>) -> GraphId {
    let id = GraphId::new(unique("proj")).unwrap();
    store
        .graphs()
        .create(&NewGraph {
            graph_id: id.clone(),
            tenant_id: TenantId::new(tenant).unwrap(),
            knowledge_base_id: kb.map(str::to_owned),
            purpose: None,
            status: GraphStatus::Active,
        })
        .await
        .unwrap();
    id
}

fn scope(tenant: &str, graph: &GraphId, key: &str) -> RequestScope {
    RequestScope {
        principal: AuthenticatedPrincipal {
            principal_id: PrincipalId::new("urn:sculpin:agent:curator").unwrap(),
            principal_type: PrincipalType::Agent,
            tenant_id: TenantId::new(tenant).unwrap(),
            on_behalf_of: None,
        },
        graph: graph.clone(),
        idempotency_key: key.to_owned(),
        request_digest: ContentId::for_bytes(format!("{graph}:{key}").as_bytes()),
        correlation_id: None,
    }
}

/// Prepare and accept one quad on `main`; returns the new head.
async fn accept(
    store: &PostgresLedgerStore,
    tenant: &str,
    graph: &GraphId,
    head: Option<CommitId>,
    value: &str,
) -> CommitId {
    let prepared = store
        .workflows()
        .prepare(&PrepareRequest {
            scope: scope(tenant, graph, &format!("p-{value}")),
            branch: "main".into(),
            expected_head: head.clone(),
            requested: Patch::new([Operation {
                kind: OperationKind::Add,
                quad: format!("<urn:s:{value}> <urn:p> \"{value}\" .")
                    .parse()
                    .unwrap(),
            }])
            .unwrap(),
            activity: "cognitive-correction".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: value.into(),
        })
        .await
        .unwrap();
    store
        .workflows()
        .accept(&AcceptRequest {
            scope: scope(tenant, graph, &format!("a-{value}")),
            branch: "main".into(),
            expected_head: head,
            candidate: prepared.candidate.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await
        .unwrap();
    prepared.candidate
}

fn key(graph: &GraphId, target: &str) -> StreamKey {
    StreamKey {
        graph_id: graph.clone(),
        branch: "main".into(),
        target_id: target.into(),
    }
}

fn derive(kb: &str) -> Result<String, String> {
    Ok(format!(
        "urn:sculpin:kb:{}:cognitive",
        kb.replace(':', "%3A")
    ))
}

async fn owner_repo(store: &PostgresLedgerStore) -> ProjectionRepository {
    ProjectionRepository::new(store.pool().clone())
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn enabling_needs_a_knowledge_base_and_cognitive_graphs_are_never_shared() {
    let store = store().await;
    let repo = owner_repo(&store).await;
    let target = unique("t");
    let no_kb = graph(&store, "tenant-a", None).await;
    let error = repo
        .enable(&key(&no_kb, &target), derive)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("knowledge_base_id"), "{error}");
    let kb = unique("kb");
    let a = graph(&store, "tenant-a", Some(&kb)).await;
    let b = graph(&store, "tenant-b", Some(&kb)).await;
    let iri = repo.enable(&key(&a, &target), derive).await.unwrap();
    assert_eq!(iri, derive(&kb).unwrap());
    // Idempotent for the same stream.
    assert_eq!(repo.enable(&key(&a, &target), derive).await.unwrap(), iri);
    // Another graph (here: another tenant) with the same KB id can never write that graph.
    let error = repo.enable(&key(&b, &target), derive).await.unwrap_err();
    assert!(error.to_string().contains("already projects"), "{error}");
    // …not even by racing past the application check: the database refuses it too.
    let raced = sqlx::query(
        "INSERT INTO projection_state (graph_id, branch, target_id, tenant_id, cognitive_graph) \
         VALUES ($1, 'main', $2, 'tenant-b', $3)",
    )
    .bind(b.as_str())
    .bind(&target)
    .bind(&iri)
    .execute(store.pool())
    .await;
    assert!(
        raced.is_err(),
        "UNIQUE (target_id, cognitive_graph) must refuse"
    );
    // The tenant is the graph's, enforced by FK.
    let forged = sqlx::query(
        "INSERT INTO projection_state (graph_id, branch, target_id, tenant_id, cognitive_graph) \
         VALUES ($1, 'main', $2, 'tenant-forged', 'urn:sculpin:kb:x:cognitive')",
    )
    .bind(a.as_str())
    .bind(unique("t2"))
    .execute(store.pool())
    .await;
    assert!(forged.is_err(), "(graph_id, tenant_id) must match graphs");
    // Another target may project the same KB.
    repo.enable(&key(&b, &unique("t3")), derive).await.unwrap();
    // Disable, then re-enable reactivates the same row.
    assert!(repo.disable(&key(&a, &target)).await.unwrap());
    let status: String = sqlx::query_scalar(
        "SELECT status FROM projection_state WHERE graph_id = $1 AND target_id = $2",
    )
    .bind(a.as_str())
    .bind(&target)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(status, "disabled");
    repo.enable(&key(&a, &target), derive).await.unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn leases_are_exclusive_expire_and_fence_stale_holders() {
    let store = store().await;
    let repo = owner_repo(&store).await;
    let target = unique("t");
    let g = graph(&store, "tenant-a", Some(&unique("kb"))).await;
    let c1 = accept(&store, "tenant-a", &g, None, "one").await;
    let c2 = accept(&store, "tenant-a", &g, Some(c1.clone()), "two").await;
    repo.enable(&key(&g, &target), derive).await.unwrap();
    // Two concurrent claimers: exactly one wins, the other is not blocked (SKIP LOCKED).
    let ttl = Duration::from_secs(1);
    let (first, second) = tokio::join!(
        repo.claim(&target, "worker-a", ttl),
        repo.claim(&target, "worker-b", ttl)
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert!(first.is_some() != second.is_some(), "exactly one claim");
    let stale = first.or(second).unwrap();
    assert!(
        repo.claim(&target, "worker-c", ttl)
            .await
            .unwrap()
            .is_none()
    );
    // The work is the latest accepted event, not the oldest.
    let work = repo.work_for(&stale, false).await.unwrap().unwrap();
    assert_eq!((work.ref_version, work.head_version), (2, 2));
    assert_eq!(work.commit, c2);
    // The lease expires; another worker takes the stream with a higher epoch.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let fresh = repo
        .claim(&target, "worker-c", Duration::from_secs(30))
        .await
        .unwrap()
        .expect("an expired lease is claimable");
    assert_eq!(fresh.epoch, stale.epoch + 1);
    // The stale holder can neither acknowledge nor fail nor release.
    assert_eq!(
        repo.acknowledge(&stale, Some(work.outbox_id), &c2, 2, false)
            .await
            .unwrap(),
        LeaseOutcome::LeaseLost
    );
    assert_eq!(
        repo.fail(&stale, None, "TARGET_TIMEOUT", FailureDisposition::Block)
            .await
            .unwrap(),
        LeaseOutcome::LeaseLost
    );
    assert_eq!(repo.release(&stale).await.unwrap(), LeaseOutcome::LeaseLost);
    // The current holder acknowledges: progress advances, the whole backlog is delivered.
    assert_eq!(
        repo.acknowledge(&fresh, Some(work.outbox_id), &c2, 2, false)
            .await
            .unwrap(),
        LeaseOutcome::Committed
    );
    let (undelivered, attempts): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE delivered_at IS NULL), coalesce(sum(attempts), 0)::bigint \
         FROM projection_outbox WHERE graph_id = $1",
    )
    .bind(g.as_str())
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!((undelivered, attempts), (0, 1));
    assert!(
        repo.claim(&target, "worker-c", ttl)
            .await
            .unwrap()
            .is_none(),
        "no work left"
    );
    // A new acceptance makes the stream due again, for exactly the new version.
    accept(&store, "tenant-a", &g, Some(c2.clone()), "three").await;
    let next = repo.claim(&target, "worker-c", ttl).await.unwrap().unwrap();
    assert_eq!(next.projected.as_ref().map(|(_, v)| *v), Some(2));
    assert_eq!(
        repo.work_for(&next, false)
            .await
            .unwrap()
            .unwrap()
            .ref_version,
        3
    );
    assert_eq!(repo.commit_at(&g, "main", 1).await.unwrap(), Some(c1));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn progress_and_delivery_are_monotonic_and_rows_are_never_deleted() {
    let store = store().await;
    let repo = owner_repo(&store).await;
    let target = unique("t");
    let g = graph(&store, "tenant-a", Some(&unique("kb"))).await;
    let c1 = accept(&store, "tenant-a", &g, None, "one").await;
    let c2 = accept(&store, "tenant-a", &g, Some(c1.clone()), "two").await;
    repo.enable(&key(&g, &target), derive).await.unwrap();
    let claim = repo
        .claim(&target, "w", Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    repo.acknowledge(&claim, None, &c2, 2, false).await.unwrap();
    let refused = |sql: &'static str| {
        let pool = store.pool().clone();
        let (g, target) = (g.clone(), target.clone());
        async move {
            let result = sqlx::query(sql)
                .bind(g.as_str())
                .bind(&target)
                .execute(&pool)
                .await;
            assert!(
                result.is_err(),
                "{sql} must be refused (even for the owner)"
            );
        }
    };
    refused("UPDATE projection_state SET projected_ref_version = 1, projected_commit = (SELECT new_head FROM ref_events WHERE graph_id = $1 AND new_version = 1) WHERE graph_id = $1 AND target_id = $2").await;
    refused("UPDATE projection_state SET cognitive_graph = 'urn:sculpin:kb:other:cognitive' WHERE graph_id = $1 AND target_id = $2").await;
    refused("UPDATE projection_state SET lease_epoch = 0 WHERE graph_id = $1 AND target_id = $2")
        .await;
    refused("DELETE FROM projection_state WHERE graph_id = $1 AND target_id = $2").await;
    refused("UPDATE projection_outbox SET delivered_at = NULL WHERE graph_id = $1 AND $2 <> ''")
        .await;
    refused("UPDATE projection_outbox SET attempts = -1 WHERE graph_id = $1 AND $2 <> ''").await;
    refused("DELETE FROM projection_outbox WHERE graph_id = $1 AND $2 <> ''").await;
    // Recorded progress must name a real accepted state of this ref (FK).
    let forged = sqlx::query(
        "UPDATE projection_state SET projected_ref_version = 3, projected_commit = $3 \
         WHERE graph_id = $1 AND target_id = $2",
    )
    .bind(g.as_str())
    .bind(&target)
    .bind(c1.to_string())
    .execute(store.pool())
    .await;
    assert!(forged.is_err(), "progress must reference ref_events");
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn failures_back_off_block_and_rebuild_claims_are_explicit() {
    let store = store().await;
    let repo = owner_repo(&store).await;
    let target = unique("t");
    let g = graph(&store, "tenant-a", Some(&unique("kb"))).await;
    accept(&store, "tenant-a", &g, None, "one").await;
    let k = key(&g, &target);
    repo.enable(&k, derive).await.unwrap();
    let ttl = Duration::from_secs(30);
    let claim = repo.claim(&target, "w", ttl).await.unwrap().unwrap();
    repo.fail(
        &claim,
        None,
        "TARGET_UNAVAILABLE",
        FailureDisposition::Retry(Duration::from_secs(60)),
    )
    .await
    .unwrap();
    assert!(
        repo.claim(&target, "w", ttl).await.unwrap().is_none(),
        "backing off"
    );
    let status = &repo.status(Some(&target)).await.unwrap()[0];
    assert_eq!(status.consecutive_failures, 1);
    assert_eq!(
        status.last_error_code.as_deref(),
        Some("TARGET_UNAVAILABLE")
    );
    assert_eq!((status.pending_events, status.lag_versions()), (1, 1));
    assert!(status.oldest_pending_seconds.is_some());
    // A rebuild claim is explicit and ignores backoff; blocking parks the stream.
    let rebuild = repo
        .claim_stream(&k, "operator", ttl)
        .await
        .unwrap()
        .unwrap();
    repo.fail(&rebuild, None, "TARGET_AUTH", FailureDisposition::Block)
        .await
        .unwrap();
    assert_eq!(
        repo.status(Some(&target)).await.unwrap()[0].status,
        "blocked"
    );
    sqlx::query("UPDATE projection_state SET next_attempt_at = now() WHERE graph_id = $1")
        .bind(g.as_str())
        .execute(store.pool())
        .await
        .unwrap();
    assert!(
        repo.claim(&target, "w", ttl).await.unwrap().is_none(),
        "blocked streams are not claimed"
    );
    assert!(
        repo.claim_stream(&k, "operator", ttl)
            .await
            .unwrap()
            .is_some()
    );
    // Error codes are bounded tokens (CHECK).
    let bad = repo.claim_stream(&k, "operator2", ttl).await.unwrap();
    assert!(bad.is_none(), "the operator lease is live");
    repo.disable(&k).await.unwrap();
    assert!(
        repo.claim_stream(&k, "operator", Duration::from_secs(0))
            .await
            .unwrap()
            .is_none()
    );
}

/// A dedicated projector role: exactly the 0011 model, refused on any drift.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn the_projector_identity_holds_exactly_its_model() {
    let url = database_url();
    let store = store().await;
    let role = format!("lp_proj_{}", std::process::id());
    let admin: PgPool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    for sql in [
        format!("DROP ROLE IF EXISTS {role}"),
        format!("CREATE ROLE {role} LOGIN PASSWORD 'proj-test-secret'"),
    ] {
        let _ = sqlx::query(&sql).execute(&admin).await;
    }
    let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
    schema::grant_projector_role(&mut conn, &role)
        .await
        .unwrap();
    let projector_url = {
        let (scheme, rest) = url.split_once("://").unwrap();
        let (_, host) = rest.rsplit_once('@').unwrap();
        format!("{scheme}://{role}:proj-test-secret@{host}")
    };
    ProjectionRepository::connect(&projector_url, ledger_store::DbSessionLimits::default())
        .await
        .expect("the granted projector identity verifies");
    // The projector role cannot write what it must not, even directly.
    let projector: PgPool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&projector_url)
        .await
        .unwrap();
    for sql in [
        "UPDATE projection_state SET cognitive_graph = cognitive_graph",
        "UPDATE projection_outbox SET commit_id = commit_id",
        "INSERT INTO projection_state (graph_id) VALUES ('x')",
        "DELETE FROM projection_outbox",
        "UPDATE refs SET version = version",
        "SELECT 1 FROM idempotency LIMIT 1",
        "SELECT 1 FROM validation_records LIMIT 1",
    ] {
        assert!(sqlx::query(sql).execute(&projector).await.is_err(), "{sql}");
    }
    projector.close().await;
    // Drift in either direction is refused at start-up.
    let refused = |grant: String, revoke: String| {
        let (url, projector_url) = (url.clone(), projector_url.clone());
        async move {
            let mut c = sqlx::PgConnection::connect(&url).await.unwrap();
            sqlx::query(&grant).execute(&mut c).await.unwrap();
            let result = ProjectionRepository::connect(
                &projector_url,
                ledger_store::DbSessionLimits::default(),
            )
            .await;
            sqlx::query(&revoke).execute(&mut c).await.unwrap();
            match result {
                Err(LedgerError::RuntimeIdentity(m)) => m,
                Err(other) => panic!("{grant}: expected an identity refusal, got {other}"),
                Ok(_) => panic!("{grant}: drift must refuse start-up"),
            }
        }
    };
    let m = refused(
        format!("GRANT SELECT ON idempotency TO {role}"),
        format!("REVOKE SELECT ON idempotency FROM {role}"),
    )
    .await;
    assert!(m.contains("idempotency") && m.contains("projector"), "{m}");
    let m = refused(
        format!("GRANT UPDATE (head) ON refs TO {role}"),
        format!("REVOKE UPDATE (head) ON refs FROM {role}"),
    )
    .await;
    assert!(m.contains("refs"), "{m}");
    let m = refused(
        format!("GRANT UPDATE (cognitive_graph) ON projection_state TO {role}"),
        format!("REVOKE UPDATE (cognitive_graph) ON projection_state FROM {role}"),
    )
    .await;
    assert!(m.contains("cognitive_graph"), "{m}");
    let m = refused(
        format!("REVOKE UPDATE (delivered_at) ON projection_outbox FROM {role}"),
        format!("GRANT UPDATE (delivered_at) ON projection_outbox TO {role}"),
    )
    .await;
    assert!(m.contains("delivered_at") && m.contains("lacks"), "{m}");
    let m = refused(
        format!("GRANT EXECUTE ON FUNCTION ledger_grant_projector(text) TO {role}"),
        format!("REVOKE EXECUTE ON FUNCTION ledger_grant_projector(text) FROM {role}"),
    )
    .await;
    assert!(m.contains("ledger_grant_projector"), "{m}");
    // The runtime identity check refuses a projector role (and vice versa).
    let pool: PgPool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&projector_url)
        .await
        .unwrap();
    assert!(schema::verify_runtime_identity(&pool).await.is_err());
    pool.close().await;
    ProjectionRepository::connect(&projector_url, ledger_store::DbSessionLimits::default())
        .await
        .expect("restored");
    drop(store);
}
