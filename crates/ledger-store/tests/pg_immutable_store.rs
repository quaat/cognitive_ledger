//! Real-PostgreSQL evidence for the shared immutable store (ADR-0012) and the v1
//! graph-binding policy (ADR-0010). `#[ignore]`d by default; `scripts/test-integration.sh`
//! runs it with `LEDGER_TEST_DATABASE_URL` set. Every scenario uses two *independent*
//! pools ("replicas"): correctness must come from the shared database, never from
//! process-local state.
#![cfg(feature = "postgres")]

use ledger_core::{
    Actor, AnyCommit, Commit, CommitId, CommitV2, ContentId, GraphId, ImmutableStore, LedgerError,
    LedgerTimestamp, PrincipalId, PrincipalType, RefStore,
};
use ledger_rdf::{Operation, OperationKind, Patch};
use ledger_store::{
    CommitRequest, GraphStatus, Ledger, NewGraph, PgGraphs, PgRefStore, PostgresImmutableStore,
    V1Binding,
};
use std::sync::Arc;
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

/// Register a graph row (ADR-0010 authority) so indexed commits can reference it.
async fn create_graph(pool: &sqlx::PgPool, graph: &str, tenant: &str) {
    create_graph_with(pool, graph, tenant, GraphStatus::Active).await;
}

/// v1 history may only be bound to a graph in status `bootstrap` or `importing`.
async fn create_import_graph(pool: &sqlx::PgPool, graph: &str, tenant: &str) {
    create_graph_with(pool, graph, tenant, GraphStatus::Importing).await;
}

async fn create_graph_with(pool: &sqlx::PgPool, graph: &str, tenant: &str, status: GraphStatus) {
    PgGraphs::new(pool.clone())
        .create(&NewGraph {
            graph_id: GraphId::new(graph).unwrap(),
            tenant_id: ledger_core::TenantId::new(tenant).unwrap(),
            knowledge_base_id: None,
            purpose: None,
            status,
        })
        .await
        .unwrap();
}

fn patch(value: &str) -> Patch {
    Patch::new([Operation {
        kind: OperationKind::Add,
        quad: format!("<urn:s> <urn:p> \"{value}\" .").parse().unwrap(),
    }])
    .unwrap()
}

fn v1(parents: Vec<CommitId>, patch: &Patch, message: &str) -> AnyCommit {
    AnyCommit::V1(Commit {
        parents,
        patch: patch.id(),
        author: "urn:agent:test".into(),
        message: message.into(),
        event_time: "2026-09-26T00:00:00Z".into(),
        recorded_time: unique("recorded"),
    })
}

