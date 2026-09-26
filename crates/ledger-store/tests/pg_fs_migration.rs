//! Real-PostgreSQL evidence for the administrative filesystem → PostgreSQL migration
//! (ADR-0012): id/byte preservation, idempotency, resumability, fault behaviour, and
//! never moving a ref onto unverified history.
#![cfg(feature = "postgres")]

use ledger_core::{
    Actor, AnyCommit, CommitId, CommitV2, ContentId, GraphId, ImmutableStore, LedgerError,
    LedgerTimestamp, PrincipalId, PrincipalType, RefStore,
};
use ledger_rdf::{Operation, OperationKind, Patch};
use ledger_store::{
    CommitRequest, CutoverPhase, FileStore, FsToPgMigration, Ledger, MigrationOutcome, PgRefStore,
    PostgresImmutableStore, V1Binding,
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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

fn quad_patch(quads: &[&str]) -> Patch {
    Patch::new(quads.iter().map(|q| Operation {
        kind: OperationKind::Add,
        quad: q.parse().unwrap(),
    }))
    .unwrap()
}

/// A v1 linear history with named-graph data: genesis adds to the default graph and to
/// `<urn:g:sensors>`, the second commit adds another named-graph quad.
async fn seed_v1_history(dir: &std::path::Path, salt: &str) -> (Ledger, Vec<CommitId>) {
    let ledger = Ledger::open(dir).unwrap();
    let c1 = ledger
        .commit(CommitRequest {
            expected_head: None,
            patch: quad_patch(&[
                &format!("<urn:material:{salt}> <urn:temperature> \"80\" ."),
                &format!("<urn:sensor:{salt}> <urn:reading> \"1\" <urn:g:sensors> ."),
            ]),
            author: "urn:agent:fs".into(),
            message: "genesis".into(),
            event_time: "2026-09-26T00:00:00Z".into(),
        })
        .await
        .unwrap();
    let c2 = ledger
        .commit(CommitRequest {
            expected_head: Some(c1.clone()),
            patch: quad_patch(&[&format!(
                "<urn:sensor:{salt}> <urn:reading> \"2\" <urn:g:sensors> ."
            )]),
            author: "urn:agent:fs".into(),
            message: "second".into(),
            event_time: "2026-09-26T00:00:01Z".into(),
        })
        .await
        .unwrap();
    (ledger, vec![c1, c2])
}

async fn migration(dir: &std::path::Path, branch: &str) -> FsToPgMigration {
    migration_to(dir, "default", branch).await
}

/// A migration whose destination ref is `(graph, branch)` and whose v1 binding is that
/// same graph.
async fn migration_to(dir: &std::path::Path, graph: &str, branch: &str) -> FsToPgMigration {
    let url = database_url();
    let source = FileStore::open_existing(dir).unwrap();
    let destination =
        PostgresImmutableStore::connect(&url, V1Binding::BindTo(GraphId::new(graph).unwrap()))
            .await
            .unwrap();
    let refs = PgRefStore::connect_ref(&url, graph, branch).await.unwrap();
    FsToPgMigration::new(source, destination, refs).unwrap()
}

async fn create_graph(pool: &sqlx::PgPool, graph: &str, status: ledger_store::GraphStatus) {
    ledger_store::PgGraphs::new(pool.clone())
        .create(&ledger_store::NewGraph {
            graph_id: GraphId::new(graph).unwrap(),
            tenant_id: ledger_core::TenantId::new("tenant-test").unwrap(),
            knowledge_base_id: None,
            purpose: None,
            status,
        })
        .await
        .unwrap();
}

async fn destination_ledger(branch: &str) -> Ledger {
    let url = database_url();
    let immutable: Arc<dyn ImmutableStore> = Arc::new(
        PostgresImmutableStore::connect(&url, V1Binding::BindTo(GraphId::new("default").unwrap()))
            .await
            .unwrap(),
    );
    let refs: Arc<dyn RefStore> = Arc::new(
        PgRefStore::connect_ref(&url, "default", branch)
            .await
            .unwrap(),
    );
    Ledger::with_stores(immutable, refs)
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn v1_linear_history_migrates_idempotently_and_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let salt = unique("m");
    let (source, commits) = seed_v1_history(dir.path(), &salt).await;
    let branch = unique("migrate");

    let report = migration(dir.path(), &branch).await.run().await.unwrap();
    assert_eq!(report.outcome, MigrationOutcome::RefInstalled);
    assert_eq!(report.destination_head_before, None);
    assert_eq!(report.commit_objects, 2);
    assert_eq!(report.content_objects, 2);
    assert_eq!(report.source_objects, 4);
    assert_eq!(
        report.commit_index_rows_verified, 2,
        "verification is scoped to the migrated commits"
    );
    assert_eq!(
        report.commits_published_in_order,
        vec![commits[0].to_string(), commits[1].to_string()],
        "parents are published before children"
    );
    assert_eq!(
        report.source_head.as_deref(),
        Some(commits[1].to_string().as_str())
    );
    assert_eq!(report.destination_head_after, report.source_head);
    assert_eq!(report.head_state_quads, Some(3));

    // Same ids, same bytes, same state — including the named-graph quads.
    let destination = destination_ledger(&branch).await;
    assert_eq!(destination.head().await.unwrap(), Some(commits[1].clone()));
    let source_state = source.state_at(&commits[1]).await.unwrap();
    let destination_state = destination.state_at(&commits[1]).await.unwrap();
    assert_eq!(source_state, destination_state);
    assert!(
        source_state
            .iter()
            .any(|q| q.to_string().contains("<urn:g:sensors>"))
    );
    for id in &commits {
        let src = source
            .immutable_store()
            .get_content(&id.0)
            .await
            .unwrap()
            .unwrap();
        let dst = destination
            .immutable_store()
            .get_content(&id.0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(src, dst);
    }
    // The source is untouched.
    assert_eq!(source.head().await.unwrap(), Some(commits[1].clone()));
    assert_eq!(
        FileStore::open(dir.path())
            .unwrap()
            .list_objects()
            .unwrap()
            .len(),
        4
    );

    // Retry of a completed migration: idempotent success, nothing moves.
    let again = migration(dir.path(), &branch).await.run().await.unwrap();
    assert_eq!(again.outcome, MigrationOutcome::AlreadyMigrated);
    assert_eq!(again.destination_head_after, report.destination_head_after);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn mixed_v1_v2_history_migrates_under_the_bootstrap_binding() {
    let dir = tempfile::tempdir().unwrap();
    let salt = unique("mixed");
    let (source, commits) = seed_v1_history(dir.path(), &salt).await;
    // Append a v2 commit bound to the bootstrap graph on top of the v1 head.
    let fs = Arc::new(FileStore::open(dir.path()).unwrap());
    let p = quad_patch(&[&format!("<urn:material:{salt}> <urn:humidity> \"40\" .")]);
    fs.put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let v2 = AnyCommit::V2(CommitV2 {
        graph_id: GraphId::new("default").unwrap(),
        parents: vec![commits[1].clone()],
        patch: p.id(),
        actor: Actor {
            principal_id: PrincipalId::new("urn:sculpin:agent:test").unwrap(),
            principal_type: PrincipalType::Agent,
            on_behalf_of: None,
        },
        activity: "test".into(),
        event_time: None,
        recorded_at: LedgerTimestamp::parse_rfc3339("2026-09-26T00:00:02Z").unwrap(),
        evidence_refs: vec![],
        source_system: None,
        message: "v2 on v1".into(),
    });
    let v2_id = fs.put_commit(&v2).await.unwrap();
    fs.compare_and_set(Some(&commits[1]), &v2_id).await.unwrap();
    let branch = unique("mixed");

    let report = migration(dir.path(), &branch).await.run().await.unwrap();
    assert_eq!(report.outcome, MigrationOutcome::RefInstalled);
    assert_eq!(report.commit_objects, 3);
    assert_eq!(report.head_state_quads, Some(4));
    let destination = destination_ledger(&branch).await;
    assert_eq!(destination.head().await.unwrap(), Some(v2_id.clone()));
    let stored = destination
        .immutable_store()
        .get_commit(&v2_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.version(), 2);
    assert_eq!(
        source.state_at(&v2_id).await.unwrap(),
        destination.state_at(&v2_id).await.unwrap()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn interrupted_migration_resumes_to_the_same_result() {
    let dir = tempfile::tempdir().unwrap();
    let salt = unique("resume");
    let (_source, commits) = seed_v1_history(dir.path(), &salt).await;
    let fs = FileStore::open_existing(dir.path()).unwrap();
    let genesis = fs.get_commit(&commits[0]).await.unwrap().unwrap();
    let second = fs.get_commit(&commits[1]).await.unwrap().unwrap();
    let destination = PostgresImmutableStore::connect(
        &database_url(),
        V1Binding::BindTo(GraphId::new("default").unwrap()),
    )
    .await
    .unwrap();
    // Reference: an uninterrupted run on its own branch.
    let reference = migration(dir.path(), &unique("reference"))
        .await
        .run()
        .await
        .unwrap();

    // State A — interrupted between steps 5 and 6: all content published, no commits.
    for id in [&genesis.patch().0, &second.patch().0] {
        let bytes = fs.get_content(id).await.unwrap().unwrap();
        destination.put_content(id, &bytes).await.unwrap();
    }
    // State B — interrupted mid-import: genesis commit published, second not.
    destination.put_commit(&genesis).await.unwrap();
    let branch_b = unique("resume-b");
    let refs = PgRefStore::connect_ref(&database_url(), "default", &branch_b)
        .await
        .unwrap();
    assert_eq!(
        refs.head().await.unwrap(),
        None,
        "no ref before content verifies"
    );
    let report = migration(dir.path(), &branch_b).await.run().await.unwrap();
    assert_eq!(report.outcome, MigrationOutcome::RefInstalled);
    assert_eq!(
        report.destination_head_after.as_deref(),
        Some(commits[1].to_string().as_str())
    );
    assert_eq!(
        report.head_state_digest, reference.head_state_digest,
        "same result as uninterrupted"
    );
    assert_eq!(
        report.commits_published_in_order,
        reference.commits_published_in_order
    );

    // State C — interrupted between steps 8 and 11: all commits published, ref absent.
    destination.put_commit(&second).await.unwrap();
    let branch_c = unique("resume-c");
    let report = migration(dir.path(), &branch_c).await.run().await.unwrap();
    assert_eq!(report.outcome, MigrationOutcome::RefInstalled);
    assert_eq!(report.head_state_digest, reference.head_state_digest);
    for id in &commits {
        let src = fs.get_content(&id.0).await.unwrap().unwrap();
        let dst = destination.get_content(&id.0).await.unwrap().unwrap();
        assert_eq!(src, dst);
    }

    // State D — a prior run under a different binding: the v1 commits are already indexed
    // under 'default', so importing them into another importing graph is a binding
    // conflict, not a silent re-home.
    let other = unique("othergraph");
    create_graph(
        destination.pool(),
        &other,
        ledger_store::GraphStatus::Importing,
    )
    .await;
    let error = migration_to(dir.path(), &other, &unique("resume-d"))
        .await
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::GraphBindingConflict { .. }),
        "{error}"
    );
    let refs = PgRefStore::connect_ref(&database_url(), &other, "main")
        .await
        .unwrap();
    assert_eq!(refs.head().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn head_in_shared_refs_topology_is_verified_and_wrong_source_is_refused() {
    // The deployed pre-P1.2 topology: FileStore objects + PgRefStore head. `refs/main` on
    // disk is never written; HEAD lives only in PostgreSQL.
    let dir = tempfile::tempdir().unwrap();
    let branch = unique("topology");
    let fs = Arc::new(FileStore::open(dir.path()).unwrap());
    let refs: Arc<dyn RefStore> = Arc::new(
        PgRefStore::connect_ref(&database_url(), "default", &branch)
            .await
            .unwrap(),
    );
    let legacy = Ledger::with_ref_store(fs.clone(), refs.clone());
    let salt = unique("t");
    let c1 = legacy
        .commit(CommitRequest {
            expected_head: None,
            patch: quad_patch(&[&format!("<urn:material:{salt}> <urn:temperature> \"80\" .")]),
            author: "urn:agent:fs".into(),
            message: "genesis".into(),
            event_time: "2026-09-26T00:00:00Z".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        fs.head().await.unwrap(),
        None,
        "filesystem ref is not used in this topology"
    );
    assert_eq!(refs.head().await.unwrap(), Some(c1.clone()));

    // Wrong source directory: an unrelated (empty) store must not "succeed".
    let wrong = tempfile::tempdir().unwrap();
    FileStore::open(wrong.path()).unwrap();
    let error = migration(wrong.path(), &branch)
        .await
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::MissingTarget(ref h) if *h == c1),
        "{error}"
    );
    // A mistyped path is an error, not an empty store.
    assert!(FileStore::open_existing(dir.path().join("does-not-exist")).is_err());

    // The right source: HEAD is taken from the shared ref, content is imported and
    // verified against it, and the existing ref is reported as already migrated.
    let report = migration(dir.path(), &branch).await.run().await.unwrap();
    assert_eq!(report.outcome, MigrationOutcome::AlreadyMigrated);
    assert_eq!(report.source_head, None);
    assert_eq!(
        report.destination_head_before.as_deref(),
        Some(c1.to_string().as_str())
    );
    assert_eq!(report.head_state_quads, Some(1));
    let destination = destination_ledger(&branch).await;
    assert_eq!(destination.verify_head().await.unwrap(), Some(c1.clone()));
    assert_eq!(destination.state_at(&c1).await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn a_ref_is_never_installed_onto_another_graphs_history() {
    // Source: v2 history bound to graph A. Target ref: graph B (registered, importing).
    let url = database_url();
    let dir = tempfile::tempdir().unwrap();
    let fs = Arc::new(FileStore::open(dir.path()).unwrap());
    let graph_a = unique("graph-a");
    let graph_b = unique("graph-b");
    let destination = PostgresImmutableStore::connect(&url, V1Binding::Reject)
        .await
        .unwrap();
    create_graph(
        destination.pool(),
        &graph_a,
        ledger_store::GraphStatus::Active,
    )
    .await;
    create_graph(
        destination.pool(),
        &graph_b,
        ledger_store::GraphStatus::Importing,
    )
    .await;
    let p = quad_patch(&[&format!("<urn:s:{}> <urn:p> \"v\" .", unique("x"))]);
    fs.put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let head = fs
        .put_commit(&AnyCommit::V2(CommitV2 {
            graph_id: GraphId::new(&graph_a).unwrap(),
            parents: vec![],
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
            message: "graph A history".into(),
        }))
        .await
        .unwrap();
    fs.compare_and_set(None, &head).await.unwrap();

    let error = migration_to(dir.path(), &graph_b, "main")
        .await
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::GraphBindingConflict { ref commit, ref indexed, ref requested }
            if *commit == head && *indexed == graph_a && *requested == graph_b),
        "{error}"
    );
    let refs = PgRefStore::connect_ref(&url, &graph_b, "main")
        .await
        .unwrap();
    assert_eq!(
        refs.head().await.unwrap(),
        None,
        "ref for graph B untouched"
    );
    assert!(
        !destination.exists(&head.0).await.unwrap(),
        "nothing was published"
    );

    // A destination whose v1 binding names a different graph than the ref is refused at
    // construction, before any I/O.
    let mismatched =
        PostgresImmutableStore::connect(&url, V1Binding::BindTo(GraphId::new(&graph_a).unwrap()))
            .await
            .unwrap();
    assert!(
        FsToPgMigration::new(
            FileStore::open_existing(dir.path()).unwrap(),
            mismatched,
            refs
        )
        .is_err()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn missing_parent_and_corrupt_source_abort_before_any_ref_moves() {
    // Missing parent: delete the genesis object from the source.
    let dir = tempfile::tempdir().unwrap();
    let (_source, commits) = seed_v1_history(dir.path(), &unique("missing")).await;
    let fs = FileStore::open(dir.path()).unwrap();
    std::fs::remove_file(fs.object_path(&commits[0].0)).unwrap();
    let branch = unique("missing");
    let error = migration(dir.path(), &branch)
        .await
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::MissingParent(ref p) if *p == commits[0]),
        "{error}"
    );
    let refs = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    assert_eq!(refs.head().await.unwrap(), None);

    // Corrupt source object: overwrite the second commit's bytes in place.
    let dir = tempfile::tempdir().unwrap();
    let (_source, commits) = seed_v1_history(dir.path(), &unique("corrupt")).await;
    let fs = FileStore::open(dir.path()).unwrap();
    std::fs::write(fs.object_path(&commits[1].0), b"not the commit").unwrap();
    let branch = unique("corrupt");
    let error = migration(dir.path(), &branch)
        .await
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::CorruptObject { ref id, .. } if *id == commits[1].0),
        "{error}"
    );
    let refs = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    assert_eq!(refs.head().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn destination_collision_and_conflicting_head_abort_without_overwriting() {
    // Destination object collision: a damaged row already sits under one source id.
    let dir = tempfile::tempdir().unwrap();
    let (source, commits) = seed_v1_history(dir.path(), &unique("collide")).await;
    let genesis = source
        .immutable_store()
        .get_commit(&commits[0])
        .await
        .unwrap()
        .unwrap();
    let patch_id: ContentId = genesis.patch().0.clone();
    let destination = PostgresImmutableStore::connect(&database_url(), V1Binding::Reject)
        .await
        .unwrap();
    sqlx::query("INSERT INTO immutable_objects (id, bytes) VALUES ($1, $2)")
        .bind(patch_id.to_string())
        .bind(b"damaged".as_slice())
        .execute(destination.pool())
        .await
        .unwrap();
    let branch = unique("collide");
    let error = migration(dir.path(), &branch)
        .await
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::ObjectCollision(ref id) if *id == patch_id),
        "{error}"
    );
    let refs = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    assert_eq!(refs.head().await.unwrap(), None);
    let row = sqlx::query("SELECT bytes FROM immutable_objects WHERE id = $1")
        .bind(patch_id.to_string())
        .fetch_one(destination.pool())
        .await
        .unwrap();
    let stored: Vec<u8> = sqlx::Row::try_get(&row, "bytes").unwrap();
    assert_eq!(stored, b"damaged", "immutable bytes are never overwritten");

    // Conflicting destination HEAD: the ref already points elsewhere.
    let dir = tempfile::tempdir().unwrap();
    let (_source, commits) = seed_v1_history(dir.path(), &unique("conflict")).await;
    let branch = unique("conflict");
    let refs = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    let elsewhere = CommitId(ContentId::for_bytes(b"some other history"));
    refs.compare_and_set(None, &elsewhere).await.unwrap();
    let error = migration(dir.path(), &branch)
        .await
        .run()
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::HeadChanged { expected: Some(ref e), actual: Some(ref a) } if *e == commits[1] && *a == elsewhere),
        "{error}"
    );
    assert_eq!(
        refs.head().await.unwrap(),
        Some(elsewhere),
        "never overwritten silently"
    );
}

