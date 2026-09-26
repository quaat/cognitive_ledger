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
use ledger_store::{CommitRequest, Ledger, PgRefStore, PostgresImmutableStore, V1Binding};
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
    assert_eq!(store.put_commit(&genesis).await.unwrap(), g);
    let child = v2(&graph, vec![g.clone()], &p, "child");
    let c = store.put_commit(&child).await.unwrap();

    // The index re-derives from bytes; the parent edge is recorded at position 0.
    assert!(store.verify_commit_index().await.unwrap() >= 2);
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
    let other =
        PostgresImmutableStore::connect(&url, V1Binding::BindTo(GraphId::new("other").unwrap()))
            .await
            .unwrap();
    assert!(matches!(
        other.put_commit(&legacy).await,
        Err(LedgerError::CorruptObject { .. })
    ));
}
