//! ADR-0016: the runtime identity can run the whole public workflow but cannot change the
//! schema, disable integrity controls, rewrite immutable or audit rows, provision graphs
//! or run migrations; the owner identity migrates and grants; startup/readiness refuse a
//! database whose schema level is not exactly the one this build requires.
//!
//! Requires PostgreSQL with an owner/superuser `LEDGER_TEST_DATABASE_URL` (the compose
//! harness's `ledger` user); the runtime role is created per test and dropped afterwards.
#![cfg(feature = "postgres")]

use ledger_core::{
    AuthenticatedPrincipal, GraphId, LedgerError, PrincipalId, PrincipalType, TenantId,
};
use ledger_rdf::{Operation, OperationKind, Patch, Quad};
use ledger_store::{
    AcceptRequest, DbSessionLimits, GraphStatus, NewGraph, PgGraphs, PostgresLedgerStore,
    PrepareRequest, RejectRequest, RequestScope, V1Binding, ValidateRequest, ValidationBegin,
    ValidationPolicy, ValidatorOutcome, schema,
};
use sqlx::{Connection, PgConnection, PgPool, Row, postgres::PgPoolOptions};
use std::{
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn owner_url() -> String {
    std::env::var("LEDGER_TEST_DATABASE_URL").expect("LEDGER_TEST_DATABASE_URL must be set")
}

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos % 1_000_000_000)
}

fn with_database(base: &str, db: &str) -> String {
    let (head, query) = base.split_once('?').map_or((base, ""), |(h, q)| (h, q));
    let slash = head.rfind('/').expect("database url has a path");
    if query.is_empty() {
        format!("{}/{db}", &head[..slash])
    } else {
        format!("{}/{db}?{query}", &head[..slash])
    }
}

fn with_credentials(url: &str, user: &str, password: &str) -> String {
    // postgres://user:pass@host/... → replace the userinfo.
    let (scheme, rest) = url.split_once("://").unwrap();
    let (_, host_part) = rest.rsplit_once('@').unwrap();
    format!("{scheme}://{user}:{password}@{host_part}")
}

/// A throwaway database, a throwaway runtime role, and the owner pool on that database.
struct Fixture {
    db: String,
    role: String,
    owner_db_url: String,
    runtime_db_url: String,
    owner: PgPool,
    admin: PgPool,
}

async fn fixture(prefix: &str) -> Fixture {
    let base = owner_url();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    let db = unique(prefix);
    let role = unique("ledger_rt");
    sqlx::query(&format!("CREATE DATABASE {db}"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'rt-test-secret'"
    ))
    .execute(&admin)
    .await
    .unwrap();
    let owner_db_url = with_database(&base, &db);
    let runtime_db_url = with_credentials(&owner_db_url, &role, "rt-test-secret");
    let owner = PgPoolOptions::new()
        .max_connections(4)
        .connect(&owner_db_url)
        .await
        .unwrap();
    Fixture {
        db,
        role,
        owner_db_url,
        runtime_db_url,
        owner,
        admin,
    }
}

impl Fixture {
    /// The production sequence: owner migrates on a dedicated connection, grants the role.
    async fn migrate_and_grant(&self) {
        let mut conn = PgConnection::connect(&self.owner_db_url).await.unwrap();
        schema::migrate_all_on(&mut conn).await.unwrap();
        schema::grant_runtime_role(&mut conn, &self.role)
            .await
            .unwrap();
        conn.close().await.unwrap();
    }
    async fn runtime_pool(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&self.runtime_db_url)
            .await
            .unwrap()
    }
    async fn teardown(self) {
        self.owner.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.db))
            .execute(&self.admin)
            .await
            .unwrap();
        sqlx::query(&format!("DROP ROLE {}", self.role))
            .execute(&self.admin)
            .await
            .unwrap();
    }
}

fn principal(id: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal {
        principal_id: PrincipalId::new(format!("urn:sculpin:agent:{id}")).unwrap(),
        principal_type: PrincipalType::Agent,
        tenant_id: TenantId::new("tenant-lp").unwrap(),
        on_behalf_of: None,
    }
}

fn scope(graph: &GraphId, key: &str, digest: &[u8]) -> RequestScope {
    RequestScope {
        principal: principal("runtime-actor"),
        graph: graph.clone(),
        idempotency_key: key.into(),
        request_digest: ledger_core::ContentId::for_bytes(digest),
        correlation_id: Some(format!("corr-{key}")),
    }
}

fn patch(quad: &str, kind: OperationKind) -> Patch {
    Patch::new([Operation {
        kind,
        quad: Quad::from_str(quad).unwrap(),
    }])
    .unwrap()
}