/// The cutover windows: refs move between verification and the ref step, or right after
/// the ref step. Deterministic via `run_with_hook`; no sleeps.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn cutover_fails_closed_when_refs_move_during_migration() {
    // 1. Filesystem source HEAD moves during the run.
    let dir = tempfile::tempdir().unwrap();
    let (_source, commits) = seed_v1_history(dir.path(), &unique("mv-src")).await;
    let branch = unique("mv-src");
    let fs = FileStore::open_existing(dir.path()).unwrap();
    let error = migration(dir.path(), &branch)
        .await
        .run_with_hook(|phase| {
            let (fs, commits) = (&fs, &commits);
            async move {
                if phase == CutoverPhase::BeforeCutover {
                    // A writer rewinds/moves the source ref while we are cutting over.
                    fs.compare_and_set(Some(&commits[1]), &commits[0])
                        .await
                        .unwrap();
                }
            }
        })
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::MigrationSourceMoved { before: Some(ref b), after: Some(ref a) }
            if *b == commits[1] && *a == commits[0]),
        "{error}"
    );
    let refs = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    assert_eq!(
        refs.head().await.unwrap(),
        None,
        "stale source state never installs a ref"
    );

    // 2. An existing destination HEAD moves during the run.
    let dir = tempfile::tempdir().unwrap();
    let (_source, commits) = seed_v1_history(dir.path(), &unique("mv-dst")).await;
    let branch = unique("mv-dst");
    let refs = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    refs.compare_and_set(None, &commits[1]).await.unwrap();
    let mover = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    let error = migration(dir.path(), &branch)
        .await
        .run_with_hook(|phase| {
            let (mover, commits) = (&mover, &commits);
            async move {
                if phase == CutoverPhase::BeforeCutover {
                    mover
                        .compare_and_set(Some(&commits[1]), &commits[0])
                        .await
                        .unwrap();
                }
            }
        })
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::HeadChanged { expected: Some(ref e), actual: Some(ref a) }
            if *e == commits[1] && *a == commits[0]),
        "{error}"
    );
    assert_eq!(
        refs.head().await.unwrap(),
        Some(commits[0].clone()),
        "never overwritten"
    );

    // 3. Absent destination installed concurrently with the *same* HEAD: idempotent.
    let dir = tempfile::tempdir().unwrap();
    let (_source, commits) = seed_v1_history(dir.path(), &unique("mv-same")).await;
    let branch = unique("mv-same");
    let twin = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    let report = migration(dir.path(), &branch)
        .await
        .run_with_hook(|phase| {
            let (twin, commits) = (&twin, &commits);
            async move {
                if phase == CutoverPhase::BeforeCutover {
                    twin.compare_and_set(None, &commits[1]).await.unwrap();
                }
            }
        })
        .await
        .unwrap();
    assert_eq!(report.outcome, MigrationOutcome::AlreadyMigrated);
    assert_eq!(
        report.destination_head_after.as_deref(),
        Some(commits[1].to_string().as_str())
    );

    // 4. Absent destination installed concurrently with a *different* HEAD: HEAD_CHANGED.
    let dir = tempfile::tempdir().unwrap();
    let (_source, commits) = seed_v1_history(dir.path(), &unique("mv-diff")).await;
    let branch = unique("mv-diff");
    let rival = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    let error = migration(dir.path(), &branch)
        .await
        .run_with_hook(|phase| {
            let (rival, commits) = (&rival, &commits);
            async move {
                if phase == CutoverPhase::BeforeCutover {
                    rival.compare_and_set(None, &commits[0]).await.unwrap();
                }
            }
        })
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::HeadChanged { expected: None, actual: Some(ref a) } if *a == commits[0]),
        "{error}"
    );
    assert_eq!(rival.head().await.unwrap(), Some(commits[0].clone()));

    // 5. No HEAD on either side, and a destination ref appears during the run: the
    //    content-only outcome must not be reported over an unverified ref.
    let dir = tempfile::tempdir().unwrap();
    let fs = Arc::new(FileStore::open(dir.path()).unwrap());
    let p = quad_patch(&[&format!("<urn:s:{}> <urn:p> \"v\" .", unique("nohead"))]);
    fs.put_content(&p.id().0, &p.canonical_bytes())
        .await
        .unwrap();
    let branch = unique("mv-nohead");
    let appearing = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    let error = migration(dir.path(), &branch)
        .await
        .run_with_hook(|phase| {
            let (appearing, commits) = (&appearing, &commits);
            async move {
                if phase == CutoverPhase::BeforeCutover {
                    appearing.compare_and_set(None, &commits[0]).await.unwrap();
                }
            }
        })
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::HeadChanged { expected: None, actual: Some(ref a) } if *a == commits[0]),
        "{error}"
    );

    // 6. The ref is installed and a writer moves it before the final agreement check: the
    //    run reports HEAD_CHANGED naming the mover's HEAD (installed-then-moved), never Ok.
    let dir = tempfile::tempdir().unwrap();
    let (_source, commits) = seed_v1_history(dir.path(), &unique("mv-after")).await;
    let branch = unique("mv-after");
    let mover = PgRefStore::connect_ref(&database_url(), "default", &branch)
        .await
        .unwrap();
    let error = migration(dir.path(), &branch)
        .await
        .run_with_hook(|phase| {
            let (mover, commits) = (&mover, &commits);
            async move {
                if phase == CutoverPhase::AfterRefInstall {
                    mover
                        .compare_and_set(Some(&commits[1]), &commits[0])
                        .await
                        .unwrap();
                }
            }
        })
        .await
        .unwrap_err();
    assert!(
        matches!(error, LedgerError::HeadChanged { expected: Some(ref e), actual: Some(ref a) }
            if *e == commits[1] && *a == commits[0]),
        "{error}"
    );
    assert_eq!(mover.head().await.unwrap(), Some(commits[0].clone()));
}
