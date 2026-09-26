//! Real-PostgreSQL evidence for the graph authority schema (ADR-0010, migration 0004):
//! global `graph_id` uniqueness, immutable tenant binding, many graphs per KB, FK
//! integrity for refs and indexed commits, RESTRICT on delete, and the upgrade policy
//! (bootstrap `default` is backfilled; unknown graph ids fail the migration). Each test
//! that needs a controlled schema state creates its own throwaway database, so the
//! shared test database is never mutated destructively.
#![cfg(feature = "postgres")]

use ledger_core::{
    Actor, AnyCommit, Commit, CommitV2, GraphId, ImmutableStore, LedgerError, LedgerTimestamp,
    PatchId, PrincipalId, PrincipalType, TenantId,
};
use ledger_rdf::{Operation, OperationKind, Patch};
use ledger_store::{GraphStatus, NewGraph, PgGraphs, PostgresImmutableStore, V1Binding};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::borrow::Cow;
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
    format!("{prefix}_{}_{nanos}", std::process::id())
}

/// The test URL with its database name replaced (`…/ledger?…` → `…/<name>?…`).
fn url_for_database(base: &str, name: &str) -> String {
    let (head, query) = match base.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (base, None),
    };
    let slash = head.rfind('/').expect("database url has a path");
    let mut url = format!("{}/{name}", &head[..slash]);
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    url
}

async fn fresh_database(prefix: &str) -> (String, PgPool) {
    let base = database_url();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    let name = unique(prefix).to_lowercase();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let url = url_for_database(&base, &name);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    (url, pool)
}

/// A migrator restricted to versions `<= upto` (the historical "deployed" state).
fn migrator_up_to(upto: i64) -> sqlx::migrate::Migrator {
    let mut migrator = sqlx::migrate!("../../migrations");
    migrator.migrations = Cow::Owned(
        migrator
            .migrations
            .iter()
            .filter(|m| m.version <= upto)
            .cloned()
            .collect(),
    );
    migrator
}