/// Expect PostgreSQL to refuse with 42501 (insufficient_privilege).
async fn assert_denied(pool: &PgPool, sql: &str) {
    match sqlx::query(sql).execute(pool).await {
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42501") => {}
        other => panic!("{sql:?} must be refused with 42501, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn runtime_identity_serves_the_workflow_but_cannot_touch_schema_or_history() {
    let f = fixture("lp_workflow").await;
    f.migrate_and_grant().await;
    // Owner provisions the graph (the runtime cannot).
    let g = GraphId::new(unique("g").replace('_', "-")).unwrap();
    PgGraphs::new(f.owner.clone())
        .create(&NewGraph {
            graph_id: g.clone(),
            tenant_id: TenantId::new("tenant-lp").unwrap(),
            knowledge_base_id: None,
            purpose: None,
            status: GraphStatus::Active,
        })
        .await
        .unwrap();

    // The server's constructor: verify-only, runtime identity, session limits applied.
    let store = PostgresLedgerStore::connect_with(
        &f.runtime_db_url,
        V1Binding::Reject,
        DbSessionLimits::default(),
    )
    .await
    .expect("runtime identity connects and verifies the schema");
    store
        .ready()
        .await
        .expect("ready under the runtime identity");
    let (timeout,): (String,) = sqlx::query_as("SHOW statement_timeout")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(timeout, "30s", "session limit applied on connect");
    let (idle,): (String,) = sqlx::query_as("SHOW idle_in_transaction_session_timeout")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(idle, "1min");

    // Every public operation: prepare, replay, accept, replay, second prepare, reject.
    let wf = store.workflows();
    let p1 = wf
        .prepare(&PrepareRequest {
            scope: scope(&g, "k1", b"p1"),
            branch: "main".into(),
            expected_head: None,
            requested: patch("<urn:s> <urn:p> \"1\" .", OperationKind::Add),
            activity: "a".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: "m".into(),
        })
        .await
        .expect("prepare under the runtime identity");
    let replay = wf
        .prepare(&PrepareRequest {
            scope: scope(&g, "k1", b"p1"),
            branch: "main".into(),
            expected_head: None,
            requested: patch("<urn:s> <urn:p> \"1\" .", OperationKind::Add),
            activity: "a".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: "m".into(),
        })
        .await
        .unwrap();
    assert!(replay.replayed);
    let a1 = wf
        .accept(&AcceptRequest {
            scope: scope(&g, "k2", b"a1"),
            branch: "main".into(),
            expected_head: None,
            candidate: p1.candidate.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await
        .expect("accept under the runtime identity");
    assert_eq!(a1.ref_version, 1);
    let p2 = wf
        .prepare(&PrepareRequest {
            scope: scope(&g, "k3", b"p2"),
            branch: "main".into(),
            expected_head: Some(p1.candidate.clone()),
            requested: patch("<urn:s> <urn:p> \"2\" .", OperationKind::Add),
            activity: "a".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: "m".into(),
        })
        .await
        .unwrap();
    wf.reject(&RejectRequest {
        scope: scope(&g, "k4", b"r2"),
        branch: "main".into(),
        candidate: p2.candidate.clone(),
        reason: "no".into(),
        validation_id: None,
    })
    .await
    .expect("reject under the runtime identity");
    // Public reads.
    assert_eq!(
        store.ref_head(&g, "main").await.unwrap(),
        Some((p1.candidate.clone(), 1))
    );
    assert_eq!(
        store.commit_graph(&p1.candidate).await.unwrap(),
        Some(g.clone())
    );
    let state = wf
        .reconstruct(
            &p1.candidate,
            &ledger_store::ReconstructionLimits::DEVELOPMENT,
        )
        .await
        .unwrap();
    assert_eq!(state.len(), 1);
    // mark_superseded (operator path) also works without a row lock on proposals.
    let p3 = wf
        .prepare(&PrepareRequest {
            scope: scope(&g, "k5", b"p3"),
            branch: "main".into(),
            expected_head: Some(p1.candidate.clone()),
            requested: patch("<urn:s> <urn:p> \"3\" .", OperationKind::Add),
            activity: "a".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: "m".into(),
        })
        .await
        .unwrap();
    let p4 = wf
        .prepare(&PrepareRequest {
            scope: scope(&g, "k6", b"p4"),
            branch: "main".into(),
            expected_head: Some(p1.candidate.clone()),
            requested: patch("<urn:s> <urn:p> \"4\" .", OperationKind::Add),
            activity: "a".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: "m".into(),
        })
        .await
        .unwrap();
    wf.accept(&AcceptRequest {
        scope: scope(&g, "k7", b"a3"),
        branch: "main".into(),
        expected_head: Some(p1.candidate.clone()),
        candidate: p3.candidate.clone(),
        reason: None,
        validation: ValidationPolicy::NoValidation,
    })
    .await
    .unwrap();
    wf.mark_superseded(
        &principal("runtime-actor"),
        &g,
        p4.proposal_id,
        "stale",
        None,
    )
    .await
    .expect("supersede under the runtime identity");

    // Proof that the workflow above ran as a non-superuser, non-owner role.
    let (who, is_super): (String, bool) = sqlx::query_as(
        "SELECT current_user::text, (SELECT rolsuper FROM pg_roles WHERE rolname = current_user)",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(who, f.role);
    assert!(!is_super);
    // Snapshot every table and the ref before the denied statements; nothing may change.
    let snapshot = |pool: PgPool| async move {
        let mut out = Vec::new();
        for table in [
            "immutable_objects",
            "commit_index",
            "commit_parents",
            "graphs",
            "refs",
            "proposals",
            "ref_events",
            "decisions",
            "projection_outbox",
            "idempotency",
            "_sqlx_migrations",
            "semantic_execution_contexts",
            "semantic_virtual_contexts",
            "validation_records",
            "validation_violations",
            "decision_validations",
        ] {
            let (n,): (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap();
            out.push((table, n));
        }
        let (head, version): (String, i64) =
            sqlx::query_as("SELECT head, version FROM refs ORDER BY graph_id, branch LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        (out, head, version)
    };
    let before = snapshot(f.owner.clone()).await;
    // The runtime identity cannot change the schema, disable controls or rewrite history.
    let rt = store.pool();
    for sql in [
        "ALTER TABLE immutable_objects ADD COLUMN x int",
        "ALTER TABLE immutable_objects DISABLE TRIGGER immutable_objects_write_once",
        "ALTER TABLE idempotency DISABLE TRIGGER idempotency_write_once",
        "DROP TABLE decisions",
        "TRUNCATE ref_events",
        "CREATE TABLE smuggled (x int)",
        "UPDATE immutable_objects SET bytes = bytes",
        "DELETE FROM immutable_objects",
        "UPDATE commit_index SET version = version",
        "DELETE FROM commit_index",
        "UPDATE commit_parents SET position = position",
        "DELETE FROM commit_parents",
        "UPDATE proposals SET branch = branch",
        "DELETE FROM proposals",
        "UPDATE ref_events SET operation = operation",
        "DELETE FROM ref_events",
        "UPDATE decisions SET reason = 'x'",
        "DELETE FROM decisions",
        "UPDATE idempotency SET request_digest = request_digest",
        "DELETE FROM idempotency",
        "UPDATE projection_outbox SET attempts = attempts",
        "DELETE FROM projection_outbox",
        "INSERT INTO graphs (graph_id, tenant_id, status) VALUES ('smuggled', 't', 'active')",
        "UPDATE graphs SET status = 'archived'",
        "DELETE FROM graphs",
        "UPDATE refs SET graph_id = graph_id",
        "UPDATE refs SET protected = protected",
        "DELETE FROM refs",
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (9999, 'x', true, '\\x00', 0)",
        "DELETE FROM _sqlx_migrations",
        "DROP FUNCTION ledger_rows_are_write_once()",
        "SELECT ledger_grant_runtime('public')",
        "CREATE ROLE smuggled_role",
        // Phase 2 tables (0010): insert-only for the runtime, never rewrite or delete.
        "UPDATE semantic_execution_contexts SET base_kb_revision = 'x'",
        "DELETE FROM semantic_execution_contexts",
        "UPDATE semantic_virtual_contexts SET source_version = 'x'",
        "DELETE FROM semantic_virtual_contexts",
        "UPDATE validation_records SET outcome = 'conforms', violation_count = 0",
        "DELETE FROM validation_records",
        "UPDATE validation_violations SET message = 'x'",
        "DELETE FROM validation_violations",
        "UPDATE decision_validations SET validation_id = validation_id",
        "DELETE FROM decision_validations",
        "ALTER TABLE validation_records DISABLE TRIGGER validation_records_write_once",
        "DROP TABLE decision_validations",
        // Phase 5 (0013): merge rows are insert-only for the runtime.
        "UPDATE merge_proposals SET strategy = 'union'",
        "DELETE FROM merge_proposals",
        "ALTER TABLE merge_proposals DISABLE TRIGGER merge_proposals_lineage",
        "ALTER TABLE ref_events DISABLE TRIGGER ref_events_kind",
        "DROP TRIGGER decisions_merge_kind ON decisions",
    ] {
        assert_denied(rt, sql).await;
    }
    for sql in [
        "SET session_replication_role = replica",
        "CREATE FUNCTION smuggled() RETURNS int LANGUAGE sql AS 'SELECT 1'",
        "SELECT setval('proposals_proposal_id_seq', 1)",
        // `protected` is insertable since 0012 (branch creation, ADR-0022); back-dating a ref
        // or a lifecycle event is not.
        "INSERT INTO refs (graph_id, branch, head, version, updated_at) VALUES ('x', 'y', 'z', 1, now())",
        "INSERT INTO branch_events (graph_id, branch, tenant_id, lifecycle_version, operation, status_after, head, ref_version, principal_id, principal_type, recorded_at) VALUES ('x','y','t',2,'deleted','deleted','z',1,'p','agent', now())",
        "INSERT INTO projection_outbox (graph_id, branch, commit_id, ref_version, event_kind, ref_event_id, delivered_at) VALUES ('x','y','z',1,'ref_advanced',1, now())",
        // Back-dating a merge row is not granted (0013).
        "INSERT INTO merge_proposals (proposal_id, graph_id, target_branch, candidate_commit, target_head, source_branch, source_head, merge_base, base_explicit, classification, strategy, merge_algorithm, conflict_count, merged_state_digest, preview_token, source_parties, created_at) VALUES (1,'x','y','z','t','s','h','b',false,'divergent','abort','structural-slot/v1',0,'d','k','{}', now())",
        "INSERT INTO decisions (proposal_id, graph_id, branch, candidate_commit, decision, tenant_id, principal_id, principal_type, reason, validation_ids, decided_at) VALUES (1,'x','y','z','rejected','t','p','agent','r','{}', now())",
        // Back-dating the audit timestamp of a validation record or context is not granted.
        "INSERT INTO validation_records (validation_id, graph_id, tenant_id, candidate_commit, candidate_state_digest, context_id, validator_service_id, validator_service_version, validator_configuration_version, outcome, violation_count, report_digest, recorded_at, principal_id, principal_type, canonical_bytes, created_at) VALUES ('x','y','t','z','d','c','s','v','c','conforms',0,'r',now(),'p','agent','\\x00', now())",
        "INSERT INTO semantic_execution_contexts (context_id, graph_id, tenant_id, candidate_commit, candidate_state_digest, base_kb_id, base_kb_revision, shapes_id, shapes_version, reasoning_profile, reasoning_implementation, reasoning_version, validator_service_id, validator_service_version, validator_configuration_version, virtual_context_count, canonical_bytes, created_at) VALUES ('x','y','t','z','d','k','r','s','v','p','i','v','s','v','c',0,'\\x00', now())",
    ] {
        assert_denied(rt, sql).await;
    }
    assert_eq!(
        snapshot(f.owner.clone()).await,
        before,
        "denied statements must change nothing"
    );
    // Running the migrations as the runtime identity is refused by PostgreSQL itself, even
    // when a migration is pending: the owner removes the last migration's record so the
    // runtime would have something to apply.
    let (checksum10,): (Vec<u8>,) =
        sqlx::query_as("SELECT checksum FROM _sqlx_migrations WHERE version = 10")
            .fetch_one(&f.owner)
            .await
            .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 10")
        .execute(&f.owner)
        .await
        .unwrap();
    let err = schema::migrate_all(rt).await.unwrap_err();
    assert!(
        matches!(&err, LedgerError::Storage(m) if m.contains("permission denied") || m.contains("must be owner")),
        "{err:?}"
    );
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM _sqlx_migrations WHERE version = 10")
        .fetch_one(&f.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "the runtime must not have applied anything");
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (10, 'semantic validation', true, $1, 0)",
    )
    .bind(&checksum10)
    .execute(&f.owner)
    .await
    .unwrap();
    // A lock wait that exceeds the session limit surfaces through the repository as a
    // retryable DependencyTimeout and leaves no rows behind; the same request then succeeds.
    let short_lock = PostgresLedgerStore::connect_with(
        &f.runtime_db_url,
        V1Binding::Reject,
        DbSessionLimits {
            lock_timeout: Duration::from_millis(200),
            ..DbSessionLimits::default()
        },
    )
    .await
    .unwrap();
    let mut holder = f.owner.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ledger_store::lock_key(&format!("graph-status:{g}")))
        .execute(&mut *holder)
        .await
        .unwrap();
    let blocked = PrepareRequest {
        scope: scope(&g, "k-lock", b"lock"),
        branch: "main".into(),
        expected_head: Some(p3.candidate.clone()),
        requested: patch("<urn:s> <urn:p> \"lock\" .", OperationKind::Add),
        activity: "a".into(),
        event_time: None,
        evidence_refs: vec![],
        source_system: None,
        message: "m".into(),
    };
    let err = short_lock.workflows().prepare(&blocked).await.unwrap_err();
    assert!(matches!(err, LedgerError::DependencyTimeout(_)), "{err:?}");
    let (pending,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM idempotency WHERE idempotency_key = 'k-lock'")
            .fetch_one(&f.owner)
            .await
            .unwrap();
    assert_eq!(pending, 0, "a timed-out request records nothing");
    holder.rollback().await.unwrap();
    let retried = short_lock.workflows().prepare(&blocked).await.unwrap();
    assert!(!retried.replayed, "the retry runs afresh");
    // An idle transaction is terminated by the session limit (25P03 or a closed connection);
    // the pool recovers.
    let short_idle = PostgresLedgerStore::connect_with(
        &f.runtime_db_url,
        V1Binding::Reject,
        DbSessionLimits {
            idle_in_transaction_timeout: Duration::from_millis(200),
            ..DbSessionLimits::default()
        },
    )
    .await
    .unwrap();
    let mut idle = short_idle.pool().begin().await.unwrap();
    sqlx::query("SELECT 1").execute(&mut *idle).await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let after = sqlx::query("SELECT 1").execute(&mut *idle).await;
    match after {
        Err(sqlx::Error::Database(ref e)) => {
            let code = e.code().map(|c| c.to_string()).unwrap_or_default();
            assert!(ledger_store::sqlstate_is_unavailable(&code), "{code}");
        }
        Err(sqlx::Error::Io(_)) | Err(sqlx::Error::WorkerCrashed) => {}
        other => panic!("idle transaction must be terminated, got {other:?}"),
    }
    drop(idle);
    sqlx::query("SELECT 1")
        .execute(short_idle.pool())
        .await
        .unwrap();
    // A statement that exceeds the session limit is a retryable timeout, not a fault.
    let short = PostgresLedgerStore::connect_with(
        &f.runtime_db_url,
        V1Binding::Reject,
        DbSessionLimits {
            statement_timeout: Duration::from_millis(200),
            ..DbSessionLimits::default()
        },
    )
    .await
    .unwrap();
    let slow = sqlx::query("SELECT pg_sleep(2)")
        .execute(short.pool())
        .await;
    let code = match slow {
        Err(sqlx::Error::Database(ref e)) => e.code().map(|c| c.to_string()),
        other => panic!("expected statement_timeout, got {other:?}"),
    };
    assert_eq!(code.as_deref(), Some("57014"));
    assert!(ledger_store::sqlstate_is_timeout("57014"));
    f.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn startup_and_readiness_refuse_any_schema_level_but_the_required_one() {
    // Never migrated: metadata absent.
    let f = fixture("lp_schema").await;
    let err = PostgresLedgerStore::connect(&f.owner_db_url, V1Binding::Reject)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&err, LedgerError::SchemaIncompatible(m) if m.contains("absent")),
        "{err}"
    );
    // Behind: migrated to 0007 only.
    schema::migrate_up_to(&f.owner, 7).await.unwrap();
    let err = PostgresLedgerStore::connect(&f.owner_db_url, V1Binding::Reject)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&err, LedgerError::SchemaIncompatible(m) if m.contains("0007")
            && m.contains(&format!("requires {:04}", schema::REQUIRED_SCHEMA_VERSION))),
        "{err}"
    );
    // Exactly right: connects; then readiness follows the schema level live.
    f.migrate_and_grant().await;
    let store = PostgresLedgerStore::connect(&f.runtime_db_url, V1Binding::Reject)
        .await
        .unwrap();
    store.ready().await.unwrap();
    assert_eq!(
        schema::verify(&f.owner).await.unwrap().version,
        schema::REQUIRED_SCHEMA_VERSION
    );
    // Ahead: a future migration recorded by a newer build → refused with an actionable message.
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9999, 'from the future', true, '\\x00', 0)",
    )
    .execute(&f.owner)
    .await
    .unwrap();
    let err = store.ready().await.unwrap_err();
    assert!(
        matches!(&err, LedgerError::SchemaIncompatible(m) if m.contains("newer")),
        "{err}"
    );
    let err = PostgresLedgerStore::connect(&f.runtime_db_url, V1Binding::Reject)
        .await
        .err()
        .unwrap();
    assert!(matches!(err, LedgerError::SchemaIncompatible(_)));
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 9999")
        .execute(&f.owner)
        .await
        .unwrap();
    // Corrupt: a recorded migration whose checksum differs from the embedded one.
    let (checksum8,): (Vec<u8>,) =
        sqlx::query_as("SELECT checksum FROM _sqlx_migrations WHERE version = 8")
            .fetch_one(&f.owner)
            .await
            .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = 8")
        .execute(&f.owner)
        .await
        .unwrap();
    let err = store.ready().await.unwrap_err();
    assert!(
        matches!(&err, LedgerError::SchemaIncompatible(m) if m.contains("different contents")),
        "{err}"
    );
    // A failed migration record is refused on its own (checksum restored first).
    sqlx::query("UPDATE _sqlx_migrations SET checksum = $1, success = false WHERE version = 8")
        .bind(&checksum8)
        .execute(&f.owner)
        .await
        .unwrap();
    let err = store.ready().await.unwrap_err();
    assert!(
        matches!(&err, LedgerError::SchemaIncompatible(m) if m.contains("recorded as failed")),
        "{err}"
    );
    sqlx::query("UPDATE _sqlx_migrations SET success = true WHERE version = 8")
        .execute(&f.owner)
        .await
        .unwrap();
    // A gap in the history is refused even though the highest version is right.
    let (checksum7,): (Vec<u8>,) =
        sqlx::query_as("SELECT checksum FROM _sqlx_migrations WHERE version = 7")
            .fetch_one(&f.owner)
            .await
            .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 7")
        .execute(&f.owner)
        .await
        .unwrap();
    let err = store.ready().await.unwrap_err();
    assert!(
        matches!(&err, LedgerError::SchemaIncompatible(m) if m.contains("not contiguous")),
        "{err}"
    );
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (7, 'actor scope', true, $1, 0)",
    )
    .bind(&checksum7)
    .execute(&f.owner)
    .await
    .unwrap();
    store.ready().await.unwrap();
    // A disabled integrity trigger is not a compatible schema.
    sqlx::query("ALTER TABLE immutable_objects DISABLE TRIGGER immutable_objects_write_once")
        .execute(&f.owner)
        .await
        .unwrap();
    let err = store.ready().await.unwrap_err();
    assert!(
        matches!(&err, LedgerError::SchemaIncompatible(m) if m.contains("immutable_objects_write_once") && m.contains("disabled")),
        "{err}"
    );
    sqlx::query("ALTER TABLE immutable_objects ENABLE TRIGGER immutable_objects_write_once")
        .execute(&f.owner)
        .await
        .unwrap();
    store.ready().await.unwrap();
    // The owner identity is refused as a runtime identity even on the right schema.
    let err = PostgresLedgerStore::connect(&f.owner_db_url, V1Binding::Reject)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&err, LedgerError::RuntimeIdentity(m) if m.contains("superuser") || m.contains("owns")),
        "{err}"
    );
    f.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn grant_function_is_owner_only_idempotent_and_refuses_unknown_roles() {
    let f = fixture("lp_grant").await;
    f.migrate_and_grant().await;
    // Re-granting is idempotent.
    let mut conn = PgConnection::connect(&f.owner_db_url).await.unwrap();
    schema::grant_runtime_role(&mut conn, &f.role)
        .await
        .unwrap();
    // Unknown role: refused by the function with a clear message.
    let err = schema::grant_runtime_role(&mut conn, "no_such_role_xyz")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
    conn.close().await.unwrap();
    // The runtime role cannot execute it (EXECUTE revoked from PUBLIC).
    let rt = f.runtime_pool().await;
    assert_denied(&rt, &format!("SELECT ledger_grant_runtime('{}')", f.role)).await;
    // Exactly the documented privilege matrix, evaluated the way PostgreSQL will (includes
    // PUBLIC and membership): SELECT everywhere, column-limited INSERT where the store
    // inserts, column-limited UPDATE on refs, nothing else.
    let rt = f.runtime_pool().await;
    let tables = [
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
        "_sqlx_migrations",
        "semantic_execution_contexts",
        "semantic_virtual_contexts",
        "validation_records",
        "validation_violations",
        "decision_validations",
    ];
    let insertable = [
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
    for table in tables {
        for privilege in [
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
            "TRIGGER",
        ] {
            // has_any_column_privilege knows only SELECT/INSERT/UPDATE/REFERENCES.
            let (table_level, any_column): (bool, bool) = sqlx::query_as(
                "SELECT has_table_privilege($1, $2), \
                        CASE WHEN $2 IN ('SELECT', 'INSERT', 'UPDATE', 'REFERENCES') \
                             THEN has_any_column_privilege($1, $2) ELSE false END",
            )
            .bind(table)
            .bind(privilege)
            .fetch_one(&rt)
            .await
            .unwrap();
            let expected_table = privilege == "SELECT";
            let expected_column = match privilege {
                "SELECT" => true,
                "INSERT" => insertable.contains(&table),
                "UPDATE" => table == "refs",
                _ => false,
            };
            assert_eq!(
                (table_level, any_column),
                (expected_table, expected_column),
                "{table} {privilege}"
            );
        }
    }
    // Column-level INSERT excludes ids, timestamps, `protected` and `delivered_at`.
    let cols = sqlx::query(
        "SELECT table_name, column_name FROM information_schema.column_privileges \
         WHERE grantee = $1 AND privilege_type = 'INSERT' ORDER BY table_name, column_name",
    )
    .bind(&f.role)
    .fetch_all(&f.owner)
    .await
    .unwrap();
    let granted: Vec<String> = cols
        .iter()
        .map(|r| {
            format!(
                "{}.{}",
                r.get::<String, _>("table_name"),
                r.get::<String, _>("column_name")
            )
        })
        .collect();
    for absent in [
        "refs.created_at",
        "refs.updated_at",
        "branches.created_at",
        "branches.updated_at",
        "branch_events.event_id",
        "branch_events.recorded_at",
        "projection_outbox.delivered_at",
        "projection_outbox.attempts",
        "projection_outbox.outbox_id",
        "proposals.proposal_id",
        "proposals.created_at",
        "decisions.decision_id",
        "decisions.decided_at",
        "ref_events.event_id",
        "ref_events.recorded_at",
        "idempotency.idempotency_id",
        "idempotency.created_at",
        "semantic_execution_contexts.created_at",
        "validation_records.created_at",
    ] {
        assert!(
            !granted.contains(&absent.to_owned()),
            "{absent} must not be insertable"
        );
    }
    for present in [
        "refs.head",
        "refs.version",
        // ADR-0022: branch creation records protection; `main` stays protected by CHECK.
        "refs.protected",
        "branches.status",
        "branch_events.operation",
        "idempotency.result_branch_event_id",
        "proposals.candidate_commit",
        "decisions.ref_event_id",
        "idempotency.request_digest",
        "idempotency.result_validation_id",
        "immutable_objects.bytes",
    ] {
        assert!(
            granted.contains(&present.to_owned()),
            "{present} must be insertable"
        );
    }
    // Phase-2 tables (0010): INSERT exactly on the columns the store's statements name.
    let phase2_insert: [(&str, &[&str]); 5] = [
        (
            "semantic_execution_contexts",
            &[
                "context_id",
                "graph_id",
                "tenant_id",
                "candidate_commit",
                "candidate_state_digest",
                "base_kb_id",
                "base_kb_revision",
                "ontology_id",
                "ontology_version",
                "shapes_id",
                "shapes_version",
                "reasoning_profile",
                "reasoning_implementation",
                "reasoning_version",
                "sources_revision",
                "validator_service_id",
                "validator_service_version",
                "validator_configuration_version",
                "virtual_context_count",
                "canonical_bytes",
            ],
        ),
        (
            "semantic_virtual_contexts",
            &[
                "context_id",
                "position",
                "dataset_id",
                "source_version",
                "object_refs",
                "query_spec_digest",
                "hydration_plan_digest",
            ],
        ),
        (
            "validation_records",
            &[
                "validation_id",
                "graph_id",
                "tenant_id",
                "candidate_commit",
                "candidate_state_digest",
                "context_id",
                "validator_service_id",
                "validator_service_version",
                "validator_configuration_version",
                "outcome",
                "violation_count",
                "report_digest",
                "report_reference",
                "recorded_at",
                "principal_id",
                "principal_type",
                "on_behalf_of",
                "correlation_id",
                "canonical_bytes",
            ],
        ),
        (
            "validation_violations",
            &["validation_id", "position", "severity", "code", "message"],
        ),
        (
            "decision_validations",
            &[
                "decision_id",
                "validation_id",
                "graph_id",
                "candidate_commit",
            ],
        ),
    ];
    for (table, columns) in phase2_insert {
        let mut expected: Vec<String> = columns.iter().map(|c| format!("{table}.{c}")).collect();
        expected.sort();
        let mut actual: Vec<String> = granted
            .iter()
            .filter(|c| c.starts_with(&format!("{table}.")))
            .cloned()
            .collect();
        actual.sort();
        assert_eq!(actual, expected, "{table}: INSERT column set");
        // Per column, through the runtime identity: INSERT only where listed, never UPDATE
        // or REFERENCES on any column (write-once, no FK targets for the runtime).
        let all: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = $1 ORDER BY column_name",
        )
        .bind(table)
        .fetch_all(&f.owner)
        .await
        .unwrap();
        for column in columns.iter() {
            assert!(all.iter().any(|c| c == column), "{table}.{column} exists");
        }
        for column in &all {
            let (select, insert, update, references): (bool, bool, bool, bool) = sqlx::query_as(
                "SELECT has_column_privilege($1, $2, 'SELECT'), has_column_privilege($1, $2, 'INSERT'), \
                        has_column_privilege($1, $2, 'UPDATE'), has_column_privilege($1, $2, 'REFERENCES')",
            )
            .bind(table)
            .bind(column)
            .fetch_one(&rt)
            .await
            .unwrap();
            assert_eq!(
                (select, insert, update, references),
                (true, columns.contains(&column.as_str()), false, false),
                "{table}.{column}"
            );
        }
    }
    // idempotency gained exactly one insertable column in 0010; it is not updatable.
    let (insert, update): (bool, bool) = sqlx::query_as(
        "SELECT has_column_privilege('idempotency', 'result_validation_id', 'INSERT'), \
                has_column_privilege('idempotency', 'result_validation_id', 'UPDATE')",
    )
    .fetch_one(&rt)
    .await
    .unwrap();
    assert_eq!((insert, update), (true, false));
    // UPDATE on refs is column-limited to what the workflow changes.
    let cols = sqlx::query(
        "SELECT column_name FROM information_schema.column_privileges \
         WHERE grantee = $1 AND table_name = 'refs' AND privilege_type = 'UPDATE' ORDER BY column_name",
    )
    .bind(&f.role)
    .fetch_all(&f.owner)
    .await
    .unwrap();
    let cols: Vec<String> = cols
        .iter()
        .map(|r| r.get::<String, _>("column_name"))
        .collect();
    assert_eq!(cols, ["head", "updated_at", "version"]);
    // Sequences: USAGE only (no setval); schema: USAGE without CREATE.
    let (usage, update, create): (bool, bool, bool) = sqlx::query_as(
        "SELECT has_sequence_privilege('proposals_proposal_id_seq', 'USAGE'), \
                has_sequence_privilege('proposals_proposal_id_seq', 'UPDATE'), \
                has_schema_privilege('public', 'CREATE')",
    )
    .fetch_one(&rt)
    .await
    .unwrap();
    assert!(usage && !update && !create);
    // A superuser or a role with CREATE on the schema is refused by the grant function.
    let mut conn = PgConnection::connect(&f.owner_db_url).await.unwrap();
    let err = schema::grant_runtime_role(&mut conn, "ledger")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("superuser"), "{err}");
    let creator = unique("ledger_creator");
    sqlx::query(&format!("CREATE ROLE {creator} LOGIN PASSWORD 'x'"))
        .execute(&f.admin)
        .await
        .unwrap();
    sqlx::query(&format!("GRANT CREATE ON SCHEMA public TO {creator}"))
        .execute(&f.owner)
        .await
        .unwrap();
    let err = schema::grant_runtime_role(&mut conn, &creator)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("CREATE"), "{err}");
    sqlx::query(&format!("REVOKE CREATE ON SCHEMA public FROM {creator}"))
        .execute(&f.owner)
        .await
        .unwrap();
    sqlx::query(&format!("DROP ROLE {creator}"))
        .execute(&f.admin)
        .await
        .unwrap();
    conn.close().await.unwrap();
    f.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn a_non_superuser_owner_can_migrate_grant_and_provision() {
    // Managed-PostgreSQL shape: the schema owner is an ordinary login role that owns the
    // database, has no CREATEROLE and is not a superuser (ADR-0016).
    let base = owner_url();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    let owner_role = unique("ledger_owner");
    let runtime_role = unique("ledger_rt");
    let db = unique("lp_managed");
    sqlx::query(&format!(
        "CREATE ROLE {owner_role} LOGIN PASSWORD 'owner-test-secret' NOSUPERUSER NOCREATEROLE NOCREATEDB"
    ))
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE ROLE {runtime_role} LOGIN PASSWORD 'rt-test-secret'"
    ))
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query(&format!("CREATE DATABASE {db} OWNER {owner_role}"))
        .execute(&admin)
        .await
        .unwrap();
    let owner_db_url =
        with_credentials(&with_database(&base, &db), &owner_role, "owner-test-secret");
    let runtime_db_url =
        with_credentials(&with_database(&base, &db), &runtime_role, "rt-test-secret");
    // PostgreSQL 15+: the database owner owns schema public by default.
    let mut conn = PgConnection::connect(&owner_db_url).await.unwrap();
    schema::migrate_all_on(&mut conn).await.unwrap();
    schema::grant_runtime_role(&mut conn, &runtime_role)
        .await
        .unwrap();
    conn.close().await.unwrap();
    let owner = PgPoolOptions::new()
        .max_connections(2)
        .connect(&owner_db_url)
        .await
        .unwrap();
    let g = GraphId::new(unique("g").replace('_', "-")).unwrap();
    PgGraphs::new(owner.clone())
        .create(&NewGraph {
            graph_id: g.clone(),
            tenant_id: TenantId::new("tenant-lp").unwrap(),
            knowledge_base_id: None,
            purpose: None,
            status: GraphStatus::Active,
        })
        .await
        .unwrap();
    let store = PostgresLedgerStore::connect(&runtime_db_url, V1Binding::Reject)
        .await
        .unwrap();
    let prepared = store
        .workflows()
        .prepare(&PrepareRequest {
            scope: scope(&g, "k1", b"p1"),
            branch: "main".into(),
            expected_head: None,
            requested: patch("<urn:s> <urn:p> \"1\" .", OperationKind::Add),
            activity: "a".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: "m".into(),
        })
        .await
        .unwrap();
    store
        .workflows()
        .accept(&AcceptRequest {
            scope: scope(&g, "k2", b"a1"),
            branch: "main".into(),
            expected_head: None,
            candidate: prepared.candidate.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await
        .unwrap();
    // Even the non-superuser owner is refused as a runtime identity (it owns the tables).
    let err = PostgresLedgerStore::connect(&owner_db_url, V1Binding::Reject)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&err, LedgerError::RuntimeIdentity(m) if m.contains("owns")),
        "{err}"
    );
    owner.close().await;
    drop(store);
    sqlx::query(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query(&format!("DROP ROLE {runtime_role}"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query(&format!("DROP ROLE {owner_role}"))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn ref_movement_and_status_changes_are_database_facts() {
    // Migration 0009: even the OWNER cannot move a ref on an active graph without a matching
    // ref event, cannot rewind or jump, cannot store an object under the wrong id, and a
    // status change waits for in-flight workflow transactions.
    let f = fixture("lp_integrity").await;
    f.migrate_and_grant().await;
    let g = GraphId::new(unique("g").replace('_', "-")).unwrap();
    PgGraphs::new(f.owner.clone())
        .create(&NewGraph {
            graph_id: g.clone(),
            tenant_id: TenantId::new("tenant-lp").unwrap(),
            knowledge_base_id: None,
            purpose: None,
            status: GraphStatus::Active,
        })
        .await
        .unwrap();
    let store = PostgresLedgerStore::connect(&f.runtime_db_url, V1Binding::Reject)
        .await
        .unwrap();
    let wf = store.workflows();
    let mut heads = Vec::new();
    for i in 0..3 {
        let p = wf
            .prepare(&PrepareRequest {
                scope: scope(&g, &format!("p{i}"), format!("p{i}").as_bytes()),
                branch: "main".into(),
                expected_head: heads.last().cloned(),
                requested: patch(&format!("<urn:s> <urn:p> \"{i}\" ."), OperationKind::Add),
                activity: "a".into(),
                event_time: None,
                evidence_refs: vec![],
                source_system: None,
                message: "m".into(),
            })
            .await
            .unwrap();
        wf.accept(&AcceptRequest {
            scope: scope(&g, &format!("a{i}"), format!("a{i}").as_bytes()),
            branch: "main".into(),
            expected_head: heads.last().cloned(),
            candidate: p.candidate.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await
        .unwrap();
        heads.push(p.candidate);
    }
    // Owner rewinds the ref by hand: refused at commit by the deferred constraint trigger.
    let rewind = sqlx::query(
        "UPDATE refs SET head = $1, version = version + 1 WHERE graph_id = $2 AND branch = 'main'",
    )
    .bind(heads[0].to_string())
    .bind(g.as_str())
    .execute(&f.owner)
    .await;
    assert!(
        matches!(rewind, Err(sqlx::Error::Database(ref e)) if e.message().contains("without a matching ref event")),
        "{rewind:?}"
    );
    // With a forged event but a non-descendant head: refused as a jump.
    let mut tx = f.owner.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO ref_events (graph_id, branch, old_head, new_head, old_version, new_version, \
         operation, tenant_id, principal_id, principal_type) \
         VALUES ($1, 'main', $2, $3, 3, 4, 'advance', 'tenant-lp', 'urn:sculpin:human:forger', 'human')",
    )
    .bind(g.as_str())
    .bind(heads[2].to_string())
    .bind(heads[0].to_string())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE refs SET head = $1, version = 4 WHERE graph_id = $2 AND branch = 'main'")
        .bind(heads[0].to_string())
        .bind(g.as_str())
        .execute(&mut *tx)
        .await
        .unwrap();
    let jump = tx.commit().await;
    assert!(
        matches!(jump, Err(sqlx::Error::Database(ref e)) if e.message().contains("direct descendant")),
        "{jump:?}"
    );
    let (head, version): (String, i64) =
        sqlx::query_as("SELECT head, version FROM refs WHERE graph_id = $1 AND branch = 'main'")
            .bind(g.as_str())
            .fetch_one(&f.owner)
            .await
            .unwrap();
    assert_eq!(
        (head, version),
        (heads[2].to_string(), 3),
        "the ref is untouched"
    );
    // Content addressing is a constraint: a mislabelled object is refused.
    let bad = sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(format!("sha256:{}", "0".repeat(64)))
        .bind(b"not the digest".as_slice())
        .execute(&f.owner)
        .await;
    assert!(
        matches!(bad, Err(sqlx::Error::Database(ref e)) if e.constraint() == Some("immutable_objects_content_addressed")),
        "{bad:?}"
    );
    // A status change takes the exclusive graph-status lock: while a workflow transaction
    // holds the shared lock, the archive waits and times out under lock_timeout.
    let mut shared = f.owner.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(ledger_store::lock_key(&format!("graph-status:{g}")))
        .execute(&mut *shared)
        .await
        .unwrap();
    let mut archiver = f.owner.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout = '200ms'")
        .execute(&mut *archiver)
        .await
        .unwrap();
    let archive = sqlx::query("UPDATE graphs SET status = 'archived' WHERE graph_id = $1")
        .bind(g.as_str())
        .execute(&mut *archiver)
        .await;
    assert!(
        matches!(archive, Err(sqlx::Error::Database(ref e)) if e.code().as_deref() == Some("55P03")),
        "{archive:?}"
    );
    archiver.rollback().await.unwrap();
    shared.rollback().await.unwrap();
    // Without a holder the archive proceeds; the workflow then refuses the graph.
    sqlx::query("UPDATE graphs SET status = 'archived' WHERE graph_id = $1")
        .bind(g.as_str())
        .execute(&f.owner)
        .await
        .unwrap();
    let err = wf
        .prepare(&PrepareRequest {
            scope: scope(&g, "p-archived", b"pa"),
            branch: "main".into(),
            expected_head: heads.last().cloned(),
            requested: patch("<urn:s> <urn:p> \"x\" .", OperationKind::Add),
            activity: "a".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: "m".into(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, LedgerError::GraphNotActive { .. }), "{err:?}");
    // The lock-key derivation agrees between SQL and Rust.
    let (sql_key,): (i64,) = sqlx::query_as("SELECT ledger_lock_key($1)")
        .bind("graph-status:demo")
        .fetch_one(&f.owner)
        .await
        .unwrap();
    assert_eq!(sql_key, ledger_store::lock_key("graph-status:demo"));
    f.teardown().await;
}

// =========================================================================================
// Weakened database controls and drifted privileges are refused at start-up (PR #2 review)
// =========================================================================================

/// The three gates a drifted database must fail: the schema verifier, a fresh server start
/// and the readiness of an already-running server. Returns the schema error text.
async fn assert_refused_by_schema(
    fx: &Fixture,
    running: &ledger_store::PostgresLedgerStore,
    why: &str,
) -> String {
    let rt = fx.runtime_pool().await;
    let verify = ledger_store::schema::verify(&rt).await;
    let message = match verify {
        Err(ledger_core::LedgerError::SchemaIncompatible(m)) => m,
        other => panic!("{why}: schema::verify should be SchemaIncompatible, got {other:?}"),
    };
    match ledger_store::PostgresLedgerStore::connect(
        &fx.runtime_db_url,
        ledger_store::V1Binding::Reject,
    )
    .await
    {
        Err(ledger_core::LedgerError::SchemaIncompatible(_)) => {}
        other => {
            panic!("{why}: server start-up should refuse with SchemaIncompatible, got {other:?}")
        }
    }
    match running.ready().await {
        Err(ledger_core::LedgerError::SchemaIncompatible(_)) => {}
        other => panic!("{why}: readiness should refuse with SchemaIncompatible, got {other:?}"),
    }
    rt.close().await;
    message
}

async fn assert_refused_by_identity(fx: &Fixture, why: &str) -> String {
    let rt = fx.runtime_pool().await;
    ledger_store::schema::verify(&rt)
        .await
        .unwrap_or_else(|e| panic!("{why}: the schema itself must still verify: {e}"));
    let message = match ledger_store::schema::verify_runtime_identity(&rt).await {
        Err(ledger_core::LedgerError::RuntimeIdentity(m)) => m,
        other => panic!("{why}: verify_runtime_identity should be RuntimeIdentity, got {other:?}"),
    };
    match ledger_store::PostgresLedgerStore::connect(
        &fx.runtime_db_url,
        ledger_store::V1Binding::Reject,
    )
    .await
    {
        Err(ledger_core::LedgerError::RuntimeIdentity(_)) => {}
        other => panic!("{why}: server start-up should refuse with RuntimeIdentity, got {other:?}"),
    }
    rt.close().await;
    message
}

async fn assert_healthy(fx: &Fixture, why: &str) {
    let rt = fx.runtime_pool().await;
    ledger_store::schema::verify(&rt)
        .await
        .unwrap_or_else(|e| panic!("{why}: {e}"));
    ledger_store::schema::verify_runtime_identity(&rt)
        .await
        .unwrap_or_else(|e| panic!("{why}: {e}"));
    rt.close().await;
}

/// A membership the runtime can `SET ROLE` into without inheriting it: PostgreSQL 16+
/// syntax, or a plain GRANT on 15 (where every membership is settable).
async fn grant_settable(fx: &Fixture, parent: &str) {
    let version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
        .fetch_one(&fx.owner)
        .await
        .unwrap();
    let sql = if version >= 160_000 {
        format!("GRANT {parent} TO {} WITH INHERIT FALSE, SET TRUE", fx.role)
    } else {
        format!("GRANT {parent} TO {}", fx.role)
    };
    owner_exec(fx, &sql).await;
}

async fn owner_exec(fx: &Fixture, sql: &str) {
    sqlx::query(sql)
        .execute(&fx.owner)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn weakened_integrity_triggers_are_refused_at_startup_and_readiness() {
    let fx = fixture("ledger_trig").await;
    fx.migrate_and_grant().await;
    let running = ledger_store::PostgresLedgerStore::connect(
        &fx.runtime_db_url,
        ledger_store::V1Binding::Reject,
    )
    .await
    .expect("healthy database serves");
    assert_healthy(&fx, "fresh migration").await;
    const REAL: &str = "CREATE CONSTRAINT TRIGGER refs_movement_audited \
        AFTER INSERT OR UPDATE OF head, version ON refs DEFERRABLE INITIALLY DEFERRED \
        FOR EACH ROW EXECUTE FUNCTION public.refs_movement_is_audited()";

    // 1. Same-named enabled trigger on another table while the real one is gone.
    owner_exec(&fx, "DROP TRIGGER refs_movement_audited ON refs").await;
    owner_exec(&fx, "CREATE CONSTRAINT TRIGGER refs_movement_audited AFTER INSERT ON proposals \
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.refs_movement_is_audited()").await;
    let m = assert_refused_by_schema(&fx, &running, "trigger moved to another table").await;
    assert!(
        m.contains("refs_movement_audited") && m.contains("missing on public.refs"),
        "{m}"
    );
    owner_exec(&fx, "DROP TRIGGER refs_movement_audited ON proposals").await;

    // 2. Right table, wrong function.
    owner_exec(&fx, "CREATE CONSTRAINT TRIGGER refs_movement_audited AFTER INSERT OR UPDATE OF head, version ON refs \
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.graphs_status_change_serializes()").await;
    let m = assert_refused_by_schema(&fx, &running, "wrong function").await;
    assert!(
        m.contains("graphs_status_change_serializes")
            && m.contains("instead of public.refs_movement_is_audited"),
        "{m}"
    );
    owner_exec(&fx, "DROP TRIGGER refs_movement_audited ON refs").await;

    // 3a. Weakened events: INSERT only.
    owner_exec(&fx, "CREATE CONSTRAINT TRIGGER refs_movement_audited AFTER INSERT ON refs \
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.refs_movement_is_audited()").await;
    let m = assert_refused_by_schema(&fx, &running, "INSERT-only trigger").await;
    assert!(m.contains("timing/events"), "{m}");
    owner_exec(&fx, "DROP TRIGGER refs_movement_audited ON refs").await;
    // 3b. Weakened column list: UPDATE OF version only (a head move without a version bump
    //     would no longer be audited).
    owner_exec(&fx, "CREATE CONSTRAINT TRIGGER refs_movement_audited AFTER INSERT OR UPDATE OF version ON refs \
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.refs_movement_is_audited()").await;
    let m = assert_refused_by_schema(&fx, &running, "UPDATE OF version only").await;
    assert!(m.contains("UPDATE OF"), "{m}");
    owner_exec(&fx, "DROP TRIGGER refs_movement_audited ON refs").await;
    // 3c. Not deferred: the event row written later in the transaction would never be seen.
    owner_exec(&fx, "CREATE CONSTRAINT TRIGGER refs_movement_audited AFTER INSERT OR UPDATE OF head, version ON refs \
        FOR EACH ROW EXECUTE FUNCTION public.refs_movement_is_audited()").await;
    let m = assert_refused_by_schema(&fx, &running, "not deferrable").await;
    assert!(m.contains("deferrable"), "{m}");
    owner_exec(&fx, "DROP TRIGGER refs_movement_audited ON refs").await;
    // 3d. A plain (non-constraint) BEFORE trigger with the right function and events.
    owner_exec(
        &fx,
        "CREATE TRIGGER refs_movement_audited BEFORE INSERT OR UPDATE OF head, version ON refs \
        FOR EACH ROW EXECUTE FUNCTION public.refs_movement_is_audited()",
    )
    .await;
    let m =
        assert_refused_by_schema(&fx, &running, "BEFORE instead of AFTER constraint trigger").await;
    assert!(m.contains("timing/events"), "{m}");
    owner_exec(&fx, "DROP TRIGGER refs_movement_audited ON refs").await;

    // 4. Disabled real trigger; and a disabled write-once guard on another table.
    owner_exec(&fx, REAL).await;
    assert_healthy(&fx, "real trigger recreated").await;
    owner_exec(
        &fx,
        "ALTER TABLE refs DISABLE TRIGGER refs_movement_audited",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "disabled trigger").await;
    assert!(m.contains("is disabled"), "{m}");
    owner_exec(&fx, "ALTER TABLE refs ENABLE TRIGGER refs_movement_audited").await;
    owner_exec(
        &fx,
        "ALTER TABLE decisions DISABLE TRIGGER decisions_write_once",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "disabled write-once guard").await;
    assert!(m.contains("decisions_write_once"), "{m}");
    owner_exec(
        &fx,
        "ALTER TABLE decisions ENABLE TRIGGER decisions_write_once",
    )
    .await;
    // A dropped write-once guard on a different table is missing, not merely disabled.
    owner_exec(&fx, "DROP TRIGGER idempotency_write_once ON idempotency").await;
    let m = assert_refused_by_schema(&fx, &running, "dropped write-once guard").await;
    assert!(
        m.contains("idempotency_write_once") && m.contains("missing"),
        "{m}"
    );
    owner_exec(
        &fx,
        "CREATE TRIGGER idempotency_write_once BEFORE UPDATE OR DELETE ON idempotency \
        FOR EACH ROW EXECUTE FUNCTION ledger_rows_are_write_once()",
    )
    .await;
    assert_healthy(&fx, "all guards restored").await;
    running.ready().await.expect("readiness after restore");
    fx.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn weakened_content_address_check_is_refused_at_startup_and_readiness() {
    let fx = fixture("ledger_check").await;
    fx.migrate_and_grant().await;
    let running = ledger_store::PostgresLedgerStore::connect(
        &fx.runtime_db_url,
        ledger_store::V1Binding::Reject,
    )
    .await
    .unwrap();
    const REAL: &str = "ALTER TABLE immutable_objects ADD CONSTRAINT immutable_objects_content_addressed \
        CHECK (id = 'sha256:' || encode(sha256(bytes), 'hex'))";
    // Dropped.
    owner_exec(
        &fx,
        "ALTER TABLE immutable_objects DROP CONSTRAINT immutable_objects_content_addressed",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "CHECK dropped").await;
    assert!(
        m.contains("immutable_objects_content_addressed") && m.contains("missing"),
        "{m}"
    );
    // Same name, vacuous condition: the lock-free catalog check alone cannot tell (present,
    // CHECK, validated), so start-up must catch it by definition + probe and a running
    // server by the changed expression fingerprint.
    owner_exec(&fx, "ALTER TABLE immutable_objects ADD CONSTRAINT immutable_objects_content_addressed CHECK (true)").await;
    {
        let rt = fx.runtime_pool().await;
        ledger_store::schema::verify(&rt)
            .await
            .expect("catalog facts alone still pass");
        match ledger_store::schema::verify_definitions_at_startup(&rt).await {
            Err(ledger_core::LedgerError::SchemaIncompatible(m)) => {
                assert!(m.contains("definition"), "{m}")
            }
            other => panic!("CHECK (true): start-up definitions should refuse, got {other:?}"),
        }
        match ledger_store::PostgresLedgerStore::connect(
            &fx.runtime_db_url,
            ledger_store::V1Binding::Reject,
        )
        .await
        {
            Err(ledger_core::LedgerError::SchemaIncompatible(_)) => {}
            other => panic!("CHECK (true): start-up should refuse, got {other:?}"),
        }
        match running.ready().await {
            Err(ledger_core::LedgerError::SchemaIncompatible(m)) => {
                assert!(m.contains("changed since start-up"), "{m}")
            }
            other => panic!("CHECK (true): readiness should refuse, got {other:?}"),
        }
        rt.close().await;
    }
    owner_exec(
        &fx,
        "ALTER TABLE immutable_objects DROP CONSTRAINT immutable_objects_content_addressed",
    )
    .await;
    // Right definition, but NOT VALID (existing rows unverified).
    owner_exec(
        &fx,
        "ALTER TABLE immutable_objects ADD CONSTRAINT immutable_objects_content_addressed \
        CHECK (id = 'sha256:' || encode(sha256(bytes), 'hex')) NOT VALID",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "NOT VALID").await;
    assert!(m.contains("NOT VALID"), "{m}");
    owner_exec(
        &fx,
        "ALTER TABLE immutable_objects DROP CONSTRAINT immutable_objects_content_addressed",
    )
    .await;
    // Healthy again (validated on add, since the table is clean).
    owner_exec(&fx, REAL).await;
    assert_healthy(&fx, "CHECK restored").await;
    running.ready().await.expect("readiness after restore");
    // The probe leaves nothing behind.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM immutable_objects")
        .fetch_one(&fx.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "the rolled-back probe must not persist a row");
    fx.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn drifted_column_and_sequence_privileges_are_refused_at_startup() {
    let fx = fixture("ledger_priv").await;
    fx.migrate_and_grant().await;
    assert_healthy(&fx, "exact grant").await;
    let role = fx.role.clone();
    let regrant = || async {
        let mut conn = PgConnection::connect(&fx.owner_db_url).await.unwrap();
        schema::grant_runtime_role(&mut conn, &fx.role)
            .await
            .unwrap();
        conn.close().await.unwrap();
        assert_healthy(&fx, "re-granted").await;
    };

    // Missing required INSERT column.
    owner_exec(
        &fx,
        &format!("REVOKE INSERT (bytes) ON immutable_objects FROM {role}"),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "revoked INSERT (bytes)").await;
    assert!(
        m.contains("lacks INSERT on public.immutable_objects.bytes"),
        "{m}"
    );
    regrant().await;

    // Only one of several expected columns.
    owner_exec(&fx, &format!("REVOKE INSERT ON proposals FROM {role}")).await;
    owner_exec(
        &fx,
        &format!("GRANT INSERT (graph_id) ON proposals TO {role}"),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "partial proposals INSERT").await;
    assert!(m.contains("lacks INSERT on public.proposals."), "{m}");
    regrant().await;

    // Full-table INSERT on refs (would make `protected` writable).
    owner_exec(&fx, &format!("GRANT INSERT ON refs TO {role}")).await;
    let m = assert_refused_by_identity(&fx, "table-level INSERT on refs").await;
    assert!(m.contains("table-level INSERT on public.refs"), "{m}");
    regrant().await;

    // Excess column: INSERT on the excluded refs.updated_at (back-dating a ref).
    owner_exec(&fx, &format!("GRANT INSERT (updated_at) ON refs TO {role}")).await;
    let m = assert_refused_by_identity(&fx, "INSERT (updated_at)").await;
    assert!(m.contains("holds INSERT on public.refs.updated_at"), "{m}");
    regrant().await;
    // Excess column on the branch tables: back-dating a lifecycle event.
    owner_exec(
        &fx,
        &format!("GRANT INSERT (recorded_at) ON branch_events TO {role}"),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "INSERT (recorded_at) on branch_events").await;
    assert!(
        m.contains("holds INSERT on public.branch_events.recorded_at"),
        "{m}"
    );
    regrant().await;
    // Excess UPDATE on a branch's immutable policy.
    owner_exec(
        &fx,
        &format!("GRANT UPDATE (require_validation) ON branches TO {role}"),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "UPDATE (require_validation) on branches").await;
    assert!(m.contains("branches.require_validation"), "{m}");
    regrant().await;

    // Excess UPDATE on an audit column that must stay write-once.
    owner_exec(
        &fx,
        &format!("GRANT UPDATE (reason) ON ref_events TO {role}"),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "UPDATE (reason) on ref_events").await;
    assert!(
        m.contains("holds UPDATE on public.ref_events.reason"),
        "{m}"
    );
    regrant().await;

    // Excess UPDATE column on refs (protected must not be flippable).
    owner_exec(&fx, &format!("GRANT UPDATE (protected) ON refs TO {role}")).await;
    let m = assert_refused_by_identity(&fx, "UPDATE (protected) on refs").await;
    assert!(m.contains("holds UPDATE on public.refs.protected"), "{m}");
    regrant().await;

    // Table-level grants inherited through role membership are caught the same way.
    owner_exec(&fx, "DROP ROLE IF EXISTS ledger_priv_parent").await;
    owner_exec(&fx, "CREATE ROLE ledger_priv_parent").await;
    owner_exec(&fx, "GRANT UPDATE ON decisions TO ledger_priv_parent").await;
    owner_exec(&fx, &format!("GRANT ledger_priv_parent TO {role}")).await;
    let m = assert_refused_by_identity(&fx, "inherited table-level UPDATE").await;
    assert!(m.contains("table-level UPDATE on public.decisions"), "{m}");
    owner_exec(&fx, &format!("REVOKE ledger_priv_parent FROM {role}")).await;
    owner_exec(&fx, "DROP OWNED BY ledger_priv_parent").await;
    owner_exec(&fx, "DROP ROLE ledger_priv_parent").await;
    regrant().await;

    // Sequences: a missing USAGE, then an excessive UPDATE.
    owner_exec(
        &fx,
        &format!("REVOKE USAGE ON SEQUENCE proposals_proposal_id_seq FROM {role}"),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "revoked sequence USAGE").await;
    assert!(
        m.contains("lacks USAGE on sequence public.proposals_proposal_id_seq"),
        "{m}"
    );
    regrant().await;
    owner_exec(
        &fx,
        &format!("GRANT UPDATE ON SEQUENCE decisions_decision_id_seq TO {role}"),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "excess sequence UPDATE").await;
    assert!(
        m.contains("SELECT/UPDATE on sequence public.decisions_decision_id_seq"),
        "{m}"
    );
    regrant().await;
    // A sequence outside the model with any privilege is refused too.
    owner_exec(&fx, "CREATE SEQUENCE stray_seq").await;
    owner_exec(&fx, &format!("GRANT USAGE ON SEQUENCE stray_seq TO {role}")).await;
    let m = assert_refused_by_identity(&fx, "stray sequence USAGE").await;
    assert!(
        m.contains("holds USAGE on sequence public.stray_seq"),
        "{m}"
    );
    owner_exec(&fx, "DROP SEQUENCE stray_seq").await;
    regrant().await;

    // The healthy exact role still serves the workflow after all that.
    let store = ledger_store::PostgresLedgerStore::connect(
        &fx.runtime_db_url,
        ledger_store::V1Binding::Reject,
    )
    .await
    .expect("exact role connects");
    assert!(store.ready().await.is_ok());
    fx.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn replaced_guard_function_bodies_are_refused_at_startup_and_readiness() {
    let fx = fixture("ledger_fnbody").await;
    fx.migrate_and_grant().await;
    let running = ledger_store::PostgresLedgerStore::connect(
        &fx.runtime_db_url,
        ledger_store::V1Binding::Reject,
    )
    .await
    .unwrap();
    let original = ledger_store::schema::expected_guard_functions()
        .into_iter()
        .find(|f| f.name == "refs_movement_is_audited")
        .unwrap();
    // Same name, OID, trigger binding and events — but a no-op body.
    owner_exec(
        &fx,
        "CREATE OR REPLACE FUNCTION public.refs_movement_is_audited() RETURNS trigger \
        LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$ BEGIN RETURN NULL; END $$",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "no-op trigger function").await;
    assert!(
        m.contains("refs_movement_is_audited") && m.contains("different body"),
        "{m}"
    );
    // Same body but SECURITY DEFINER, and the same body without the pinned search_path.
    let restore = format!(
        "CREATE OR REPLACE FUNCTION public.refs_movement_is_audited() RETURNS trigger LANGUAGE {} \
         SET search_path = {} AS $${}$$",
        original.language,
        original.search_path.clone().unwrap(),
        original.body
    );
    owner_exec(&fx, &format!("{restore} SECURITY DEFINER")).await;
    let m = assert_refused_by_schema(&fx, &running, "SECURITY DEFINER guard").await;
    assert!(m.contains("SECURITY DEFINER"), "{m}");
    owner_exec(&fx, &format!(
        "CREATE OR REPLACE FUNCTION public.refs_movement_is_audited() RETURNS trigger LANGUAGE {} AS $${}$$",
        original.language, original.body
    )).await;
    let m = assert_refused_by_schema(&fx, &running, "search_path dropped").await;
    assert!(m.contains("settings"), "{m}");
    // A write-once guard replaced with a permissive body (returns NEW instead of raising).
    owner_exec(&fx, &restore).await;
    assert_healthy(&fx, "audited function restored").await;
    owner_exec(
        &fx,
        "CREATE OR REPLACE FUNCTION ledger_rows_are_write_once() RETURNS trigger \
        LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "permissive write-once guard").await;
    assert!(
        m.contains("ledger_rows_are_write_once") && m.contains("different body"),
        "{m}"
    );
    let wo = ledger_store::schema::expected_guard_functions()
        .into_iter()
        .find(|f| f.name == "ledger_rows_are_write_once")
        .unwrap();
    owner_exec(&fx, &format!(
        "CREATE OR REPLACE FUNCTION ledger_rows_are_write_once() RETURNS trigger LANGUAGE {} AS $${}$$",
        wo.language, wo.body
    )).await;
    assert_healthy(&fx, "write-once guard restored").await;
    running.ready().await.expect("readiness after restore");
    fx.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn settable_or_inherited_memberships_in_privileged_roles_are_refused() {
    let fx = fixture("ledger_member").await;
    fx.migrate_and_grant().await;
    assert_healthy(&fx, "exact role").await;
    let role = fx.role.clone();
    // A non-inheriting but settable membership in the (superuser) owner: `SET ROLE` would
    // hand the runtime credentials owner powers while every direct grant still looks exact.
    grant_settable(&fx, "ledger").await;
    let m = assert_refused_by_identity(&fx, "settable owner membership").await;
    // PostgreSQL 16+: the membership itself is reported (non-inheriting, settable). On 15 the
    // membership inherits, so the earlier superuser/CREATE checks fire first; both refuse.
    assert!(
        m.contains("is a member of ledger")
            || m.contains("CREATE on schema public")
            || m.contains("superuser"),
        "{m}"
    );
    owner_exec(&fx, &format!("REVOKE ledger FROM {role}")).await;
    assert_healthy(&fx, "membership revoked").await;
    // Transitive: runtime → intermediate → owner.
    owner_exec(&fx, "DROP ROLE IF EXISTS ledger_member_mid").await;
    owner_exec(&fx, "CREATE ROLE ledger_member_mid").await;
    {
        let version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
            .fetch_one(&fx.owner)
            .await
            .unwrap();
        owner_exec(
            &fx,
            if version >= 160_000 {
                "GRANT ledger TO ledger_member_mid WITH INHERIT FALSE, SET TRUE"
            } else {
                "GRANT ledger TO ledger_member_mid"
            },
        )
        .await;
    }
    owner_exec(&fx, &format!("GRANT ledger_member_mid TO {role}")).await;
    let m = assert_refused_by_identity(&fx, "transitive owner membership").await;
    assert!(
        m.contains("is a member of ledger")
            || m.contains("CREATE on schema public")
            || m.contains("superuser"),
        "{m}"
    );
    owner_exec(&fx, &format!("REVOKE ledger_member_mid FROM {role}")).await;
    owner_exec(&fx, "REVOKE ledger FROM ledger_member_mid").await;
    owner_exec(&fx, "DROP ROLE ledger_member_mid").await;
    // Predefined roles and role attributes are refused too.
    owner_exec(&fx, &format!("GRANT pg_read_all_data TO {role}")).await;
    let m = assert_refused_by_identity(&fx, "pg_read_all_data").await;
    assert!(m.contains("pg_read_all_data"), "{m}");
    owner_exec(&fx, &format!("REVOKE pg_read_all_data FROM {role}")).await;
    owner_exec(&fx, &format!("ALTER ROLE {role} CREATEDB")).await;
    let m = assert_refused_by_identity(&fx, "CREATEDB attribute").await;
    assert!(m.contains("CREATEDB"), "{m}");
    owner_exec(&fx, &format!("ALTER ROLE {role} NOCREATEDB")).await;
    assert_healthy(&fx, "plain login role again").await;
    fx.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn parameter_grants_and_function_ownership_drift_are_refused() {
    let fx = fixture("ledger_param").await;
    fx.migrate_and_grant().await;
    assert_healthy(&fx, "exact role").await;
    let role = fx.role.clone();
    // SET privilege on session_replication_role (PostgreSQL 15+): the runtime could switch to
    // `replica` and silence every ordinary trigger.
    owner_exec(
        &fx,
        &format!("GRANT SET ON PARAMETER session_replication_role TO {role}"),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "SET session_replication_role").await;
    assert!(m.contains("session_replication_role"), "{m}");
    owner_exec(
        &fx,
        &format!("REVOKE SET ON PARAMETER session_replication_role FROM {role}"),
    )
    .await;
    assert_healthy(&fx, "parameter grant revoked").await;
    // The same grant reached through a settable membership.
    owner_exec(&fx, "DROP ROLE IF EXISTS ledger_param_parent").await;
    owner_exec(&fx, "CREATE ROLE ledger_param_parent").await;
    owner_exec(
        &fx,
        "GRANT SET ON PARAMETER session_replication_role TO ledger_param_parent",
    )
    .await;
    grant_settable(&fx, "ledger_param_parent").await;
    let m = assert_refused_by_identity(&fx, "parameter grant via settable membership").await;
    // 16+: reported through the membership; 15 inherits the grant, so the direct check fires.
    assert!(m.contains("session_replication_role"), "{m}");
    owner_exec(&fx, &format!("REVOKE ledger_param_parent FROM {role}")).await;
    owner_exec(&fx, "DROP OWNED BY ledger_param_parent").await;
    owner_exec(&fx, "DROP ROLE ledger_param_parent").await;
    assert_healthy(&fx, "membership revoked").await;
    // A guard function transferred to the runtime (or to a role it can become) could be
    // dropped with CASCADE, taking its trigger along: a schema fact, refused by verify.
    let running = ledger_store::PostgresLedgerStore::connect(
        &fx.runtime_db_url,
        ledger_store::V1Binding::Reject,
    )
    .await
    .unwrap();
    owner_exec(
        &fx,
        &format!("ALTER FUNCTION public.refs_movement_is_audited() OWNER TO {role}"),
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "guard function owned by the runtime").await;
    assert!(
        m.contains("refs_movement_is_audited") && m.contains("is owned by"),
        "{m}"
    );
    owner_exec(
        &fx,
        "ALTER FUNCTION public.refs_movement_is_audited() OWNER TO ledger",
    )
    .await;
    assert_healthy(&fx, "ownership restored").await;
    running.ready().await.expect("readiness after restore");
    fx.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn conditional_triggers_and_set_role_reachable_privileges_are_refused() {
    let fx = fixture("ledger_when").await;
    fx.migrate_and_grant().await;
    let running = ledger_store::PostgresLedgerStore::connect(
        &fx.runtime_db_url,
        ledger_store::V1Binding::Reject,
    )
    .await
    .unwrap();
    // A WHEN clause keeps every structural property identical yet the guard never fires.
    owner_exec(&fx, "DROP TRIGGER refs_movement_audited ON refs").await;
    owner_exec(&fx, "CREATE CONSTRAINT TRIGGER refs_movement_audited AFTER INSERT OR UPDATE OF head, version ON refs \
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (false) EXECUTE FUNCTION public.refs_movement_is_audited()").await;
    let m = assert_refused_by_schema(&fx, &running, "WHEN (false) trigger").await;
    assert!(m.contains("WHEN condition"), "{m}");
    owner_exec(&fx, "DROP TRIGGER refs_movement_audited ON refs").await;
    owner_exec(&fx, "CREATE CONSTRAINT TRIGGER refs_movement_audited AFTER INSERT OR UPDATE OF head, version ON refs \
        DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.refs_movement_is_audited()").await;
    assert_healthy(&fx, "trigger restored").await;
    // Privileges one SET ROLE away: a NOINHERIT parent holding UPDATE (status) on graphs (the
    // 0009 importing exemption) or USAGE on a stray sequence or EXECUTE on the grant function.
    let role = fx.role.clone();
    owner_exec(&fx, "DROP ROLE IF EXISTS ledger_when_parent").await;
    owner_exec(&fx, "CREATE ROLE ledger_when_parent").await;
    owner_exec(&fx, "GRANT SELECT ON graphs TO ledger_when_parent").await;
    owner_exec(&fx, "GRANT UPDATE (status) ON graphs TO ledger_when_parent").await;
    grant_settable(&fx, "ledger_when_parent").await;
    let m = assert_refused_by_identity(&fx, "SET-ROLE-reachable UPDATE (status)").await;
    assert!(
        m.contains("holds UPDATE on public.graphs.status") && m.contains("ledger_when_parent"),
        "{m}"
    );
    owner_exec(
        &fx,
        "REVOKE UPDATE (status) ON graphs FROM ledger_when_parent",
    )
    .await;
    // SELECT-only parent is within the model (a subset) and passes.
    assert_healthy(&fx, "parent within the model").await;
    owner_exec(
        &fx,
        "GRANT EXECUTE ON FUNCTION public.ledger_grant_runtime(text) TO ledger_when_parent",
    )
    .await;
    let m =
        assert_refused_by_identity(&fx, "SET-ROLE-reachable EXECUTE on the grant function").await;
    assert!(m.contains("ledger_grant_runtime"), "{m}");
    owner_exec(
        &fx,
        "REVOKE EXECUTE ON FUNCTION public.ledger_grant_runtime(text) FROM ledger_when_parent",
    )
    .await;
    owner_exec(&fx, "CREATE SEQUENCE ledger_when_seq").await;
    owner_exec(
        &fx,
        "GRANT USAGE ON SEQUENCE ledger_when_seq TO ledger_when_parent",
    )
    .await;
    let m = assert_refused_by_identity(&fx, "SET-ROLE-reachable sequence USAGE").await;
    assert!(m.contains("ledger_when_seq"), "{m}");
    owner_exec(&fx, "DROP SEQUENCE ledger_when_seq").await;
    owner_exec(&fx, &format!("REVOKE ledger_when_parent FROM {role}")).await;
    owner_exec(&fx, "DROP OWNED BY ledger_when_parent").await;
    owner_exec(&fx, "DROP ROLE ledger_when_parent").await;
    assert_healthy(&fx, "membership removed").await;
    running.ready().await.expect("readiness after restore");
    fx.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn lost_referential_and_uniqueness_constraints_are_refused_at_startup_and_readiness() {
    let fx = fixture("ledger_fk").await;
    fx.migrate_and_grant().await;
    let running = ledger_store::PostgresLedgerStore::connect(
        &fx.runtime_db_url,
        ledger_store::V1Binding::Reject,
    )
    .await
    .unwrap();
    // The FK that binds every indexed commit to real immutable bytes (0003).
    owner_exec(
        &fx,
        "ALTER TABLE commit_index DROP CONSTRAINT commit_index_id_fkey",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "commit_index.id FK dropped").await;
    assert!(
        m.contains("commit_index FOREIGN KEY [\"id\"] -> public.immutable_objects"),
        "{m}"
    );
    // A same-shaped FK recreated NOT VALID does not count either.
    owner_exec(&fx, "ALTER TABLE commit_index ADD CONSTRAINT commit_index_id_fkey FOREIGN KEY (id) REFERENCES immutable_objects (id) NOT VALID").await;
    let m = assert_refused_by_schema(&fx, &running, "NOT VALID FK").await;
    assert!(m.contains("NOT VALID"), "{m}");
    owner_exec(
        &fx,
        "ALTER TABLE commit_index VALIDATE CONSTRAINT commit_index_id_fkey",
    )
    .await;
    assert_healthy(&fx, "FK validated").await;
    // A deferrable replacement of the ref head FK, a dropped uniqueness rule and a dropped
    // partial unique index are refused too.
    owner_exec(&fx, "ALTER TABLE refs DROP CONSTRAINT refs_head_fk").await;
    owner_exec(&fx, "ALTER TABLE refs ADD CONSTRAINT refs_head_fk FOREIGN KEY (graph_id, head) REFERENCES commit_index (graph_id, id) DEFERRABLE").await;
    let m = assert_refused_by_schema(&fx, &running, "deferrable ref head FK").await;
    assert!(m.contains("deferrable"), "{m}");
    owner_exec(&fx, "ALTER TABLE refs DROP CONSTRAINT refs_head_fk").await;
    owner_exec(&fx, "ALTER TABLE refs ADD CONSTRAINT refs_head_fk FOREIGN KEY (graph_id, head) REFERENCES commit_index (graph_id, id)").await;
    // (ref_events_version_unique has dependent FKs; the one-proposal-per-candidate rule has none.)
    owner_exec(
        &fx,
        "ALTER TABLE proposals DROP CONSTRAINT proposals_candidate_unique",
    )
    .await;
    let m = assert_refused_by_schema(
        &fx,
        &running,
        "one-proposal-per-candidate uniqueness dropped",
    )
    .await;
    assert!(m.contains("proposals UNIQUE"), "{m}");
    owner_exec(
        &fx,
        "ALTER TABLE proposals ADD CONSTRAINT proposals_candidate_unique UNIQUE (candidate_commit)",
    )
    .await;
    owner_exec(&fx, "DROP INDEX decisions_one_per_candidate").await;
    let m =
        assert_refused_by_schema(&fx, &running, "one-decision-per-candidate index dropped").await;
    assert!(
        m.contains("unique index decisions_one_per_candidate"),
        "{m}"
    );
    owner_exec(
        &fx,
        "CREATE UNIQUE INDEX decisions_one_per_candidate ON decisions (candidate_commit)",
    )
    .await;
    // A named CHECK replaced by a vacuous one keeps its name: presence/validation pass here
    // (definitions are the Rust layer's job for these); a dropped one is refused.
    owner_exec(
        &fx,
        "ALTER TABLE ref_events DROP CONSTRAINT ref_events_genesis_shape",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "genesis-shape CHECK dropped").await;
    assert!(m.contains("ref_events_genesis_shape"), "{m}");
    owner_exec(&fx, "ALTER TABLE ref_events ADD CONSTRAINT ref_events_genesis_shape CHECK ((operation = 'genesis' AND old_head IS NULL AND old_version IS NULL AND new_version = 1) OR (operation IN ('advance', 'merge') AND old_head IS NOT NULL AND old_version IS NOT NULL AND new_version = old_version + 1))").await;
    assert_healthy(&fx, "all constraints restored").await;
    running.ready().await.expect("readiness after restore");
    fx.teardown().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn duplicate_invalid_constraints_foreign_schemas_null_semantics_and_predicates_are_refused() {
    let fx = fixture("ledger_shape").await;
    fx.migrate_and_grant().await;
    let running = ledger_store::PostgresLedgerStore::connect(
        &fx.runtime_db_url,
        ledger_store::V1Binding::Reject,
    )
    .await
    .unwrap();
    // 1. Two NOT VALID copies of the same shape are not an enforced constraint.
    owner_exec(
        &fx,
        "ALTER TABLE commit_index DROP CONSTRAINT commit_index_id_fkey",
    )
    .await;
    owner_exec(&fx, "ALTER TABLE commit_index ADD CONSTRAINT ci_id_fk_a FOREIGN KEY (id) REFERENCES immutable_objects (id) NOT VALID").await;
    owner_exec(&fx, "ALTER TABLE commit_index ADD CONSTRAINT ci_id_fk_b FOREIGN KEY (id) REFERENCES immutable_objects (id) NOT VALID").await;
    let m = assert_refused_by_schema(&fx, &running, "two NOT VALID copies").await;
    assert!(m.contains("only as NOT VALID or deferrable copies"), "{m}");
    owner_exec(&fx, "ALTER TABLE commit_index DROP CONSTRAINT ci_id_fk_a").await;
    owner_exec(&fx, "ALTER TABLE commit_index DROP CONSTRAINT ci_id_fk_b").await;
    // 2. Same-named table in another schema as the FK target.
    owner_exec(&fx, "CREATE SCHEMA shadow").await;
    owner_exec(
        &fx,
        "CREATE TABLE shadow.immutable_objects (id TEXT PRIMARY KEY)",
    )
    .await;
    owner_exec(&fx, "ALTER TABLE commit_index ADD CONSTRAINT commit_index_id_fkey FOREIGN KEY (id) REFERENCES shadow.immutable_objects (id)").await;
    let m = assert_refused_by_schema(&fx, &running, "FK into another schema").await;
    assert!(
        m.contains("commit_index FOREIGN KEY [\"id\"] -> public.immutable_objects")
            && m.contains("missing"),
        "{m}"
    );
    owner_exec(
        &fx,
        "ALTER TABLE commit_index DROP CONSTRAINT commit_index_id_fkey",
    )
    .await;
    owner_exec(&fx, "DROP SCHEMA shadow CASCADE").await;
    owner_exec(&fx, "ALTER TABLE commit_index ADD CONSTRAINT commit_index_id_fkey FOREIGN KEY (id) REFERENCES immutable_objects (id)").await;
    assert_healthy(&fx, "FK restored").await;
    // 3. The idempotency scope without NULLS NOT DISTINCT lets NULL on_behalf_of scopes collide.
    owner_exec(
        &fx,
        "ALTER TABLE idempotency DROP CONSTRAINT idempotency_scope_unique",
    )
    .await;
    owner_exec(&fx, "ALTER TABLE idempotency ADD CONSTRAINT idempotency_scope_unique UNIQUE (tenant_id, graph_id, operation, idempotency_key, principal_id, principal_type, on_behalf_of)").await;
    let m = assert_refused_by_schema(&fx, &running, "UNIQUE without NULLS NOT DISTINCT").await;
    assert!(
        m.contains("NULLS NOT DISTINCT") && m.contains("missing"),
        "{m}"
    );
    owner_exec(
        &fx,
        "ALTER TABLE idempotency DROP CONSTRAINT idempotency_scope_unique",
    )
    .await;
    owner_exec(&fx, "ALTER TABLE idempotency ADD CONSTRAINT idempotency_scope_unique UNIQUE NULLS NOT DISTINCT (tenant_id, graph_id, operation, idempotency_key, principal_id, principal_type, on_behalf_of)").await;
    assert_healthy(&fx, "NULLS NOT DISTINCT restored").await;
    // 4. A partial unique index whose predicate never holds: catalog shape identical, so
    //    start-up (deparse) and readiness (fingerprint) must catch it.
    owner_exec(&fx, "DROP INDEX decisions_one_per_proposal").await;
    owner_exec(
        &fx,
        "CREATE UNIQUE INDEX decisions_one_per_proposal ON decisions (proposal_id) WHERE false",
    )
    .await;
    {
        let rt = fx.runtime_pool().await;
        match ledger_store::schema::verify_definitions_at_startup(&rt).await {
            Err(ledger_core::LedgerError::SchemaIncompatible(m)) => {
                assert!(m.contains("predicate"), "{m}")
            }
            other => panic!("WHERE false: start-up should refuse, got {other:?}"),
        }
        match ledger_store::PostgresLedgerStore::connect(
            &fx.runtime_db_url,
            ledger_store::V1Binding::Reject,
        )
        .await
        {
            Err(ledger_core::LedgerError::SchemaIncompatible(_)) => {}
            other => panic!("WHERE false: server start-up should refuse, got {other:?}"),
        }
        match running.ready().await {
            Err(ledger_core::LedgerError::SchemaIncompatible(m)) => {
                assert!(m.contains("decisions_one_per_proposal"), "{m}")
            }
            other => panic!("WHERE false: readiness should refuse, got {other:?}"),
        }
        rt.close().await;
    }
    owner_exec(&fx, "DROP INDEX decisions_one_per_proposal").await;
    owner_exec(&fx, "CREATE UNIQUE INDEX decisions_one_per_proposal ON decisions (proposal_id) WHERE proposal_id IS NOT NULL").await;
    // 5. A vacuous replacement of any other named CHECK is caught the same way.
    owner_exec(
        &fx,
        "ALTER TABLE decisions DROP CONSTRAINT decisions_accepted_has_event",
    )
    .await;
    owner_exec(
        &fx,
        "ALTER TABLE decisions ADD CONSTRAINT decisions_accepted_has_event CHECK (true)",
    )
    .await;
    {
        let rt = fx.runtime_pool().await;
        match ledger_store::schema::verify_definitions_at_startup(&rt).await {
            Err(ledger_core::LedgerError::SchemaIncompatible(m)) => {
                assert!(m.contains("decisions_accepted_has_event"), "{m}")
            }
            other => panic!("vacuous CHECK: start-up should refuse, got {other:?}"),
        }
        match running.ready().await {
            Err(ledger_core::LedgerError::SchemaIncompatible(m)) => {
                assert!(m.contains("decisions_accepted_has_event"), "{m}")
            }
            other => panic!("vacuous CHECK: readiness should refuse, got {other:?}"),
        }
        rt.close().await;
    }
    owner_exec(
        &fx,
        "ALTER TABLE decisions DROP CONSTRAINT decisions_accepted_has_event",
    )
    .await;
    owner_exec(&fx, "ALTER TABLE decisions ADD CONSTRAINT decisions_accepted_has_event CHECK ((decision = 'accepted' AND ref_event_id IS NOT NULL) OR (decision <> 'accepted' AND ref_event_id IS NULL))").await;
    assert_healthy(&fx, "all restored").await;
    running.ready().await.expect("readiness after restore");
    fx.teardown().await;
}

/// Phase 2 (Plan 0006): the runtime identity records validation contexts and records and
/// accepts a candidate under a cited validation with exactly the 0010 grants; the 0010
/// controls the acceptance binding depends on are verified at start-up and readiness.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn validation_persistence_runs_under_the_runtime_identity_and_its_controls_are_verified() {
    use ledger_validation_protocol::{
        BaseKb, Reasoning, RequestedContext, SemanticExecutionContext, ShapeSet, ValidationOutcome,
        ValidatorIdentity,
    };
    let fx = fixture("lp_validation").await;
    fx.migrate_and_grant().await;
    let g = GraphId::new(unique("g").replace('_', "-")).unwrap();
    PgGraphs::new(fx.owner.clone())
        .create(&NewGraph {
            graph_id: g.clone(),
            tenant_id: TenantId::new("tenant-lp").unwrap(),
            knowledge_base_id: None,
            purpose: None,
            status: GraphStatus::Active,
        })
        .await
        .unwrap();
    let running = PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject)
        .await
        .unwrap()
        .with_validation_trust(
            ledger_store::ValidationTrustPolicy::single("urn:sculpin:service:validator").unwrap(),
        );
    let wf = running.workflows();
    let prepared = wf
        .prepare(&PrepareRequest {
            scope: scope(&g, "k1", b"p1"),
            branch: "main".into(),
            expected_head: None,
            requested: patch("<urn:s> <urn:p> \"1\" .", OperationKind::Add),
            activity: "a".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: "m".into(),
        })
        .await
        .unwrap();
    let request = ValidateRequest {
        scope: scope(&g, "k-v", b"validate"),
        candidate: prepared.candidate.clone(),
        requested: RequestedContext::default(),
    };
    let ValidationBegin::Fresh(ticket) = running.validations().begin(&request).await.unwrap()
    else {
        panic!("no validation yet");
    };
    let context = SemanticExecutionContext {
        graph_id: g.clone(),
        candidate_commit: prepared.candidate.clone(),
        candidate_state_digest: ticket.state_digest().clone(),
        base_kb: BaseKb {
            kb_id: "kb".into(),
            revision: "r1".into(),
        },
        ontology: None,
        shapes: ShapeSet {
            id: "shapes".into(),
            version: "1".into(),
        },
        reasoning: Some(Reasoning {
            profile: "none".into(),
            implementation: "pyshacl".into(),
            version: "0.26".into(),
        }),
        sources_revision: None,
        virtual_contexts: vec![],
        validator: ValidatorIdentity {
            service_id: "urn:sculpin:service:validator".into(),
            service_version: "1".into(),
            configuration_version: "1".into(),
        },
    };
    let recorded = running
        .validations()
        .record(
            &request,
            &ticket,
            ValidatorOutcome {
                context,
                outcome: ValidationOutcome::conforms(),
                report_digest: ledger_core::ContentId::for_bytes(b"report"),
                report_reference: None,
            },
        )
        .await
        .expect("record a validation under the runtime identity");
    let accepted = wf
        .accept(&AcceptRequest {
            scope: scope(&g, "k-a", b"accept"),
            branch: "main".into(),
            expected_head: None,
            candidate: prepared.candidate.clone(),
            reason: None,
            validation: ValidationPolicy::Validated {
                validation_id: recorded.validation_id.clone(),
                semantic_environment_id: recorded.environment_id.clone(),
            },
        })
        .await
        .expect("accept under a cited validation with the runtime identity");
    assert_eq!(accepted.ref_version, 1);
    let (linked,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM decision_validations WHERE decision_id = $1")
            .bind(accepted.decision_id)
            .fetch_one(&fx.owner)
            .await
            .unwrap();
    assert_eq!(linked, 1);

    // Drift of the controls the binding depends on (owner statements), each refused at
    // start-up and readiness, healthy again after restoration.
    owner_exec(
        &fx,
        "ALTER TABLE decision_validations DROP CONSTRAINT dv_validation_fk",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "decision→validation FK dropped").await;
    assert!(m.contains("decision_validations FOREIGN KEY"), "{m}");
    owner_exec(&fx, "ALTER TABLE decision_validations ADD CONSTRAINT dv_validation_fk FOREIGN KEY (validation_id, graph_id, candidate_commit) REFERENCES validation_records (validation_id, graph_id, candidate_commit)").await;
    assert_healthy(&fx, "FK restored").await;
    owner_exec(
        &fx,
        "ALTER TABLE validation_records DROP CONSTRAINT vr_context_fk",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "record→context FK dropped").await;
    assert!(m.contains("validation_records FOREIGN KEY"), "{m}");
    owner_exec(&fx, "ALTER TABLE validation_records ADD CONSTRAINT vr_context_fk FOREIGN KEY (context_id, graph_id, candidate_commit, candidate_state_digest, validator_service_id, validator_service_version, validator_configuration_version) REFERENCES semantic_execution_contexts (context_id, graph_id, candidate_commit, candidate_state_digest, validator_service_id, validator_service_version, validator_configuration_version)").await;
    assert_healthy(&fx, "FK restored").await;
    owner_exec(
        &fx,
        "ALTER TABLE validation_records DISABLE TRIGGER validation_records_write_once",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "record write-once guard disabled").await;
    assert!(m.contains("validation_records_write_once"), "{m}");
    owner_exec(
        &fx,
        "ALTER TABLE validation_records ENABLE TRIGGER validation_records_write_once",
    )
    .await;
    assert_healthy(&fx, "guard enabled").await;
    // A vacuous content-address CHECK keeps its name: catalog presence passes, the definition
    // check refuses start-up, the fingerprint refuses readiness, the probe would refuse too.
    owner_exec(
        &fx,
        "ALTER TABLE validation_records DROP CONSTRAINT vr_content_addressed",
    )
    .await;
    owner_exec(
        &fx,
        "ALTER TABLE validation_records ADD CONSTRAINT vr_content_addressed CHECK (true)",
    )
    .await;
    match PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject).await {
        Err(LedgerError::SchemaIncompatible(m)) => {
            assert!(m.contains("vr_content_addressed"), "{m}")
        }
        other => panic!("vacuous content-address CHECK must refuse start-up: {other:?}"),
    }
    assert!(matches!(
        running.ready().await,
        Err(LedgerError::SchemaIncompatible(_))
    ));
    owner_exec(
        &fx,
        "ALTER TABLE validation_records DROP CONSTRAINT vr_content_addressed",
    )
    .await;
    owner_exec(&fx, "ALTER TABLE validation_records ADD CONSTRAINT vr_content_addressed CHECK (validation_id = 'sha256:' || encode(sha256(canonical_bytes), 'hex'))").await;
    PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject)
        .await
        .expect("healthy after restoring the CHECK");
    // The idempotency operation CHECK (0010 admits `validate`, 0012 the branch operations) must be present;
    // a `validate` row already exists here, so a pre-0010 definition cannot even be re-added.
    owner_exec(
        &fx,
        "ALTER TABLE idempotency DROP CONSTRAINT idempotency_operation",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "idempotency_operation CHECK dropped").await;
    assert!(m.contains("idempotency_operation"), "{m}");
    owner_exec(&fx, "ALTER TABLE idempotency ADD CONSTRAINT idempotency_operation CHECK (operation IN ('prepare', 'accept', 'reject', 'validate', 'branch_create', 'branch_delete', 'branch_restore', 'merge_propose', 'merge_apply'))").await;
    assert_healthy(&fx, "operation CHECK restored").await;
    // A runtime that gained UPDATE on a validation column, or lost a required INSERT column,
    // is refused as an identity drift.
    owner_exec(
        &fx,
        &format!(
            "GRANT UPDATE (outcome) ON validation_records TO {}",
            fx.role
        ),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "UPDATE (outcome) granted").await;
    assert!(m.contains("validation_records.outcome"), "{m}");
    owner_exec(
        &fx,
        &format!(
            "REVOKE UPDATE (outcome) ON validation_records FROM {}",
            fx.role
        ),
    )
    .await;
    owner_exec(
        &fx,
        &format!(
            "REVOKE INSERT (canonical_bytes) ON semantic_execution_contexts FROM {}",
            fx.role
        ),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "INSERT (canonical_bytes) revoked").await;
    assert!(
        m.contains("semantic_execution_contexts.canonical_bytes"),
        "{m}"
    );
    let mut conn = sqlx::postgres::PgConnection::connect(&fx.owner_db_url)
        .await
        .unwrap();
    schema::grant_runtime_role(&mut conn, &fx.role)
        .await
        .unwrap();
    conn.close().await.unwrap();
    assert_healthy(&fx, "re-granted").await;
    running
        .ready()
        .await
        .expect("readiness after every restoration");
    fx.teardown().await;
}

/// A named CHECK replaced by a vacuous `CHECK (true)` keeps its name, so the lock-free
/// catalog check passes; start-up must refuse by definition (naming the constraint) and a
/// running server's readiness by the changed fingerprint. Restored, both serve again.
async fn assert_vacuous_check_refused(
    fx: &Fixture,
    running: &PostgresLedgerStore,
    table: &str,
    name: &str,
    real: &str,
) {
    owner_exec(fx, &format!("ALTER TABLE {table} DROP CONSTRAINT {name}")).await;
    let m = assert_refused_by_schema(fx, running, &format!("{name} dropped")).await;
    assert!(m.contains(name) && m.contains("missing"), "{name}: {m}");
    owner_exec(
        fx,
        &format!("ALTER TABLE {table} ADD CONSTRAINT {name} CHECK (true)"),
    )
    .await;
    {
        let rt = fx.runtime_pool().await;
        schema::verify(&rt)
            .await
            .unwrap_or_else(|e| panic!("{name}: catalog facts alone still pass: {e}"));
        rt.close().await;
    }
    match PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject).await {
        Err(LedgerError::SchemaIncompatible(m)) => assert!(m.contains(name), "{name}: {m}"),
        other => panic!("{name} CHECK (true) must refuse start-up: {other:?}"),
    }
    match running.ready().await {
        Err(LedgerError::SchemaIncompatible(m)) => {
            assert!(m.contains("changed since start-up"), "{name}: {m}")
        }
        other => panic!("{name} CHECK (true) must refuse readiness: {other:?}"),
    }
    owner_exec(fx, &format!("ALTER TABLE {table} DROP CONSTRAINT {name}")).await;
    owner_exec(
        fx,
        &format!("ALTER TABLE {table} ADD CONSTRAINT {name} CHECK ({real})"),
    )
    .await;
    PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject)
        .await
        .unwrap_or_else(|e| panic!("{name} restored: start-up serves again: {e}"));
    running
        .ready()
        .await
        .unwrap_or_else(|e| panic!("{name} restored: readiness again: {e}"));
}

/// ADR-0017: a logical restore recreates every CHECK from its deparsed text. Recreating each
/// one exactly that way (what `pg_restore` does) keeps the database servable — PostgreSQL
/// flattens the nested `AND` of `BETWEEN`-style bounds, which the verifier accepts as the
/// restored form — while a same-named constraint with a changed bound is still refused.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn checks_recreated_as_a_logical_restore_does_are_accepted_and_changes_still_refused() {
    let fx = fixture("lp_restore_checks").await;
    fx.migrate_and_grant().await;
    let running = PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject)
        .await
        .expect("healthy database serves");
    let checks: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.relname::text, con.conname::text, pg_get_constraintdef(con.oid) FROM pg_constraint con \
         JOIN pg_class c ON c.oid = con.conrelid WHERE con.contype = 'c' \
           AND c.relnamespace = 'public'::regnamespace ORDER BY 1, 2",
    )
    .fetch_all(&fx.owner)
    .await
    .unwrap();
    assert!(checks.len() >= 56, "{}", checks.len());
    let mut changed = 0;
    for (table, name, def) in &checks {
        owner_exec(
            &fx,
            &format!("ALTER TABLE {table} DROP CONSTRAINT {name}, ADD CONSTRAINT {name} {def}"),
        )
        .await;
        let again: String = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(con.oid) FROM pg_constraint con JOIN pg_class c \
             ON c.oid = con.conrelid WHERE c.relname = $1 AND con.conname = $2",
        )
        .bind(table)
        .bind(name)
        .fetch_one(&fx.owner)
        .await
        .unwrap();
        if &again != def {
            changed += 1;
        }
    }
    assert_eq!(
        changed, 7,
        "the re-parse flattens exactly the BETWEEN-style bounds"
    );
    // A server started before the recreation sees changed fingerprints and stops serving
    // until verified again (drift detection); a server started on the restored database —
    // what a restore always means — verifies the restored forms and serves.
    assert!(
        running.ready().await.is_err(),
        "drift since start-up is refused"
    );
    assert_healthy(&fx, "every CHECK recreated from its deparsed text").await;
    let running = PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject)
        .await
        .expect("a server started on the restored form serves");
    running
        .ready()
        .await
        .expect("readiness on the restored form");
    // The restored form is accepted only as the exact flattened text: a changed bound is not.
    owner_exec(
        &fx,
        "ALTER TABLE refs DROP CONSTRAINT refs_branch_bounds, ADD CONSTRAINT refs_branch_bounds \
         CHECK (octet_length(branch) >= 1 AND octet_length(branch) <= 129 \
         AND branch ~ '^[A-Za-z0-9._/-]+$')",
    )
    .await;
    match PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject).await {
        Err(LedgerError::SchemaIncompatible(m)) => assert!(m.contains("refs_branch_bounds"), "{m}"),
        other => panic!("a widened branch bound must refuse start-up: {other:?}"),
    }
    match running.ready().await {
        Err(LedgerError::SchemaIncompatible(m)) => {
            assert!(m.contains("changed since start-up"), "{m}")
        }
        other => panic!("a widened branch bound must refuse readiness: {other:?}"),
    }
    // Whitespace inside a literal is content: a space added to the branch alphabet is refused.
    owner_exec(
        &fx,
        "ALTER TABLE refs DROP CONSTRAINT refs_branch_bounds, ADD CONSTRAINT refs_branch_bounds \
         CHECK (octet_length(branch) >= 1 AND octet_length(branch) <= 128 \
         AND branch ~ '^[A-Za-z0-9._/ -]+$')",
    )
    .await;
    match PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject).await {
        Err(LedgerError::SchemaIncompatible(m)) => assert!(m.contains("refs_branch_bounds"), "{m}"),
        other => panic!("a widened branch alphabet must refuse start-up: {other:?}"),
    }
    // A quoted identifier is compared exactly: a CHECK moved to a look-alike column
    // `"position::text"` does not pass for the one on `position`.
    owner_exec(
        &fx,
        "ALTER TABLE commit_parents ADD COLUMN \"position::text\" smallint NOT NULL DEFAULT 0",
    )
    .await;
    owner_exec(
        &fx,
        "ALTER TABLE commit_parents DROP CONSTRAINT commit_parents_position, \
         ADD CONSTRAINT commit_parents_position CHECK (\"position::text\" IN (0, 1))",
    )
    .await;
    match PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject).await {
        Err(LedgerError::SchemaIncompatible(m)) => {
            assert!(m.contains("commit_parents"), "{m}")
        }
        other => panic!("a CHECK on a look-alike column must refuse start-up: {other:?}"),
    }
}

/// Composite foreign keys are `MATCH SIMPLE`: a key column that became nullable lets a row
/// skip the foreign key entirely (e.g. a `decision_validations` link with NULL graph and
/// candidate). Nullability is therefore verified structurally, at start-up and readiness.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn nullable_key_columns_are_refused_at_startup_and_readiness() {
    let fx = fixture("lp_not_null").await;
    fx.migrate_and_grant().await;
    let running = PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject)
        .await
        .expect("healthy database serves");
    assert_healthy(&fx, "fresh migration").await;
    // The inventory is exactly what the migrations declare: a NOT NULL column added by a
    // future migration without extending the inventory fails here, not silently.
    let declared: std::collections::BTreeSet<(String, String)> = sqlx::query_as(
        "SELECT c.relname::text, a.attname::text FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind = 'r' AND a.attnum > 0 AND NOT a.attisdropped \
           AND a.attnotnull AND c.relname <> '_sqlx_migrations'",
    )
    .fetch_all(&fx.owner)
    .await
    .unwrap()
    .into_iter()
    .collect();
    let expected: std::collections::BTreeSet<(String, String)> =
        ledger_store::schema::EXPECTED_NOT_NULL
            .iter()
            .flat_map(|(t, cols)| cols.iter().map(|c| ((*t).to_owned(), (*c).to_owned())))
            .collect();
    assert_eq!(
        declared, expected,
        "EXPECTED_NOT_NULL drifted from the migrations"
    );
    for (table, column) in [
        ("decision_validations", "graph_id"),
        ("decision_validations", "candidate_commit"),
        ("validation_records", "candidate_commit"),
        ("semantic_execution_contexts", "candidate_state_digest"),
        ("idempotency", "graph_id"),
    ] {
        owner_exec(
            &fx,
            &format!("ALTER TABLE {table} ALTER COLUMN {column} DROP NOT NULL"),
        )
        .await;
        let m =
            assert_refused_by_schema(&fx, &running, &format!("{table}.{column} nullable")).await;
        assert!(
            m.contains(&format!("{table}.{column} must be NOT NULL")),
            "{m}"
        );
        owner_exec(
            &fx,
            &format!("ALTER TABLE {table} ALTER COLUMN {column} SET NOT NULL"),
        )
        .await;
        assert_healthy(&fx, &format!("{table}.{column} restored")).await;
        running.ready().await.expect("readiness after restore");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn drifted_phase2_identity_constraints_and_checks_are_refused_at_startup_and_readiness() {
    let fx = fixture("lp_p2_drift").await;
    fx.migrate_and_grant().await;
    let running = PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject)
        .await
        .expect("healthy database serves");
    assert_healthy(&fx, "fresh migration").await;

    // Content addressing of contexts, the record's outcome shape and the idempotency shape.
    assert_vacuous_check_refused(
        &fx,
        &running,
        "semantic_execution_contexts",
        "sec_content_addressed",
        "context_id = 'sha256:' || encode(sha256(canonical_bytes), 'hex')",
    )
    .await;
    assert_vacuous_check_refused(
        &fx,
        &running,
        "validation_records",
        "vr_outcome_shape",
        "(outcome = 'conforms' AND violation_count >= 0) OR (outcome = 'violations' AND violation_count >= 1)",
    )
    .await;
    assert_vacuous_check_refused(
        &fx,
        &running,
        "idempotency",
        "idempotency_validation_shape",
        "((operation = 'validate') = (result_kind = 'validated')) \
         AND ((result_kind = 'validated') = (result_validation_id IS NOT NULL)) \
         AND (result_kind <> 'validated' OR result_commit IS NOT NULL)",
    )
    .await;

    // The context identity the record FK targets: dropping it takes vr_context_fk along
    // (CASCADE). Both are refused, each on its own.
    const SEC_IDENTITY: &str = "ALTER TABLE semantic_execution_contexts ADD CONSTRAINT sec_identity \
        UNIQUE (context_id, graph_id, candidate_commit, candidate_state_digest, validator_service_id, \
        validator_service_version, validator_configuration_version)";
    const VR_CONTEXT_FK: &str = "ALTER TABLE validation_records ADD CONSTRAINT vr_context_fk \
        FOREIGN KEY (context_id, graph_id, candidate_commit, candidate_state_digest, validator_service_id, \
        validator_service_version, validator_configuration_version) REFERENCES semantic_execution_contexts \
        (context_id, graph_id, candidate_commit, candidate_state_digest, validator_service_id, \
        validator_service_version, validator_configuration_version)";
    owner_exec(
        &fx,
        "ALTER TABLE semantic_execution_contexts DROP CONSTRAINT sec_identity CASCADE",
    )
    .await;
    let fk_left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_constraint WHERE conname = 'vr_context_fk'")
            .fetch_one(&fx.owner)
            .await
            .unwrap();
    assert_eq!(fk_left, 0, "CASCADE dropped the dependent FK");
    let m = assert_refused_by_schema(&fx, &running, "sec_identity dropped (CASCADE)").await;
    assert!(m.contains("semantic_execution_contexts UNIQUE"), "{m}");
    owner_exec(&fx, SEC_IDENTITY).await;
    let m = assert_refused_by_schema(&fx, &running, "vr_context_fk still missing").await;
    assert!(
        m.contains("validation_records FOREIGN KEY") && m.contains("semantic_execution_contexts"),
        "{m}"
    );
    owner_exec(&fx, VR_CONTEXT_FK).await;
    assert_healthy(&fx, "context identity and record FK restored").await;
    running.ready().await.expect("readiness after restore");

    // The decision identity the decision_validations FK targets, likewise.
    owner_exec(
        &fx,
        "ALTER TABLE decisions DROP CONSTRAINT decisions_identity CASCADE",
    )
    .await;
    let fk_left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_constraint WHERE conname = 'dv_decision_fk'")
            .fetch_one(&fx.owner)
            .await
            .unwrap();
    assert_eq!(fk_left, 0, "CASCADE dropped the dependent FK");
    let m = assert_refused_by_schema(&fx, &running, "decisions_identity dropped (CASCADE)").await;
    assert!(
        m.contains("decisions UNIQUE [\"decision_id\", \"graph_id\", \"candidate_commit\"]"),
        "{m}"
    );
    owner_exec(
        &fx,
        "ALTER TABLE decisions ADD CONSTRAINT decisions_identity UNIQUE (decision_id, graph_id, candidate_commit)",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "dv_decision_fk still missing").await;
    assert!(
        m.contains("decision_validations FOREIGN KEY [\"decision_id\""),
        "{m}"
    );
    owner_exec(&fx, "ALTER TABLE decision_validations ADD CONSTRAINT dv_decision_fk FOREIGN KEY (decision_id, graph_id, candidate_commit) REFERENCES decisions (decision_id, graph_id, candidate_commit)").await;
    assert_healthy(&fx, "decision identity and link FK restored").await;
    running.ready().await.expect("readiness after restore");

    // The idempotency → validation record FK.
    owner_exec(
        &fx,
        "ALTER TABLE idempotency DROP CONSTRAINT idempotency_validation_fk",
    )
    .await;
    let m = assert_refused_by_schema(&fx, &running, "idempotency_validation_fk dropped").await;
    assert!(
        m.contains("idempotency FOREIGN KEY [\"result_validation_id\""),
        "{m}"
    );
    owner_exec(&fx, "ALTER TABLE idempotency ADD CONSTRAINT idempotency_validation_fk FOREIGN KEY (result_validation_id, graph_id, result_commit) REFERENCES validation_records (validation_id, graph_id, candidate_commit)").await;
    assert_healthy(&fx, "idempotency FK restored").await;
    running
        .ready()
        .await
        .expect("readiness after every restoration");
    PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject)
        .await
        .expect("start-up after every restoration");
    fx.teardown().await;
}