fn v2(graph: &str, parents: Vec<CommitId>, patch: &Patch, message: &str) -> AnyCommit {
    AnyCommit::V2(CommitV2 {
        graph_id: GraphId::new(graph).unwrap(),
        parents,
        patch: patch.id(),
        actor: Actor {
            principal_id: PrincipalId::new("urn:sculpin:agent:test").unwrap(),
            principal_type: PrincipalType::Agent,
            on_behalf_of: None,
        },
        activity: "test".into(),
        event_time: None,
        recorded_at: LedgerTimestamp::parse_rfc3339("2026-09-26T00:00:00Z").unwrap(),
        evidence_refs: vec![unique("urn:evidence")],
        source_system: None,
        message: message.into(),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn replica_b_reconstructs_what_replica_a_committed() {
    let url = database_url();
    let branch = unique("replica");
    let bootstrap = GraphId::new("default").unwrap();
    let make_replica = || async {
        let immutable: Arc<dyn ImmutableStore> = Arc::new(
            PostgresImmutableStore::connect(&url, V1Binding::BindTo(bootstrap.clone()))
                .await
                .unwrap(),
        );
        let refs: Arc<dyn RefStore> = Arc::new(
            PgRefStore::connect_ref(&url, "default", &branch)
                .await
                .unwrap(),
        );
        Ledger::with_stores(immutable, refs)
    };
    let replica_a = make_replica().await;
    let replica_b = make_replica().await;

    let c1 = replica_a
        .commit(CommitRequest {
            expected_head: None,
            patch: patch("temperature"),
            author: "urn:agent:a".into(),
            message: "genesis".into(),
            event_time: "2026-09-26T00:00:00Z".into(),
        })
        .await
        .unwrap();
    let c2 = replica_a
        .commit(CommitRequest {
            expected_head: Some(c1.clone()),
            patch: patch("humidity"),
            author: "urn:agent:a".into(),
            message: "second".into(),
            event_time: "2026-09-26T00:00:01Z".into(),
        })
        .await
        .unwrap();

    // Replica B holds nothing locally; everything it knows comes from the shared store.
    assert_eq!(replica_b.head().await.unwrap(), Some(c2.clone()));
    let state_b = replica_b.state_at(&c2).await.unwrap();
    let state_a = replica_a.state_at(&c2).await.unwrap();
    assert_eq!(state_a, state_b);
    assert_eq!(state_b.len(), 2);
    let stored = replica_b
        .immutable_store()
        .get_commit(&c2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.parents(), std::slice::from_ref(&c1));
    assert_eq!(stored.version(), 1);

    // Replica B can continue the line; A observes it.
    let c3 = replica_b
        .commit(CommitRequest {
            expected_head: Some(c2.clone()),
            patch: patch("pressure"),
            author: "urn:agent:b".into(),
            message: "third".into(),
            event_time: "2026-09-26T00:00:02Z".into(),
        })
        .await
        .unwrap();
    assert_eq!(replica_a.head().await.unwrap(), Some(c3.clone()));
    assert_eq!(replica_a.state_at(&c3).await.unwrap().len(), 3);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn commit_index_is_verified_typed_and_idempotent() {
    let _ = IGNORE;
    let url = database_url();
    let graph = unique("graph");
    let store = PostgresImmutableStore::connect(&url, V1Binding::Reject)
        .await
        .unwrap();
    create_graph(store.pool(), &graph, "tenant-a").await;
    let p = patch(&unique("value"));
    store
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();

    // Typed parent check: a parent that is a real object but not a commit is rejected.
    let orphan = v2(&graph, vec![CommitId(p.id().0.clone())], &p, "orphan");
    assert!(matches!(
        store.put_commit(&orphan).await,
        Err(LedgerError::MissingParent(_))
    ));
    // Missing patch is rejected before anything is written.
    let absent_patch = patch(&unique("absent"));
    let no_patch = v2(&graph, vec![], &absent_patch, "no patch");
    assert!(matches!(
        store.put_commit(&no_patch).await,
        Err(LedgerError::MissingPatch(_))
    ));

    let genesis = v2(&graph, vec![], &p, "genesis");
    let g = store.put_commit(&genesis).await.unwrap();
    // Idempotent replay returns the same id and writes nothing new.
    let indexed_at = |store: &PostgresImmutableStore, id: &CommitId| {
        let q = sqlx::query("SELECT indexed_at::text AS t FROM commit_index WHERE id = $1")
            .bind(id.to_string());
        let pool = store.pool().clone();
        async move {
            let row = q.fetch_one(&pool).await.unwrap();
            let t: String = sqlx::Row::try_get(&row, "t").unwrap();
            t
        }
    };
    let before = indexed_at(&store, &g).await;
    assert_eq!(store.put_commit(&genesis).await.unwrap(), g);
    assert_eq!(
        indexed_at(&store, &g).await,
        before,
        "replay did not rewrite the row"
    );
    let child = v2(&graph, vec![g.clone()], &p, "child");
    let c = store.put_commit(&child).await.unwrap();

    // The index re-derives from bytes; the parent edge is recorded at position 0.
    assert_eq!(
        store.verify_commits(&[g.clone(), c.clone()]).await.unwrap(),
        2
    );
    assert!(
        matches!(
            store.verify_commits(&[CommitId(p.id().0.clone())]).await,
            Err(LedgerError::NotFound(_))
        ),
        "an unindexed object never verifies as a commit"
    );
    let row =
        sqlx::query("SELECT parent_id FROM commit_parents WHERE commit_id = $1 AND position = 0")
            .bind(c.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap();
    let parent_id: String = sqlx::Row::try_get(&row, "parent_id").unwrap();
    assert_eq!(parent_id, g.to_string());
    let row = sqlx::query("SELECT graph_id, version FROM commit_index WHERE id = $1")
        .bind(c.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    let indexed_graph: String = sqlx::Row::try_get(&row, "graph_id").unwrap();
    let version: i16 = sqlx::Row::try_get(&row, "version").unwrap();
    assert_eq!(indexed_graph, graph);
    assert_eq!(version, 2);

    // A non-commit object never reads as a commit, and content is digest-verified.
    assert_eq!(
        store.get_commit(&CommitId(p.id().0.clone())).await.unwrap(),
        None
    );
    assert!(store.exists(&p.id().0).await.unwrap());
    assert_eq!(store.get_commit(&c).await.unwrap(), Some(child));
    assert!(matches!(
        store.put_content(&ContentId::for_bytes(b"a"), b"b").await,
        Err(LedgerError::ObjectCollision(_))
    ));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn v1_binding_policy_is_enforced_on_write() {
    let url = database_url();
    let p = patch(&unique("policy"));
    let rejecting = PostgresImmutableStore::connect(&url, V1Binding::Reject)
        .await
        .unwrap();
    rejecting
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let legacy = v1(vec![], &p, "legacy");
    assert!(matches!(
        rejecting.put_commit(&legacy).await,
        Err(LedgerError::InvalidCommit(_))
    ));
    assert!(!rejecting.exists(&legacy.id().unwrap().0).await.unwrap());

    let bootstrap = GraphId::new("default").unwrap();
    let binding = PostgresImmutableStore::connect(&url, V1Binding::BindTo(bootstrap))
        .await
        .unwrap();
    let id = binding.put_commit(&legacy).await.unwrap();
    let row = sqlx::query("SELECT graph_id FROM commit_index WHERE id = $1")
        .bind(id.to_string())
        .fetch_one(binding.pool())
        .await
        .unwrap();
    let indexed: String = sqlx::Row::try_get(&row, "graph_id").unwrap();
    assert_eq!(indexed, "default");
    // The rejecting store still *reads* v1 (dual read): policy governs writes only.
    assert_eq!(
        rejecting.get_commit(&id).await.unwrap(),
        Some(legacy.clone())
    );
    // Re-binding the same v1 commit to another graph is impossible: one id, one graph.
    let other_graph = unique("graph");
    create_import_graph(binding.pool(), &other_graph, "tenant-b").await;
    let other = PostgresImmutableStore::connect(
        &url,
        V1Binding::BindTo(GraphId::new(other_graph).unwrap()),
    )
    .await
    .unwrap();
    assert!(matches!(
        other.put_commit(&legacy).await,
        Err(LedgerError::GraphBindingConflict { .. })
    ));
    // A v2 commit for a graph nobody registered is refused before anything is written.
    let unregistered = v2(&unique("nograph"), vec![], &p, "unregistered");
    assert!(matches!(
        binding.put_commit(&unregistered).await,
        Err(LedgerError::UnknownGraph(_))
    ));
    assert!(!binding.exists(&unregistered.id().unwrap().0).await.unwrap());
    // v1 history cannot be bound to an *active* graph (ADR-0010: bootstrap/importing only).
    let active = unique("active");
    create_graph(binding.pool(), &active, "tenant-c").await;
    let into_active =
        PostgresImmutableStore::connect(&url, V1Binding::BindTo(GraphId::new(&active).unwrap()))
            .await
            .unwrap();
    let fresh_v1 = v1(vec![], &p, "into active");
    assert!(matches!(
        into_active.put_commit(&fresh_v1).await,
        Err(LedgerError::InvalidCommit(_))
    ));
    assert!(!into_active.exists(&fresh_v1.id().unwrap().0).await.unwrap());
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn commit_bytes_are_refused_as_content_and_corrupt_envelopes_are_errors_not_absent() {
    let url = database_url();
    let store = PostgresImmutableStore::connect(&url, V1Binding::Reject)
        .await
        .unwrap();
    let p = patch(&unique("content"));
    store
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let graph = unique("graph");
    create_graph(store.pool(), &graph, "tenant-a").await;
    // A commit envelope cannot bypass put_commit's checks by arriving as "content".
    let commit = v2(&graph, vec![], &p, "smuggled");
    let bytes = commit.canonical_bytes().unwrap();
    assert!(matches!(
        store.put_content(&commit.id().unwrap().0, &bytes).await,
        Err(LedgerError::InvalidCommit(_))
    ));
    assert!(!store.exists(&commit.id().unwrap().0).await.unwrap());
    // A known commit header with a broken body is corruption, not "no such commit".
    let mut broken = ledger_core::COMMIT_V2_HEADER.to_vec();
    broken.extend_from_slice(unique("garbage").as_bytes());
    let broken_id = ContentId::for_bytes(&broken);
    sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(broken_id.to_string())
        .bind(&broken)
        .execute(store.pool())
        .await
        .unwrap();
    assert!(matches!(
        store.get_commit(&CommitId(broken_id)).await,
        Err(LedgerError::CorruptObject { .. })
    ));
    // An unknown commit version fails closed rather than reading as content.
    let mut future = b"sculpin-cognitive-commit-v9\0".to_vec();
    future.extend_from_slice(unique("future").as_bytes());
    let future_id = ContentId::for_bytes(&future);
    sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(future_id.to_string())
        .bind(&future)
        .execute(store.pool())
        .await
        .unwrap();
    assert!(matches!(
        store.get_commit(&CommitId(future_id)).await,
        Err(LedgerError::UnknownCommitVersion(_))
    ));
    // A patch must be content: a commit naming another commit as its patch is refused.
    let genesis = store
        .put_commit(&v2(&graph, vec![], &p, "genesis"))
        .await
        .unwrap();
    let AnyCommit::V2(mut inner) = v2(&graph, vec![], &p, "bogus") else {
        unreachable!()
    };
    inner.patch = ledger_core::PatchId(genesis.0.clone());
    assert!(matches!(
        store.put_commit(&AnyCommit::V2(inner)).await,
        Err(LedgerError::InvalidCommit(_))
    ));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn publication_is_truthful_against_inconsistent_stored_bytes() {
    let url = database_url();
    let store = PostgresImmutableStore::connect(&url, V1Binding::Reject)
        .await
        .unwrap();
    // Seed a damaged row directly: the id is well-formed but the bytes are not its preimage.
    let good = unique("payload").into_bytes();
    let id = ContentId::for_bytes(&good);
    sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(id.to_string())
        .bind(b"damaged".as_slice())
        .execute(store.pool())
        .await
        .unwrap();
    // Publishing the genuine bytes must fail hard instead of "succeeding" on the conflict.
    assert!(matches!(
        store.put_content(&id, &good).await,
        Err(LedgerError::ObjectCollision(_))
    ));
    // Reads report corruption, and the damaged bytes were not overwritten.
    assert!(matches!(
        store.get_content(&id).await,
        Err(LedgerError::CorruptObject { .. })
    ));
    let row = sqlx::query("SELECT bytes FROM immutable_objects WHERE id = $1")
        .bind(id.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    let stored: Vec<u8> = sqlx::Row::try_get(&row, "bytes").unwrap();
    assert_eq!(stored, b"damaged");
    // A commit whose own envelope id is damaged in storage is refused the same way.
    let p = patch(&unique("truthful"));
    store
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let graph = unique("graph");
    create_graph(store.pool(), &graph, "tenant-a").await;
    let commit = v2(&graph, vec![], &p, "victim");
    let commit_id = commit.id().unwrap();
    sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(commit_id.to_string())
        .bind(b"damaged commit".as_slice())
        .execute(store.pool())
        .await
        .unwrap();
    assert!(matches!(
        store.put_commit(&commit).await,
        Err(LedgerError::ObjectCollision(_))
    ));
    let indexed = sqlx::query("SELECT 1 FROM commit_index WHERE id = $1")
        .bind(commit_id.to_string())
        .fetch_optional(store.pool())
        .await
        .unwrap();
    assert!(indexed.is_none(), "a failed publication indexes nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn concurrent_incompatible_v1_bindings_resolve_to_exactly_one_graph() {
    let url = database_url();
    let graph_a = unique("graph-a");
    let graph_b = unique("graph-b");
    let store_a =
        PostgresImmutableStore::connect(&url, V1Binding::BindTo(GraphId::new(&graph_a).unwrap()))
            .await
            .unwrap();
    let store_b =
        PostgresImmutableStore::connect(&url, V1Binding::BindTo(GraphId::new(&graph_b).unwrap()))
            .await
            .unwrap();
    create_import_graph(store_a.pool(), &graph_a, "tenant-a").await;
    create_import_graph(store_a.pool(), &graph_b, "tenant-b").await;
    let p = patch(&unique("race"));
    store_a
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();

    for round in 0..5 {
        let commit = v1(vec![], &p, &format!("raced v1 commit {round}"));
        let id = commit.id().unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for store in [store_a.clone(), store_b.clone()] {
            let barrier = Arc::clone(&barrier);
            let commit = commit.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                store.put_commit(&commit).await
            }));
        }
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }
        // results[0] is store A (graph_a), results[1] is store B (graph_b).
        let rows = sqlx::query("SELECT graph_id FROM commit_index WHERE id = $1")
            .bind(id.to_string())
            .fetch_all(store_a.pool())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "exactly one binding is authoritative");
        let indexed: String = sqlx::Row::try_get(&rows[0], "graph_id").unwrap();
        let (winner, loser, loser_graph) = if indexed == graph_a {
            (0, 1, &graph_b)
        } else {
            assert_eq!(indexed, graph_b);
            (1, 0, &graph_a)
        };
        assert_eq!(results[winner].as_ref().unwrap(), &id, "round {round}");
        match &results[loser] {
            Err(LedgerError::GraphBindingConflict {
                commit,
                indexed: i,
                requested,
            }) => {
                assert_eq!(commit, &id);
                assert_eq!(i, &indexed);
                assert_eq!(requested, loser_graph);
            }
            other => panic!("round {round}: loser got {other:?}"),
        }
        assert_eq!(
            store_a
                .verify_commits(std::slice::from_ref(&id))
                .await
                .unwrap(),
            1
        );
    }

    // Two concurrent *compatible* publications both succeed (idempotent).
    let legacy = v1(vec![], &p, "compatible v1 commit");
    let id = legacy.id().unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for store in [store_a.clone(), store_a.clone()] {
        let barrier = Arc::clone(&barrier);
        let commit = legacy.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            store.put_commit(&commit).await
        }));
    }
    for handle in handles {
        assert_eq!(handle.await.unwrap().unwrap(), id);
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn cross_graph_ancestry_is_rejected() {
    let url = database_url();
    let graph_a = unique("graph-a");
    let graph_b = unique("graph-b");
    let store_a =
        PostgresImmutableStore::connect(&url, V1Binding::BindTo(GraphId::new(&graph_a).unwrap()))
            .await
            .unwrap();
    let store_b =
        PostgresImmutableStore::connect(&url, V1Binding::BindTo(GraphId::new(&graph_b).unwrap()))
            .await
            .unwrap();
    create_import_graph(store_a.pool(), &graph_a, "tenant-a").await;
    create_import_graph(store_a.pool(), &graph_b, "tenant-b").await;
    let p = patch(&unique("cross"));
    store_a
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();

    let a_genesis = store_a
        .put_commit(&v2(&graph_a, vec![], &p, "A genesis"))
        .await
        .unwrap();
    let b_genesis = store_b
        .put_commit(&v2(&graph_b, vec![], &p, "B genesis"))
        .await
        .unwrap();
    let v1_in_a = store_a
        .put_commit(&v1(vec![], &p, "v1 bound to A"))
        .await
        .unwrap();

    // v2 graph-B child -> graph-A v2 parent.
    let cross_child = v2(&graph_b, vec![a_genesis.clone()], &p, "B child of A");
    assert!(matches!(
        store_b.put_commit(&cross_child).await,
        Err(LedgerError::CrossGraphParent { .. })
    ));
    // v2 graph-B merge -> one graph-B parent + one graph-A parent.
    let cross_merge = v2(
        &graph_b,
        vec![b_genesis.clone(), a_genesis.clone()],
        &p,
        "B merge with A",
    );
    assert!(matches!(
        store_b.put_commit(&cross_merge).await,
        Err(LedgerError::CrossGraphParent { .. })
    ));
    // v2 child -> v1 parent bound to a different graph.
    let cross_v1 = v2(&graph_b, vec![v1_in_a.clone()], &p, "B child of v1-in-A");
    assert!(matches!(
        store_b.put_commit(&cross_v1).await,
        Err(LedgerError::CrossGraphParent { .. })
    ));
    // The same shapes within one graph are fine, including a v2 child of a v1 parent.
    store_a
        .put_commit(&v2(&graph_a, vec![a_genesis.clone()], &p, "A child"))
        .await
        .unwrap();
    store_a
        .put_commit(&v2(&graph_a, vec![v1_in_a.clone()], &p, "A child of v1"))
        .await
        .unwrap();
    // Nothing cross-graph was indexed.
    for rejected in [cross_child, cross_merge, cross_v1] {
        assert!(!store_b.exists(&rejected.id().unwrap().0).await.unwrap());
    }
    assert_eq!(
        store_a
            .verify_commits(&[a_genesis.clone(), b_genesis.clone(), v1_in_a.clone()])
            .await
            .unwrap(),
        3
    );
}

/// Deterministic exercise of the *blocked* path: a transaction holds an uncommitted
/// commit_index row for the same id under graph B; store A's INSERT … ON CONFLICT must
/// wait on the unique index, then read back the winner's row (or proceed if B rolls back).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn blocked_publication_reads_the_winner_after_commit_and_proceeds_after_rollback() {
    let url = database_url();
    let graph_a = unique("graph-a");
    let graph_b = unique("graph-b");
    let store_a =
        PostgresImmutableStore::connect(&url, V1Binding::BindTo(GraphId::new(&graph_a).unwrap()))
            .await
            .unwrap();
    create_import_graph(store_a.pool(), &graph_a, "tenant-a").await;
    create_import_graph(store_a.pool(), &graph_b, "tenant-b").await;
    let p = patch(&unique("blocked"));
    store_a
        .put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let holder = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();

    for (round, holder_commits) in [(0, true), (1, false)] {
        let commit = v1(vec![], &p, &format!("blocked round {round}"));
        let id = commit.id().unwrap();
        let bytes = commit.canonical_bytes().unwrap();
        // B: uncommitted object + index row under graph_b.
        let mut tx = holder.begin().await.unwrap();
        sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
            .bind(id.to_string())
            .bind(&bytes)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO commit_index (id, graph_id, version, patch_id, parent_count) \
             VALUES ($1, $2, 1, $3, 0)",
        )
        .bind(id.to_string())
        .bind(&graph_b)
        .bind(p.id().to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
        // A: publish concurrently; it must block on the unique index.
        let a = store_a.clone();
        let a_commit = commit.clone();
        let publish = tokio::spawn(async move { a.put_commit(&a_commit).await });
        let mut waited = 0;
        loop {
            let blocked = sqlx::query(
                "SELECT count(*) AS n FROM pg_stat_activity \
                 WHERE wait_event_type = 'Lock' AND state = 'active' \
                 AND query LIKE 'INSERT INTO immutable_objects%'",
            )
            // Poll through store A's pool: the holder pool's only connection is inside
            // the open transaction, so borrowing it here would deadlock the test itself.
            .fetch_one(store_a.pool())
            .await
            .unwrap();
            let n: i64 = sqlx::Row::try_get(&blocked, "n").unwrap();
            if n >= 1 {
                break;
            }
            waited += 1;
            assert!(waited < 400, "store A never blocked on the held row");
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        if holder_commits {
            tx.commit().await.unwrap();
            match publish.await.unwrap() {
                Err(LedgerError::GraphBindingConflict {
                    commit,
                    indexed,
                    requested,
                }) => {
                    assert_eq!(commit, id);
                    assert_eq!(indexed, graph_b);
                    assert_eq!(requested, graph_a);
                }
                other => panic!("expected the blocked loser to see the winner: {other:?}"),
            }
        } else {
            tx.rollback().await.unwrap();
            assert_eq!(publish.await.unwrap().unwrap(), id);
        }
        let row = sqlx::query("SELECT graph_id FROM commit_index WHERE id = $1")
            .bind(id.to_string())
            .fetch_one(store_a.pool())
            .await
            .unwrap();
        let indexed: String = sqlx::Row::try_get(&row, "graph_id").unwrap();
        let expected = if holder_commits { &graph_b } else { &graph_a };
        assert_eq!(&indexed, expected);
        if !holder_commits {
            assert_eq!(store_a.verify_commits(&[id]).await.unwrap(), 1);
        }
    }
}