/// Everything a schema comparison must cover: columns, constraints (with definitions),
/// indexes (with definitions) and triggers — not just table/column names.
async fn schema_snapshot(pool: &PgPool) -> String {
    let mut out = String::new();
    for row in sqlx::query(
        "SELECT table_name, column_name, data_type, is_nullable, coalesce(column_default, '') AS d \
         FROM information_schema.columns WHERE table_schema = 'public' \
         AND table_name <> '_sqlx_migrations' ORDER BY table_name, ordinal_position",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    {
        let t: String = row.get("table_name");
        let c: String = row.get("column_name");
        let ty: String = row.get("data_type");
        let n: String = row.get("is_nullable");
        let d: String = row.get("d");
        out.push_str(&format!("column {t}.{c} {ty} nullable={n} default={d}\n"));
    }
    for row in sqlx::query(
        "SELECT conrelid::regclass::text AS t, conname, pg_get_constraintdef(oid) AS def \
         FROM pg_constraint WHERE connamespace = 'public'::regnamespace \
         AND conrelid::regclass::text <> '_sqlx_migrations' ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    {
        let t: String = row.get("t");
        let n: String = row.get("conname");
        let d: String = row.get("def");
        out.push_str(&format!("constraint {t}.{n} {d}\n"));
    }
    for row in sqlx::query(
        "SELECT tablename, indexname, indexdef FROM pg_indexes WHERE schemaname = 'public' \
         AND tablename <> '_sqlx_migrations' ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    {
        let t: String = row.get("tablename");
        let n: String = row.get("indexname");
        let d: String = row.get("indexdef");
        out.push_str(&format!("index {t}.{n} {d}\n"));
    }
    for row in sqlx::query(
        "SELECT tgrelid::regclass::text AS t, tgname, pg_get_triggerdef(oid) || ' enabled=' || tgenabled::text AS def \
         FROM pg_trigger WHERE NOT tgisinternal ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    {
        let t: String = row.get("t");
        let n: String = row.get("tgname");
        let d: String = row.get("def");
        out.push_str(&format!("trigger {t}.{n} {d}\n"));
    }
    for row in sqlx::query(
        "SELECT proname, pg_get_functiondef(oid) AS def FROM pg_proc \
         WHERE pronamespace = 'public'::regnamespace ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    {
        let n: String = row.get("proname");
        let d: String = row.get("def");
        out.push_str(&format!("function {n} {d}\n"));
    }
    out
}

/// SQLSTATE of a database error, so tests assert the *kind* of refusal, not just `is_err`.
fn sqlstate(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> String {
    match result {
        Err(sqlx::Error::Database(e)) => e.code().map(|c| c.to_string()).unwrap_or_default(),
        Err(other) => panic!("expected a database error, got {other:?}"),
        Ok(_) => panic!("expected the statement to be refused"),
    }
}
const FK_VIOLATION: &str = "23503";
const UNIQUE_VIOLATION: &str = "23505";
const INTEGRITY_VIOLATION: &str = "23000";

fn new_graph(graph: &str, tenant: &str, kb: Option<&str>) -> NewGraph {
    NewGraph {
        graph_id: GraphId::new(graph).unwrap(),
        tenant_id: TenantId::new(tenant).unwrap(),
        knowledge_base_id: kb.map(str::to_owned),
        purpose: Some("test".into()),
        status: GraphStatus::Active,
    }
}

fn patch(value: &str) -> Patch {
    Patch::new([Operation {
        kind: OperationKind::Add,
        quad: format!("<urn:s> <urn:p> \"{value}\" .").parse().unwrap(),
    }])
    .unwrap()
}

fn v2(graph: &str, patch: &PatchId, message: &str) -> AnyCommit {
    AnyCommit::V2(CommitV2 {
        graph_id: GraphId::new(graph).unwrap(),
        parents: vec![],
        patch: patch.clone(),
        actor: Actor {
            principal_id: PrincipalId::new("urn:sculpin:agent:test").unwrap(),
            principal_type: PrincipalType::Agent,
            on_behalf_of: None,
        },
        activity: "test".into(),
        event_time: None,
        recorded_at: LedgerTimestamp::parse_rfc3339("2026-09-26T00:00:00Z").unwrap(),
        evidence_refs: vec![],
        source_system: None,
        message: message.into(),
    })
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn graph_id_is_globally_unique_and_tenant_binding_is_immutable() {
    let _ = IGNORE;
    let store = PostgresImmutableStore::connect(&database_url(), V1Binding::Reject)
        .await
        .unwrap();
    let graphs = PgGraphs::new(store.pool().clone());
    let graph = unique("g");

    // 1. Same graph_id under different tenants fails.
    graphs
        .create(&new_graph(&graph, "tenant-a", None))
        .await
        .unwrap();
    assert!(matches!(
        graphs.create(&new_graph(&graph, "tenant-b", None)).await,
        Err(LedgerError::GraphAlreadyExists(_))
    ));
    assert!(matches!(
        graphs.create(&new_graph(&graph, "tenant-a", None)).await,
        Err(LedgerError::GraphAlreadyExists(_))
    ));
    let raw_duplicate = sqlx::query(
        "INSERT INTO graphs (graph_id, tenant_id, status) VALUES ($1, 'tenant-b', 'active')",
    )
    .bind(&graph)
    .execute(store.pool())
    .await;
    assert_eq!(sqlstate(raw_duplicate), UNIQUE_VIOLATION);

    // 2. tenant_id (and graph_id) cannot change; metadata/status can.
    let tenant_update = sqlx::query("UPDATE graphs SET tenant_id = 'tenant-b' WHERE graph_id = $1")
        .bind(&graph)
        .execute(store.pool())
        .await;
    match tenant_update {
        Err(sqlx::Error::Database(e)) => {
            assert_eq!(e.code().as_deref(), Some(INTEGRITY_VIOLATION));
            assert!(
                e.message().contains("tenant_id is immutable"),
                "{}",
                e.message()
            );
        }
        other => panic!("tenant binding must be immutable: {other:?}"),
    }
    let rename = sqlx::query("UPDATE graphs SET graph_id = $2 WHERE graph_id = $1")
        .bind(&graph)
        .bind(unique("renamed"))
        .execute(store.pool())
        .await;
    assert_eq!(sqlstate(rename), INTEGRITY_VIOLATION);
    // ON CONFLICT DO UPDATE is an UPDATE path too, and the trigger covers it.
    let upsert = sqlx::query(
        "INSERT INTO graphs (graph_id, tenant_id, status) VALUES ($1, 'tenant-z', 'active') \
         ON CONFLICT (graph_id) DO UPDATE SET tenant_id = EXCLUDED.tenant_id",
    )
    .bind(&graph)
    .execute(store.pool())
    .await;
    assert_eq!(sqlstate(upsert), INTEGRITY_VIOLATION);
    sqlx::query("UPDATE graphs SET status = 'archived', purpose = 'closed' WHERE graph_id = $1")
        .bind(&graph)
        .execute(store.pool())
        .await
        .unwrap();
    let record = graphs
        .get(&GraphId::new(&graph).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.status, GraphStatus::Archived);
    assert_eq!(record.tenant_id.as_str(), "tenant-a");

    // 3. Two graphs under one tenant may reference the same KB.
    let kb = unique("urn:exodus:kb");
    graphs
        .create(&new_graph(&unique("g1"), "tenant-c", Some(&kb)))
        .await
        .unwrap();
    graphs
        .create(&new_graph(&unique("g2"), "tenant-c", Some(&kb)))
        .await
        .unwrap();
    let row = sqlx::query("SELECT count(*) AS n FROM graphs WHERE knowledge_base_id = $1")
        .bind(&kb)
        .fetch_one(store.pool())
        .await
        .unwrap();
    let n: i64 = row.get("n");
    assert_eq!(n, 2);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn refs_and_commits_cannot_reference_unknown_graphs_and_graphs_with_history_cannot_be_deleted()
 {
    let store = PostgresImmutableStore::connect(&database_url(), V1Binding::Reject)
        .await
        .unwrap();
    let pool = store.pool();
    let unknown = unique("nograph");

    // 5. refs cannot reference an unknown graph.
    let dangling_ref =
        sqlx::query("INSERT INTO refs (graph_id, branch, head) VALUES ($1, 'main', $2)")
            .bind(&unknown)
            .bind("sha256:0000000000000000000000000000000000000000000000000000000000000000")
            .execute(pool)
            .await;
    assert_eq!(sqlstate(dangling_ref), FK_VIOLATION);

    // 6. commit_index cannot reference an unknown graph (FK), and 8. the store refuses a
    //    v2 commit whose graph does not exist before writing anything.
    let p = patch(&unique("fk"));
    store
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let commit = v2(&unknown, &p.id(), "graphless");
    assert!(matches!(
        store.put_commit(&commit).await,
        Err(LedgerError::UnknownGraph(_))
    ));
    assert!(
        !store.exists(&commit.id().unwrap().0).await.unwrap(),
        "refused before any write"
    );
    let raw_index = sqlx::query(
        "INSERT INTO commit_index (id, graph_id, version, patch_id, parent_count) \
         VALUES ($1, $2, 2, $3, 0)",
    )
    .bind(p.id().to_string()) // any existing object id works for the FK on id
    .bind(&unknown)
    .bind(p.id().to_string())
    .execute(pool)
    .await;
    assert_eq!(
        sqlstate(raw_index),
        FK_VIOLATION,
        "commit_index.graph_id FK must hold"
    );

    // 7. A graph with a ref or an indexed commit cannot be deleted; an empty one can.
    let graphs = PgGraphs::new(pool.clone());
    let with_ref = unique("withref");
    graphs
        .create(&new_graph(&with_ref, "tenant-a", None))
        .await
        .unwrap();
    let anchor = store
        .put_commit(&v2(&with_ref, &p.id(), "anchor"))
        .await
        .unwrap();
    sqlx::query("INSERT INTO refs (graph_id, branch, head) VALUES ($1, 'main', $2)")
        .bind(&with_ref)
        .bind(anchor.to_string())
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        sqlstate(
            sqlx::query("DELETE FROM graphs WHERE graph_id = $1")
                .bind(&with_ref)
                .execute(pool)
                .await
        ),
        FK_VIOLATION
    );
    let with_commit = unique("withcommit");
    graphs
        .create(&new_graph(&with_commit, "tenant-a", None))
        .await
        .unwrap();
    store
        .put_commit(&v2(&with_commit, &p.id(), "anchor"))
        .await
        .unwrap();
    assert_eq!(
        sqlstate(
            sqlx::query("DELETE FROM graphs WHERE graph_id = $1")
                .bind(&with_commit)
                .execute(pool)
                .await
        ),
        FK_VIOLATION
    );
    let empty = unique("empty");
    graphs
        .create(&new_graph(&empty, "tenant-a", None))
        .await
        .unwrap();
    sqlx::query("DELETE FROM graphs WHERE graph_id = $1")
        .bind(&empty)
        .execute(pool)
        .await
        .unwrap();
    // The bootstrap graph: give it a ref of our own first so this holds regardless of
    // which other suites ran before, then confirm it cannot be deleted.
    let bootstrap_anchor = store
        .put_commit(&v2("default", &p.id(), "bootstrap anchor"))
        .await
        .unwrap();
    sqlx::query("INSERT INTO refs (graph_id, branch, head) VALUES ('default', $1, $2)")
        .bind(unique("bootstrap-anchor"))
        .bind(bootstrap_anchor.to_string())
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        sqlstate(
            sqlx::query("DELETE FROM graphs WHERE graph_id = 'default'")
                .execute(pool)
                .await
        ),
        FK_VIOLATION
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn immutable_tables_are_write_once_and_ref_identity_is_immutable() {
    let store = PostgresImmutableStore::connect(&database_url(), V1Binding::Reject)
        .await
        .unwrap();
    let pool = store.pool();
    let graph = unique("wo");
    PgGraphs::new(pool.clone())
        .create(&new_graph(&graph, "tenant-a", None))
        .await
        .unwrap();
    let p = patch(&unique("wo"));
    store
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let genesis = store
        .put_commit(&v2(&graph, &p.id(), "genesis"))
        .await
        .unwrap();
    let AnyCommit::V2(mut child) = v2(&graph, &p.id(), "child") else {
        unreachable!()
    };
    child.parents = vec![genesis.clone()];
    let child_id = store.put_commit(&AnyCommit::V2(child)).await.unwrap();

    for statement in [
        "UPDATE immutable_objects SET bytes = 'x' WHERE id = $1",
        "DELETE FROM immutable_objects WHERE id = $1",
        "UPDATE commit_index SET version = 1 WHERE id = $1",
        "UPDATE commit_index SET graph_id = 'default' WHERE id = $1",
        "DELETE FROM commit_index WHERE id = $1",
        "UPDATE commit_parents SET parent_id = $1 WHERE commit_id = $1",
        "DELETE FROM commit_parents WHERE commit_id = $1",
    ] {
        let result = sqlx::query(statement)
            .bind(child_id.to_string())
            .execute(pool)
            .await;
        assert_eq!(sqlstate(result), INTEGRITY_VIOLATION, "{statement}");
    }
    // refs: head moves, identity does not.
    let branch = unique("wo-ref");
    sqlx::query("INSERT INTO refs (graph_id, branch, head) VALUES ($1, $2, $3)")
        .bind(&graph)
        .bind(&branch)
        .bind(genesis.to_string())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE refs SET head = $3, version = version + 1 WHERE graph_id = $1 AND branch = $2",
    )
    .bind(&graph)
    .bind(&branch)
    .bind(child_id.to_string())
    .execute(pool)
    .await
    .unwrap();
    let rename =
        sqlx::query("UPDATE refs SET branch = 'renamed' WHERE graph_id = $1 AND branch = $2")
            .bind(&graph)
            .bind(&branch)
            .execute(pool)
            .await;
    assert_eq!(sqlstate(rename), INTEGRITY_VIOLATION);
    let rehome =
        sqlx::query("UPDATE refs SET graph_id = 'default' WHERE graph_id = $1 AND branch = $2")
            .bind(&graph)
            .bind(&branch)
            .execute(pool)
            .await;
    assert_eq!(sqlstate(rehome), INTEGRITY_VIOLATION);
    assert_eq!(store.verify_commits(&[genesis, child_id]).await.unwrap(), 2);
}

/// The ADR-0012 gate check must *fail* on every tampered column, not just pass on healthy
/// rows. Tampering needs the write-once trigger disabled, so this runs in its own database.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn verify_commit_index_detects_every_tampered_column() {
    let (_url, pool) = fresh_database("ledger_tamper").await;
    let store = PostgresImmutableStore::from_pool(pool.clone(), V1Binding::Reject)
        .await
        .unwrap();
    let graphs = PgGraphs::new(pool.clone());
    let graph = unique("t");
    let other = unique("o");
    graphs
        .create(&new_graph(&graph, "tenant-a", None))
        .await
        .unwrap();
    graphs
        .create(&new_graph(&other, "tenant-a", None))
        .await
        .unwrap();
    let p = patch("tamper");
    store
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let genesis = store
        .put_commit(&v2(&graph, &p.id(), "genesis"))
        .await
        .unwrap();
    let AnyCommit::V2(mut child) = v2(&graph, &p.id(), "child") else {
        unreachable!()
    };
    child.parents = vec![genesis.clone()];
    let child_id = store.put_commit(&AnyCommit::V2(child)).await.unwrap();
    assert_eq!(store.verify_commit_index().await.unwrap(), 2);
    for table in ["commit_index", "commit_parents", "immutable_objects"] {
        sqlx::query(&format!(
            "ALTER TABLE {table} DISABLE TRIGGER {table}_write_once"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }

    // Each statement binds $1 = child, $2 = genesis, $3 = the other graph.
    let apply = |sql: &'static str| {
        let pool = pool.clone();
        let child = child_id.to_string();
        let genesis = genesis.to_string();
        let other = other.clone();
        async move {
            sqlx::query(sql)
                .bind(&child)
                .bind(&genesis)
                .bind(&other)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    // Each tamper is applied, checked, and reverted so cases stay independent.
    let cases: [(&'static str, &'static str); 6] = [
        (
            "UPDATE commit_index SET version = 1 WHERE id = $1",
            "UPDATE commit_index SET version = 2 WHERE id = $1",
        ),
        (
            "UPDATE commit_index SET patch_id = $2 WHERE id = $1",
            "UPDATE commit_index SET patch_id = (SELECT patch_id FROM commit_index WHERE id = $2) WHERE id = $1",
        ),
        (
            "UPDATE commit_index SET parent_count = 0 WHERE id = $1",
            "UPDATE commit_index SET parent_count = 1 WHERE id = $1",
        ),
        (
            "UPDATE commit_index SET graph_id = $3 WHERE id = $1",
            "UPDATE commit_index SET graph_id = (SELECT graph_id FROM commit_index WHERE id = $2) WHERE id = $1",
        ),
        (
            "UPDATE commit_parents SET parent_id = $1 WHERE commit_id = $1",
            "UPDATE commit_parents SET parent_id = $2 WHERE commit_id = $1",
        ),
        (
            "DELETE FROM commit_parents WHERE commit_id = $1",
            "INSERT INTO commit_parents (commit_id, position, parent_id) VALUES ($1, 0, $2)",
        ),
    ];
    for (tamper, revert) in cases {
        apply(tamper).await;
        let result = store.verify_commit_index().await;
        assert!(
            matches!(result, Err(LedgerError::CorruptObject { .. })),
            "{tamper}: {result:?}"
        );
        apply(revert).await;
        assert_eq!(store.verify_commit_index().await.unwrap(), 2, "{revert}");
    }
    // A parent re-homed to another graph (both rows still self-consistent) is caught by
    // the relational same-graph check.
    apply("UPDATE commit_index SET graph_id = $3 WHERE id = $2").await;
    assert!(matches!(
        store.verify_commit_index().await,
        Err(LedgerError::CorruptObject { .. })
    ));
    apply("UPDATE commit_index SET graph_id = (SELECT graph_id FROM commit_index WHERE id = $1) WHERE id = $2").await;
    assert_eq!(store.verify_commit_index().await.unwrap(), 2);
    // Damaged bytes behind a healthy-looking index row are caught too.
    apply("UPDATE immutable_objects SET bytes = 'damaged' WHERE id = $1").await;
    assert!(matches!(
        store.verify_commit_index().await,
        Err(LedgerError::CorruptObject { .. })
    ));
    // Restore the commit bytes, then tamper the *patch* the commits reference: a patch
    // that no longer hashes to its id, or no longer decodes as a canonical patch, fails
    // verification of every commit that references it.
    let child_bytes = AnyCommit::V2(CommitV2 {
        graph_id: GraphId::new(&graph).unwrap(),
        parents: vec![genesis.clone()],
        patch: p.id(),
        actor: Actor {
            principal_id: PrincipalId::new("urn:sculpin:agent:test").unwrap(),
            principal_type: PrincipalType::Agent,
            on_behalf_of: None,
        },
        activity: "test".into(),
        event_time: None,
        recorded_at: LedgerTimestamp::parse_rfc3339("2026-09-26T00:00:00Z").unwrap(),
        evidence_refs: vec![],
        source_system: None,
        message: "child".into(),
    })
    .canonical_bytes()
    .unwrap();
    sqlx::query("UPDATE immutable_objects SET bytes = $2 WHERE id = $1")
        .bind(child_id.to_string())
        .bind(child_bytes)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(store.verify_commit_index().await.unwrap(), 2);
    sqlx::query("UPDATE immutable_objects SET bytes = $2 WHERE id = $1")
        .bind(p.id().to_string())
        .bind(b"sculpin-rdf-patch-v1\nA <urn:s> <urn:p> \"tampered\" .\n".as_slice())
        .execute(&pool)
        .await
        .unwrap();
    match store.verify_commit_index().await {
        Err(LedgerError::CorruptObject { reason, .. }) => {
            assert!(reason.contains("do not hash"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    // Hash-correct but non-canonical patch bytes referenced by an indexed commit: the
    // decode branch of verification must fail too. Build the rows directly by SQL, as an
    // older store (pre patch-validity rule) could have left them.
    let noncanonical =
        b"sculpin-rdf-patch-v1\nA <urn:s> <urn:p> \"b\" .\nA <urn:s> <urn:p> \"a\" .\n".to_vec();
    let nc_id = ledger_core::PatchId(ledger_core::ContentId::for_bytes(&noncanonical));
    sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(nc_id.to_string())
        .bind(&noncanonical)
        .execute(&pool)
        .await
        .unwrap();
    let old = AnyCommit::V2(CommitV2 {
        graph_id: GraphId::new(&graph).unwrap(),
        parents: vec![],
        patch: nc_id.clone(),
        actor: Actor {
            principal_id: PrincipalId::new("urn:sculpin:agent:old").unwrap(),
            principal_type: PrincipalType::Agent,
            on_behalf_of: None,
        },
        activity: "legacy".into(),
        event_time: None,
        recorded_at: LedgerTimestamp::parse_rfc3339("2026-09-25T00:00:00Z").unwrap(),
        evidence_refs: vec![],
        source_system: None,
        message: "pre-rule commit".into(),
    });
    let old_id = old.id().unwrap();
    sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(old_id.to_string())
        .bind(old.canonical_bytes().unwrap())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO commit_index (id, graph_id, version, patch_id, parent_count) \
         VALUES ($1, $2, 2, $3, 0)",
    )
    .bind(old_id.to_string())
    .bind(&graph)
    .bind(nc_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    match store.verify_commits(std::slice::from_ref(&old_id)).await {
        Err(LedgerError::CorruptObject { reason, .. }) => {
            assert!(reason.contains("referenced patch is invalid"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn upgrade_from_bootstrap_state_converges_with_clean_install() {
    // 4. Upgrade path: deploy 0001 only, write the bootstrap ref, then deploy 0002–0003 and
    //    write bootstrap v1 history under 'default' as the pre-graph store would have, then
    //    upgrade fully. Compare the resulting schema with a clean install.
    let (_up_url, up) = fresh_database("ledger_up").await;
    migrator_up_to(1).run(&up).await.unwrap();
    // The bootstrap ref points at the v1 commit the pre-0004 store had written; since
    // 0006 the ref FK requires exactly that (a fake head would fail the upgrade guard).
    let p = patch("bootstrap");
    let legacy = AnyCommit::V1(Commit {
        parents: vec![],
        patch: p.id(),
        author: "urn:agent:bootstrap".into(),
        message: "pre-upgrade".into(),
        event_time: "e".into(),
        recorded_time: "r".into(),
    });
    let legacy_bytes = legacy.canonical_bytes().unwrap();
    let legacy_id = legacy.id().unwrap();
    let head_string = legacy_id.to_string();
    let head = head_string.as_str();
    sqlx::query("INSERT INTO refs (graph_id, branch, head) VALUES ('default', 'main', $1)")
        .bind(head)
        .execute(&up)
        .await
        .unwrap();
    migrator_up_to(3).run(&up).await.unwrap();
    for (id, bytes) in [
        (p.id().0.to_string(), p.canonical_bytes()),
        (legacy_id.to_string(), legacy_bytes),
    ] {
        sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
            .bind(id)
            .bind(bytes)
            .execute(&up)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO commit_index (id, graph_id, version, patch_id, parent_count) \
         VALUES ($1, 'default', 1, $2, 0)",
    )
    .bind(legacy_id.to_string())
    .bind(p.id().to_string())
    .execute(&up)
    .await
    .unwrap();

    sqlx::migrate!("../../migrations").run(&up).await.unwrap();

    // 9. Bootstrap history remains valid after upgrade: the ref, the index row, and the
    //    backfilled graph row all agree, and the derived index re-verifies.
    let row = sqlx::query("SELECT tenant_id, status FROM graphs WHERE graph_id = 'default'")
        .fetch_one(&up)
        .await
        .unwrap();
    let tenant: String = row.get("tenant_id");
    let status: String = row.get("status");
    assert_eq!(
        (tenant.as_str(), status.as_str()),
        ("bootstrap", "bootstrap")
    );
    let row = sqlx::query("SELECT head FROM refs WHERE graph_id = 'default' AND branch = 'main'")
        .fetch_one(&up)
        .await
        .unwrap();
    let stored_head: String = row.get("head");
    assert_eq!(stored_head, head);
    let store = PostgresImmutableStore::from_pool(
        up.clone(),
        V1Binding::BindTo(GraphId::new("default").unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(store.verify_commit_index().await.unwrap(), 1);
    assert_eq!(
        store.put_commit(&legacy).await.unwrap(),
        legacy_id,
        "idempotent re-publication"
    );

    let (_clean_url, clean) = fresh_database("ledger_clean").await;
    sqlx::migrate!("../../migrations")
        .run(&clean)
        .await
        .unwrap();
    let row = sqlx::query("SELECT status FROM graphs WHERE graph_id = 'default'")
        .fetch_one(&clean)
        .await
        .unwrap();
    let status: String = row.get("status");
    assert_eq!(
        status, "bootstrap",
        "clean install also carries the bootstrap graph"
    );

    let upgraded = schema_snapshot(&up).await;
    let fresh = schema_snapshot(&clean).await;
    assert!(upgraded.contains("constraint refs.refs_graph_fk"));
    assert!(upgraded.contains("constraint commit_index.commit_index_graph_fk"));
    assert!(upgraded.contains("trigger graphs.graphs_identity_immutable"));
    assert!(upgraded.contains("constraint refs.refs_head_fk"));
    assert!(upgraded.contains("trigger refs.refs_version_monotonic"));
    let row =
        sqlx::query("SELECT version FROM refs WHERE graph_id = 'default' AND branch = 'main'")
            .fetch_one(&up)
            .await
            .unwrap();
    let version: i64 = row.get("version");
    assert_eq!(version, 1, "existing refs start at version 1 after upgrade");
    assert_eq!(upgraded, fresh, "upgrade and clean install must converge");
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn upgrade_refuses_graphs_without_a_derivable_owner() {
    let (url, pool) = fresh_database("ledger_unknown").await;
    migrator_up_to(3).run(&pool).await.unwrap();
    sqlx::query("INSERT INTO refs (graph_id, branch, head) VALUES ('team-alpha', 'main', $1)")
        .bind("sha256:2222222222222222222222222222222222222222222222222222222222222222")
        .execute(&pool)
        .await
        .unwrap();
    // The commit_index half of the guard: an indexed commit under an unknown graph.
    let p = patch("unknown-graph");
    sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(p.id().to_string())
        .bind(p.canonical_bytes())
        .execute(&pool)
        .await
        .unwrap();
    let stray = AnyCommit::V1(Commit {
        parents: vec![],
        patch: p.id(),
        author: "a".into(),
        message: "stray".into(),
        event_time: "e".into(),
        recorded_time: "r".into(),
    });
    sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(stray.id().unwrap().to_string())
        .bind(stray.canonical_bytes().unwrap())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO commit_index (id, graph_id, version, patch_id, parent_count) \
         VALUES ($1, 'team-beta', 1, $2, 0)",
    )
    .bind(stray.id().unwrap().to_string())
    .bind(p.id().to_string())
    .execute(&pool)
    .await
    .unwrap();
    let error = sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect_err("migration 0004 must refuse to guess tenants for team-alpha/team-beta");
    let message = error.to_string();
    assert!(message.contains("team-alpha, team-beta"), "{message}");
    assert!(message.contains("no derivable tenant owner"), "{message}");
    let row = sqlx::query("SELECT max(version) AS v FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    let applied: i64 = row.get("v");
    assert_eq!(applied, 3, "nothing past 0003 was recorded as applied");
    // Nothing from 0004 was applied: no graphs table, no FK, refs untouched.
    let graphs_table = sqlx::query("SELECT to_regclass('public.graphs')::text AS t")
        .fetch_one(&pool)
        .await
        .unwrap();
    let t: Option<String> = graphs_table.get("t");
    assert_eq!(
        t, None,
        "failed migration must not leave a partial graphs table"
    );
    let row = sqlx::query("SELECT count(*) AS n FROM refs WHERE graph_id = 'team-alpha'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let n: i64 = row.get("n");
    assert_eq!(n, 1);
    // The operator remedy: remove (or import) the unowned rows, then the upgrade proceeds.
    sqlx::query("DELETE FROM refs WHERE graph_id = 'team-alpha'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM commit_index WHERE graph_id = 'team-beta'")
        .execute(&pool)
        .await
        .unwrap();
    // A failed sqlx migration run leaves its session-level advisory lock on the pooled
    // connection that ran it; a real operator retries from a fresh process, so reconnect.
    pool.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
    let t: Option<String> = sqlx::query("SELECT to_regclass('public.graphs')::text AS t")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("t");
    assert_eq!(t.as_deref(), Some("graphs"));
}

/// Migration 0007 binds every existing idempotency row to its proposal's complete actor
/// and fails closed when a row cannot be bound or an audit row's tenant disagrees with
/// its graph.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn migration_0007_backfills_actor_scope_and_refuses_unbound_or_mismatched_rows() {
    use ledger_core::{AuthenticatedPrincipal, PrincipalId, PrincipalType, TenantId};
    use ledger_store::{PrepareRequest, RequestScope, WorkflowRepository};

    // Happy upgrade: a P1.3 database with a proposal by an agent (on behalf of a human),
    // its prepare idempotency row, and a REJECT by a different human reviewer with its
    // own idempotency row (the reviewer's row must take the reviewer's actor, not the
    // proposer's).
    let (_url, pool) = fresh_database("ledger_0007").await;
    migrator_up_to(6).run(&pool).await.unwrap();
    let graphs = PgGraphs::new(pool.clone());
    let graph = unique("g");
    graphs
        .create(&new_graph(&graph, "tenant-a", None))
        .await
        .unwrap();
    let store = PostgresImmutableStore::from_pool_migrated(pool.clone(), V1Binding::Reject);
    let p = patch("0007");
    store
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let candidate = store
        .put_commit(&v2(&graph, &p.id(), "candidate"))
        .await
        .unwrap();
    let digest = ledger_core::ContentId::for_bytes(b"req").to_string();
    let row = sqlx::query(
        "INSERT INTO proposals (graph_id, branch, tenant_id, principal_id, principal_type, on_behalf_of, \
         expected_head, requested_patch_id, effective_patch_id, candidate_commit) \
         VALUES ($1, 'main', 'tenant-a', 'urn:sculpin:agent:test', 'agent', 'urn:sculpin:human:h', NULL, $2, $2, $3) \
         RETURNING proposal_id",
    )
    .bind(&graph)
    .bind(p.id().to_string())
    .bind(candidate.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let proposal_id: i64 = row.get("proposal_id");
    sqlx::query(
        "INSERT INTO idempotency (tenant_id, principal_id, graph_id, operation, idempotency_key, \
         request_digest, result_kind, result_commit, result_proposal_id) \
         VALUES ('tenant-a', 'urn:sculpin:agent:test', $1, 'prepare', 'k1', $2, 'prepared', $3, $4)",
    )
    .bind(&graph)
    .bind(&digest)
    .bind(candidate.to_string())
    .bind(proposal_id)
    .execute(&pool)
    .await
    .unwrap();
    let row = sqlx::query(
        "INSERT INTO decisions (proposal_id, graph_id, branch, candidate_commit, decision, tenant_id, \
         principal_id, principal_type, on_behalf_of, reason, validation_ids) \
         VALUES ($1, $2, 'main', $3, 'rejected', 'tenant-a', 'urn:sculpin:human:reviewer', 'human', NULL, 'no', '{}') \
         RETURNING decision_id",
    )
    .bind(proposal_id)
    .bind(&graph)
    .bind(candidate.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let decision_id: i64 = row.get("decision_id");
    sqlx::query(
        "INSERT INTO idempotency (tenant_id, principal_id, graph_id, operation, idempotency_key, \
         request_digest, result_kind, result_decision_id, result_proposal_id) \
         VALUES ('tenant-a', 'urn:sculpin:human:reviewer', $1, 'reject', 'k2', $2, 'rejected', $3, $4)",
    )
    .bind(&graph)
    .bind(&digest)
    .bind(decision_id)
    .bind(proposal_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
    let scope = |key: &str| {
        let key = key.to_owned();
        let pool = pool.clone();
        async move {
            let row = sqlx::query(
                "SELECT principal_type, on_behalf_of, request_digest, result_kind, result_commit, \
                 result_proposal_id, result_decision_id FROM idempotency WHERE idempotency_key = $1",
            )
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap();
            (
                row.get::<String, _>("principal_type"),
                row.get::<Option<String>, _>("on_behalf_of"),
                row.get::<String, _>("request_digest"),
                row.get::<String, _>("result_kind"),
                row.get::<Option<String>, _>("result_commit"),
                row.get::<Option<i64>, _>("result_proposal_id"),
                row.get::<Option<i64>, _>("result_decision_id"),
            )
        }
    };
    // The prepare row takes the proposer's actor; the reject row the reviewer's. Results
    // are untouched by the backfill.
    assert_eq!(
        scope("k1").await,
        (
            "agent".to_owned(),
            Some("urn:sculpin:human:h".to_owned()),
            digest.clone(),
            "prepared".to_owned(),
            Some(candidate.to_string()),
            Some(proposal_id),
            None
        )
    );
    assert_eq!(
        scope("k2").await,
        (
            "human".to_owned(),
            None,
            digest.clone(),
            "rejected".to_owned(),
            None,
            Some(proposal_id),
            Some(decision_id)
        )
    );
    // The write-once guard is back in force after the backfill.
    let rewrite = sqlx::query(
        "UPDATE idempotency SET request_digest = request_digest WHERE idempotency_key = 'k1'",
    )
    .execute(&pool)
    .await;
    assert!(
        matches!(rewrite, Err(sqlx::Error::Database(ref e)) if e.message().contains("write-once")),
        "{rewrite:?}"
    );
    // Behavioural convergence: the upgraded row replays through the repository for the
    // complete actor it was bound to, and is invisible to the same principal without the
    // delegation.
    let workflows = WorkflowRepository::new(pool.clone(), store.clone());
    let request = |on_behalf_of: Option<&str>| PrepareRequest {
        scope: RequestScope {
            principal: AuthenticatedPrincipal {
                principal_id: PrincipalId::new("urn:sculpin:agent:test").unwrap(),
                principal_type: PrincipalType::Agent,
                tenant_id: TenantId::new("tenant-a").unwrap(),
                on_behalf_of: on_behalf_of.map(|o| PrincipalId::new(o).unwrap()),
            },
            graph: GraphId::new(&graph).unwrap(),
            idempotency_key: "k1".into(),
            request_digest: ledger_core::ContentId::for_bytes(b"req"),
            correlation_id: None,
        },
        branch: "main".into(),
        expected_head: None,
        requested: p.clone(),
        activity: "a".into(),
        event_time: None,
        evidence_refs: vec![],
        source_system: None,
        message: "m".into(),
    };
    let replayed = workflows
        .prepare(&request(Some("urn:sculpin:human:h")))
        .await
        .unwrap();
    assert!(replayed.replayed);
    assert_eq!(replayed.candidate, candidate);
    assert_eq!(replayed.proposal_id, proposal_id);
    let fresh = workflows.prepare(&request(None)).await.unwrap();
    assert!(
        !fresh.replayed,
        "a different delegation is a different namespace"
    );
    assert_ne!(fresh.proposal_id, proposal_id);
    // Schema convergence with a populated upgrade: identical to a clean install.
    let (_clean_url, clean) = fresh_database("ledger_0007_clean").await;
    sqlx::migrate!("../../migrations")
        .run(&clean)
        .await
        .unwrap();
    assert_eq!(schema_snapshot(&pool).await, schema_snapshot(&clean).await);
    clean.close().await;
    pool.close().await;

    // Unbound row: an idempotency row without a resolvable proposal/decision fails the upgrade.
    let (_url, pool) = fresh_database("ledger_0007_unbound").await;
    migrator_up_to(6).run(&pool).await.unwrap();
    PgGraphs::new(pool.clone())
        .create(&new_graph(&graph, "tenant-a", None))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO idempotency (tenant_id, principal_id, graph_id, operation, idempotency_key, \
         request_digest, result_kind) \
         VALUES ('tenant-a', 'urn:sculpin:agent:test', $1, 'reject', 'k2', $2, 'rejected')",
    )
    .bind(&graph)
    .bind(&digest)
    .execute(&pool)
    .await
    .unwrap();
    let error = sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("cannot be bound to an actor"),
        "{error}"
    );
    pool.close().await;

    // Mismatched actor: an idempotency row whose principal differs from its proposal's.
    let (_url, pool) = fresh_database("ledger_0007_actor").await;
    migrator_up_to(6).run(&pool).await.unwrap();
    PgGraphs::new(pool.clone())
        .create(&new_graph(&graph, "tenant-a", None))
        .await
        .unwrap();
    let store = PostgresImmutableStore::from_pool_migrated(pool.clone(), V1Binding::Reject);
    store
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let candidate = store
        .put_commit(&v2(&graph, &p.id(), "candidate"))
        .await
        .unwrap();
    let row = sqlx::query(
        "INSERT INTO proposals (graph_id, branch, tenant_id, principal_id, principal_type, expected_head, \
         requested_patch_id, effective_patch_id, candidate_commit) \
         VALUES ($1, 'main', 'tenant-a', 'urn:sculpin:agent:test', 'agent', NULL, $2, $2, $3) RETURNING proposal_id",
    )
    .bind(&graph)
    .bind(p.id().to_string())
    .bind(candidate.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let proposal_id: i64 = row.get("proposal_id");
    sqlx::query(
        "INSERT INTO idempotency (tenant_id, principal_id, graph_id, operation, idempotency_key, \
         request_digest, result_kind, result_commit, result_proposal_id) \
         VALUES ('tenant-a', 'urn:sculpin:agent:OTHER', $1, 'prepare', 'k1', $2, 'prepared', $3, $4)",
    )
    .bind(&graph)
    .bind(&digest)
    .bind(candidate.to_string())
    .bind(proposal_id)
    .execute(&pool)
    .await
    .unwrap();
    let error = sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("disagree"), "{error}");
    pool.close().await;

    // Mismatched tenant: an audit row whose graph belongs to another tenant fails the upgrade.
    let (_url, pool) = fresh_database("ledger_0007_mismatch").await;
    migrator_up_to(6).run(&pool).await.unwrap();
    PgGraphs::new(pool.clone())
        .create(&new_graph(&graph, "tenant-a", None))
        .await
        .unwrap();
    let store = PostgresImmutableStore::from_pool_migrated(pool.clone(), V1Binding::Reject);
    store
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let candidate = store
        .put_commit(&v2(&graph, &p.id(), "candidate"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO proposals (graph_id, branch, tenant_id, principal_id, principal_type, expected_head, \
         requested_patch_id, effective_patch_id, candidate_commit) \
         VALUES ($1, 'main', 'tenant-WRONG', 'urn:sculpin:agent:test', 'agent', NULL, $2, $2, $3)",
    )
    .bind(&graph)
    .bind(p.id().to_string())
    .bind(candidate.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let error = sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not belong to their tenant"),
        "{error}"
    );
    pool.close().await;
}