/// The owner-side definition of a named constraint, index or function (for restoration).
async fn catalog_def(fx: &Fixture, sql: &str) -> String {
    sqlx::query_scalar(sql)
        .fetch_one(&fx.owner)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Migration 0011's controls (ADR-0021): guard triggers and their function bodies, the
/// partial one-stream-per-cognitive-graph index, the progress and tenant FKs and the
/// projection CHECKs are each refused at start-up and readiness when weakened, and the
/// database is healthy again after restoration.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn weakened_projection_controls_are_refused_at_startup_and_readiness() {
    let fx = fixture("lp_proj_drift").await;
    fx.migrate_and_grant().await;
    let running = PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject)
        .await
        .expect("healthy database serves");
    assert_healthy(&fx, "fresh migration").await;
    // The projector identity on the same database: its readiness must refuse the same drift.
    let projector_role = unique("lp_pj");
    owner_exec(
        &fx,
        &format!("CREATE ROLE {projector_role} LOGIN PASSWORD 'pj-test-secret'"),
    )
    .await;
    {
        let mut conn = PgConnection::connect(&fx.owner_db_url).await.unwrap();
        schema::grant_projector_role(&mut conn, &projector_role)
            .await
            .unwrap();
        conn.close().await.unwrap();
    }
    let projector = ledger_store::ProjectionRepository::connect(
        &with_credentials(&fx.owner_db_url, &projector_role, "pj-test-secret"),
        DbSessionLimits::default(),
    )
    .await
    .expect("the projector identity verifies");
    projector.ready().await.expect("projector ready");
    // A privilege granted after start-up is refused by the projector's readiness.
    owner_exec(&fx, &format!("GRANT INSERT ON refs TO {projector_role}")).await;
    match projector.ready().await {
        Err(LedgerError::RuntimeIdentity(m)) => assert!(m.contains("refs"), "{m}"),
        other => panic!("projector readiness must refuse a drifted grant: {other:?}"),
    }
    owner_exec(&fx, &format!("REVOKE INSERT ON refs FROM {projector_role}")).await;
    projector
        .ready()
        .await
        .expect("projector ready after revoke");

    // Guard triggers disabled.
    for (trigger, table) in [
        ("projection_state_guard", "projection_state"),
        ("outbox_delivery_monotonic", "projection_outbox"),
    ] {
        owner_exec(
            &fx,
            &format!("ALTER TABLE {table} DISABLE TRIGGER {trigger}"),
        )
        .await;
        let m = assert_refused_by_schema(&fx, &running, &format!("{trigger} disabled")).await;
        assert!(m.contains(trigger), "{m}");
        owner_exec(
            &fx,
            &format!("ALTER TABLE {table} ENABLE TRIGGER {trigger}"),
        )
        .await;
        assert_healthy(&fx, &format!("{trigger} enabled")).await;
    }

    // Guard function bodies replaced by a pass-through.
    for function in ["projection_state_guard", "outbox_delivery_is_monotonic"] {
        let real = catalog_def(
            &fx,
            &format!("SELECT pg_get_functiondef('public.{function}()'::regprocedure)"),
        )
        .await;
        owner_exec(
            &fx,
            &format!(
                "CREATE OR REPLACE FUNCTION public.{function}() RETURNS trigger \
                 LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$"
            ),
        )
        .await;
        let m = assert_refused_by_schema(&fx, &running, &format!("{function} replaced")).await;
        assert!(m.contains(function), "{m}");
        owner_exec(&fx, &real).await;
        assert_healthy(&fx, &format!("{function} restored")).await;
    }

    // The partial unique index: dropped, or recreated without its predicate.
    let index = catalog_def(
        &fx,
        "SELECT pg_get_indexdef('public.projection_state_graph_unique'::regclass)",
    )
    .await;
    owner_exec(&fx, "DROP INDEX projection_state_graph_unique").await;
    let m = assert_refused_by_schema(&fx, &running, "graph-unique index dropped").await;
    assert!(m.contains("projection_state_graph_unique"), "{m}");
    owner_exec(
        &fx,
        "CREATE UNIQUE INDEX projection_state_graph_unique ON projection_state (target_id, cognitive_graph) WHERE status = 'active'",
    )
    .await;
    // Same catalog shape, narrower predicate (two blocked streams could then share a graph):
    // refused by deparse at start-up and by fingerprint on readiness.
    match PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject).await {
        Err(LedgerError::SchemaIncompatible(m)) => {
            assert!(
                m.contains("projection_state_graph_unique") && m.contains("predicate"),
                "{m}"
            )
        }
        other => panic!("narrowed graph-unique predicate must refuse start-up: {other:?}"),
    }
    match running.ready().await {
        Err(LedgerError::SchemaIncompatible(m)) => {
            assert!(m.contains("projection_state_graph_unique"), "{m}")
        }
        other => panic!("narrowed graph-unique predicate must refuse readiness: {other:?}"),
    }
    assert!(
        matches!(
            projector.ready().await,
            Err(LedgerError::SchemaIncompatible(_))
        ),
        "narrowed graph-unique predicate must refuse the projector's readiness"
    );
    owner_exec(&fx, "DROP INDEX projection_state_graph_unique").await;
    owner_exec(&fx, &index).await;
    assert_healthy(&fx, "graph-unique index restored").await;

    // The progress and tenant FKs.
    for fk in [
        "projection_state_progress_fk",
        "projection_state_graph_tenant_fk",
    ] {
        let def = catalog_def(
            &fx,
            &format!("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = '{fk}'"),
        )
        .await;
        owner_exec(
            &fx,
            &format!("ALTER TABLE projection_state DROP CONSTRAINT {fk}"),
        )
        .await;
        let m = assert_refused_by_schema(&fx, &running, &format!("{fk} dropped")).await;
        assert!(m.contains("projection_state FOREIGN KEY"), "{m}");
        owner_exec(
            &fx,
            &format!("ALTER TABLE projection_state ADD CONSTRAINT {fk} {def}"),
        )
        .await;
        assert_healthy(&fx, &format!("{fk} restored")).await;
    }

    // Projection CHECKs dropped, or replaced by vacuous ones under the same name.
    for check in [
        "ps_status",
        "ps_progress_shape",
        "ps_lease_shape",
        "ps_error_code_format",
        "ps_counters",
    ] {
        let def = catalog_def(
            &fx,
            &format!(
                "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = '{check}'"
            ),
        )
        .await;
        owner_exec(
            &fx,
            &format!("ALTER TABLE projection_state DROP CONSTRAINT {check}"),
        )
        .await;
        let m = assert_refused_by_schema(&fx, &running, &format!("{check} dropped")).await;
        assert!(m.contains(check), "{m}");
        owner_exec(
            &fx,
            &format!("ALTER TABLE projection_state ADD CONSTRAINT {check} CHECK (true)"),
        )
        .await;
        match PostgresLedgerStore::connect(&fx.runtime_db_url, V1Binding::Reject).await {
            Err(LedgerError::SchemaIncompatible(m)) => assert!(m.contains(check), "{m}"),
            other => panic!("vacuous {check} must refuse start-up: {other:?}"),
        }
        assert!(
            matches!(
                running.ready().await,
                Err(LedgerError::SchemaIncompatible(_))
            ),
            "vacuous {check} must refuse readiness"
        );
        assert!(
            matches!(
                projector.ready().await,
                Err(LedgerError::SchemaIncompatible(_))
            ),
            "vacuous {check} must refuse the projector's readiness"
        );
        owner_exec(
            &fx,
            &format!("ALTER TABLE projection_state DROP CONSTRAINT {check}"),
        )
        .await;
        owner_exec(
            &fx,
            &format!("ALTER TABLE projection_state ADD CONSTRAINT {check} {def}"),
        )
        .await;
        assert_healthy(&fx, &format!("{check} restored")).await;
    }
    running.ready().await.expect("readiness after restore");
    projector
        .ready()
        .await
        .expect("projector readiness after restore");
    // Both identity models are exhaustive: a privilege on any other public table refuses.
    owner_exec(&fx, "CREATE TABLE public.lp_extra (x int)").await;
    for role in [fx.role.clone(), projector_role.clone()] {
        owner_exec(&fx, &format!("GRANT SELECT ON public.lp_extra TO {role}")).await;
    }
    let m = assert_refused_by_identity(&fx, "runtime SELECT on an unlisted table").await;
    assert!(m.contains("lp_extra"), "{m}");
    match projector.ready().await {
        Err(LedgerError::RuntimeIdentity(m)) => assert!(m.contains("lp_extra"), "{m}"),
        other => panic!("projector readiness must refuse an unlisted grant: {other:?}"),
    }
    owner_exec(&fx, "DROP TABLE public.lp_extra").await;
    // …and on any other schema's relations too (not only `public`).
    owner_exec(&fx, "CREATE SCHEMA lp_private").await;
    owner_exec(&fx, "CREATE TABLE lp_private.secrets (x int)").await;
    // A PUBLIC grant in a schema the role cannot use is unreachable: still healthy.
    owner_exec(&fx, "GRANT SELECT ON lp_private.secrets TO PUBLIC").await;
    assert_healthy(&fx, "PUBLIC grant behind a schema without USAGE").await;
    owner_exec(&fx, "REVOKE SELECT ON lp_private.secrets FROM PUBLIC").await;
    owner_exec(
        &fx,
        &format!("GRANT USAGE ON SCHEMA lp_private TO {}", fx.role),
    )
    .await;
    owner_exec(
        &fx,
        &format!("GRANT SELECT ON lp_private.secrets TO {}", fx.role),
    )
    .await;
    let m = assert_refused_by_identity(&fx, "runtime SELECT on another schema").await;
    assert!(m.contains("lp_private.secrets"), "{m}");
    owner_exec(&fx, "DROP SCHEMA lp_private CASCADE").await;
    drop(running);
    projector.pool().close().await;
    drop(projector);
    let admin = fx.admin.clone();
    fx.teardown().await;
    sqlx::query(&format!("DROP ROLE {projector_role}"))
        .execute(&admin)
        .await
        .unwrap();
}
