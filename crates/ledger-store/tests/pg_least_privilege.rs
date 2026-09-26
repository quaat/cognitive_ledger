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
    PrepareRequest, RejectRequest, RequestScope, V1Binding, ValidationPolicy, schema,
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
    ] {
        assert_denied(rt, sql).await;
    }
    for sql in [
        "SET session_replication_role = replica",
        "CREATE FUNCTION smuggled() RETURNS int LANGUAGE sql AS 'SELECT 1'",
        "SELECT setval('proposals_proposal_id_seq', 1)",
        "INSERT INTO refs (graph_id, branch, head, version, protected) VALUES ('x', 'y', 'z', 1, false)",
        "INSERT INTO projection_outbox (graph_id, branch, commit_id, ref_version, event_kind, ref_event_id, delivered_at) VALUES ('x','y','z',1,'ref_advanced',1, now())",
        "INSERT INTO decisions (proposal_id, graph_id, branch, candidate_commit, decision, tenant_id, principal_id, principal_type, reason, validation_ids, decided_at) VALUES (1,'x','y','z','rejected','t','p','agent','r','{}', now())",
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
    let (checksum9,): (Vec<u8>,) =
        sqlx::query_as("SELECT checksum FROM _sqlx_migrations WHERE version = 9")
            .fetch_one(&f.owner)
            .await
            .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 9")
        .execute(&f.owner)
        .await
        .unwrap();
    let err = schema::migrate_all(rt).await.unwrap_err();
    assert!(
        matches!(&err, LedgerError::Storage(m) if m.contains("permission denied") || m.contains("must be owner")),
        "{err:?}"
    );
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM _sqlx_migrations WHERE version = 9")
        .fetch_one(&f.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "the runtime must not have applied anything");
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9, 'ref movement integrity', true, $1, 0)",
    )
    .bind(&checksum9)
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
        matches!(&err, LedgerError::SchemaIncompatible(m) if m.contains("0007") && m.contains("requires 0009")),
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
        "refs.protected",
        "refs.created_at",
        "refs.updated_at",
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
    ] {
        assert!(
            !granted.contains(&absent.to_owned()),
            "{absent} must not be insertable"
        );
    }
    for present in [
        "refs.head",
        "refs.version",
        "proposals.candidate_commit",
        "decisions.ref_event_id",
        "idempotency.request_digest",
        "immutable_objects.bytes",
    ] {
        assert!(
            granted.contains(&present.to_owned()),
            "{present} must be insertable"
        );
    }
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
