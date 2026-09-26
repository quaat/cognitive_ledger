//! Real-PostgreSQL evidence for atomic workflow persistence (ADR-0013, Plan 0004 P1.3):
//! one transaction per accepted transition, lineage against the verified index,
//! idempotency designed for concurrency, effective-delta candidates, deterministic fault
//! injection, and database-level invariants. Every concurrency scenario uses independent
//! pools ("replicas"); correctness must come from PostgreSQL, never from process state.
#![cfg(feature = "postgres")]

use ledger_core::{
    AuthenticatedPrincipal, CommitId, ContentId, GraphId, ImmutableStore, LedgerError, PrincipalId,
    PrincipalType, RefStore, TenantId,
};
use ledger_rdf::{DeltaPolicy, Operation, OperationKind, Patch, apply_patch, effective_delta};
use ledger_store::{
    AcceptRequest, FailPoint, GraphStatus, Ledger, NewGraph, PgRefStore, PostgresLedgerStore,
    PrepareRequest, RejectRequest, RequestScope, V1Binding, ValidationPolicy,
};
use sqlx::Row;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{collections::BTreeSet, sync::Arc};

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
    PostgresLedgerStore::connect(&database_url(), V1Binding::Reject)
        .await
        .unwrap()
}

async fn graph(store: &PostgresLedgerStore, status: GraphStatus) -> GraphId {
    let id = GraphId::new(unique("wf")).unwrap();
    store
        .graphs()
        .create(&NewGraph {
            graph_id: id.clone(),
            tenant_id: TenantId::new("tenant-wf").unwrap(),
            knowledge_base_id: None,
            purpose: None,
            status,
        })
        .await
        .unwrap();
    id
}

fn principal(id: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal {
        principal_id: PrincipalId::new(format!("urn:sculpin:agent:{id}")).unwrap(),
        principal_type: PrincipalType::Agent,
        tenant_id: TenantId::new("tenant-wf").unwrap(),
        on_behalf_of: None,
    }
}

fn scope(graph: &GraphId, key: &str, digest: &[u8]) -> RequestScope {
    RequestScope {
        principal: principal("curator"),
        graph: graph.clone(),
        idempotency_key: key.to_owned(),
        request_digest: ContentId::for_bytes(digest),
        correlation_id: Some(format!("corr-{key}")),
    }
}

fn quad(value: &str) -> ledger_rdf::Quad {
    format!("<urn:s:{value}> <urn:p> \"{value}\" .")
        .parse()
        .unwrap()
}
fn add(value: &str) -> Operation {
    Operation {
        kind: OperationKind::Add,
        quad: quad(value),
    }
}
fn del(value: &str) -> Operation {
    Operation {
        kind: OperationKind::Delete,
        quad: quad(value),
    }
}

/// The digest the API layer would compute: every request field that must be identical for
/// a retry to count as the same request.
fn prepare_request(
    graph: &GraphId,
    key: &str,
    expected_head: Option<CommitId>,
    ops: Vec<Operation>,
) -> PrepareRequest {
    let requested = Patch::new(ops).unwrap();
    let evidence = format!("urn:evidence:{}:{key}", graph.as_str());
    let mut digest = Vec::new();
    digest.extend_from_slice(b"main\0");
    digest.extend_from_slice(
        expected_head
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default()
            .as_bytes(),
    );
    digest.push(0);
    digest.extend_from_slice(&requested.canonical_bytes());
    digest.extend_from_slice(b"\0cognitive-correction\0");
    digest.extend_from_slice(evidence.as_bytes());
    digest.push(0);
    digest.extend_from_slice(key.as_bytes());
    PrepareRequest {
        scope: scope(graph, key, &digest),
        branch: "main".into(),
        expected_head,
        requested,
        activity: "cognitive-correction".into(),
        event_time: None,
        evidence_refs: vec![evidence],
        source_system: None,
        message: key.to_owned(),
    }
}

async fn indexed_commits(store: &PostgresLedgerStore, graph: &GraphId) -> i64 {
    let row = sqlx::query("SELECT count(*) AS n FROM commit_index WHERE graph_id = $1")
        .bind(graph.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    row.get("n")
}

fn accept_request(
    graph: &GraphId,
    key: &str,
    expected_head: Option<CommitId>,
    candidate: &CommitId,
) -> AcceptRequest {
    AcceptRequest {
        scope: scope(graph, key, candidate.to_string().as_bytes()),
        branch: "main".into(),
        expected_head,
        candidate: candidate.clone(),
        reason: None,
        validation: ValidationPolicy::NoValidation,
    }
}

struct Counts {
    events: i64,
    decisions: i64,
    accepted: i64,
    outbox: i64,
    idempotency: i64,
    proposals: i64,
}

async fn counts(store: &PostgresLedgerStore, graph: &GraphId) -> Counts {
    let one = |sql: &'static str| {
        let pool = store.pool().clone();
        let g = graph.to_string();
        async move {
            let row = sqlx::query(sql).bind(g).fetch_one(&pool).await.unwrap();
            let n: i64 = row.get("n");
            n
        }
    };
    Counts {
        events: one("SELECT count(*) AS n FROM ref_events WHERE graph_id = $1").await,
        decisions: one("SELECT count(*) AS n FROM decisions WHERE graph_id = $1").await,
        accepted: one(
            "SELECT count(*) AS n FROM decisions WHERE graph_id = $1 AND decision = 'accepted'",
        )
        .await,
        outbox: one("SELECT count(*) AS n FROM projection_outbox WHERE graph_id = $1").await,
        idempotency: one("SELECT count(*) AS n FROM idempotency WHERE graph_id = $1").await,
        proposals: one("SELECT count(*) AS n FROM proposals WHERE graph_id = $1").await,
    }
}

async fn ref_row(store: &PostgresLedgerStore, graph: &GraphId) -> Option<(CommitId, i64)> {
    sqlx::query("SELECT head, version FROM refs WHERE graph_id = $1 AND branch = 'main'")
        .bind(graph.to_string())
        .fetch_optional(store.pool())
        .await
        .unwrap()
        .map(|row| {
            let head: String = row.get("head");
            (head.parse().unwrap(), row.get("version"))
        })
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn genesis_and_advance_each_produce_exactly_one_event_decision_and_outbox_row() {
    let _ = IGNORE;
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();

    // Genesis: everything requested is effective against the empty base.
    let p1 = wf
        .prepare(&prepare_request(&g, "prep-1", None, vec![add("a")]))
        .await
        .unwrap();
    assert!(!p1.replayed);
    assert_eq!(p1.requested_patch, p1.effective_patch);
    assert!(store.immutable().exists(&p1.candidate.0).await.unwrap());
    assert_eq!(ref_row(&store, &g).await, None, "prepare moves no ref");
    let a1 = wf
        .accept(&accept_request(&g, "acc-1", None, &p1.candidate))
        .await
        .unwrap();
    assert_eq!(a1.ref_version, 1);
    assert_eq!(a1.head, p1.candidate);
    assert_eq!(ref_row(&store, &g).await, Some((p1.candidate.clone(), 1)));
    let c = counts(&store, &g).await;
    assert_eq!(
        (
            c.events,
            c.decisions,
            c.accepted,
            c.outbox,
            c.idempotency,
            c.proposals
        ),
        (1, 1, 1, 1, 2, 1)
    );

    // Advance: re-adding `a` collapses; only `b` enters history (ADR-0008).
    let p2 = wf
        .prepare(&prepare_request(
            &g,
            "prep-2",
            Some(p1.candidate.clone()),
            vec![add("a"), add("b")],
        ))
        .await
        .unwrap();
    assert_ne!(
        p2.requested_patch, p2.effective_patch,
        "requested intent is kept apart"
    );
    let effective_bytes = store
        .immutable()
        .get_content(&p2.effective_patch.0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        Patch::from_canonical_bytes(&effective_bytes).unwrap(),
        Patch::new([add("b")]).unwrap()
    );
    let requested_bytes = store
        .immutable()
        .get_content(&p2.requested_patch.0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        Patch::from_canonical_bytes(&requested_bytes).unwrap(),
        Patch::new([add("a"), add("b")]).unwrap()
    );
    let a2 = wf
        .accept(&accept_request(
            &g,
            "acc-2",
            Some(p1.candidate.clone()),
            &p2.candidate,
        ))
        .await
        .unwrap();
    assert_eq!(a2.ref_version, 2);
    assert_eq!(ref_row(&store, &g).await, Some((p2.candidate.clone(), 2)));
    let c = counts(&store, &g).await;
    assert_eq!(
        (
            c.events,
            c.decisions,
            c.accepted,
            c.outbox,
            c.idempotency,
            c.proposals
        ),
        (2, 2, 2, 2, 4, 2)
    );
    let event = sqlx::query(
        "SELECT operation, old_head, old_version, new_version FROM ref_events \
         WHERE graph_id = $1 AND new_version = 2",
    )
    .bind(g.to_string())
    .fetch_one(store.pool())
    .await
    .unwrap();
    let op: String = event.get("operation");
    let old_head: Option<String> = event.get("old_head");
    let old_version: Option<i64> = event.get("old_version");
    assert_eq!(op, "advance");
    assert_eq!(old_head.as_deref(), Some(p1.candidate.to_string().as_str()));
    assert_eq!(old_version, Some(1));
    let outbox = sqlx::query(
        "SELECT commit_id, ref_version, event_kind, delivered_at::text AS delivered \
         FROM projection_outbox WHERE graph_id = $1 ORDER BY ref_version",
    )
    .bind(g.to_string())
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(outbox.len(), 2);
    let last_commit: String = outbox[1].get("commit_id");
    let last_version: i64 = outbox[1].get("ref_version");
    let delivered: Option<String> = outbox[1].get("delivered");
    assert_eq!(
        (last_commit, last_version, delivered),
        (p2.candidate.to_string(), 2, None)
    );

    // Reconstruction through the shared store sees exactly {a, b}.
    let ledger = Ledger::with_stores(
        Arc::new(store.immutable().clone()) as Arc<dyn ImmutableStore>,
        Arc::new(PgRefStore::with_ref_migrated(
            store.pool().clone(),
            g.as_str(),
            "main",
        )) as Arc<dyn RefStore>,
    );
    let state = ledger.state_at(&p2.candidate).await.unwrap();
    assert_eq!(state, [quad("a"), quad("b")].into_iter().collect());
    assert_eq!(
        store
            .immutable()
            .verify_commits(&[p1.candidate, p2.candidate])
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn strict_effective_delta_refuses_stale_deletes_and_no_op_requests_without_persisting() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    let p1 = wf
        .prepare(&prepare_request(&g, "prep-1", None, vec![add("a")]))
        .await
        .unwrap();
    wf.accept(&accept_request(&g, "acc-1", None, &p1.candidate))
        .await
        .unwrap();
    let before = counts(&store, &g).await;

    let stale = wf
        .prepare(&prepare_request(
            &g,
            "prep-stale",
            Some(p1.candidate.clone()),
            vec![add("b"), del("zzz")],
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(stale, LedgerError::BaseMismatch(ref q) if q.contains("urn:s:zzz")),
        "{stale}"
    );
    let noop = wf
        .prepare(&prepare_request(
            &g,
            "prep-noop",
            Some(p1.candidate.clone()),
            vec![add("a")],
        ))
        .await
        .unwrap_err();
    assert!(matches!(noop, LedgerError::NoEffectiveChange), "{noop}");
    let after = counts(&store, &g).await;
    assert_eq!(
        after.proposals, before.proposals,
        "no proposal for a refused request"
    );
    assert_eq!(
        after.idempotency, before.idempotency,
        "no idempotency result for a refused request"
    );
    // A stale expected head is HEAD_CHANGED, not a base mismatch.
    let stale_head = wf
        .prepare(&prepare_request(
            &g,
            "prep-stale-head",
            None,
            vec![add("b")],
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(stale_head, LedgerError::HeadChanged { .. }),
        "{stale_head}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn effective_delta_is_identical_from_materialized_state_and_full_reconstruction() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    let mut head = None;
    let mut materialized = BTreeSet::new();
    for (i, ops) in [
        vec![add("a"), add("b")],
        vec![del("a"), add("c")],
        vec![add("d")],
    ]
    .into_iter()
    .enumerate()
    {
        let p = wf
            .prepare(&prepare_request(
                &g,
                &format!("p{i}"),
                head.clone(),
                ops.clone(),
            ))
            .await
            .unwrap();
        wf.accept(&accept_request(
            &g,
            &format!("a{i}"),
            head.clone(),
            &p.candidate,
        ))
        .await
        .unwrap();
        apply_patch(&mut materialized, &Patch::new(ops).unwrap());
        head = Some(p.candidate);
    }
    let head = head.unwrap();
    let ledger = Ledger::with_stores(
        Arc::new(store.immutable().clone()) as Arc<dyn ImmutableStore>,
        Arc::new(PgRefStore::with_ref_migrated(
            store.pool().clone(),
            g.as_str(),
            "main",
        )) as Arc<dyn RefStore>,
    );
    let reconstructed = ledger.state_at(&head).await.unwrap();
    assert_eq!(reconstructed, materialized);
    let request = Patch::new([add("b"), add("e"), del("c")]).unwrap();
    let from_materialized = effective_delta(&materialized, &request, DeltaPolicy::Strict).unwrap();
    let from_reconstructed =
        effective_delta(&reconstructed, &request, DeltaPolicy::Strict).unwrap();
    assert_eq!(from_materialized.id(), from_reconstructed.id());
    assert_eq!(from_materialized, Patch::new([add("e"), del("c")]).unwrap());
    // And the repository derives the same effective patch id from its own reconstruction.
    let p = wf
        .prepare(&prepare_request(
            &g,
            "final",
            Some(head),
            vec![add("b"), add("e"), del("c")],
        ))
        .await
        .unwrap();
    assert_eq!(p.effective_patch, from_materialized.id());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn prepare_idempotency_yields_one_candidate_identity_even_under_concurrency() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    let request = prepare_request(&g, "prep-idem", None, vec![add("a")]);
    let first = wf.prepare(&request).await.unwrap();
    // Lost response + retry: the exact original CommitId, flagged as a replay.
    let again = wf.prepare(&request).await.unwrap();
    assert_eq!(again.candidate, first.candidate);
    assert_eq!(again.proposal_id, first.proposal_id);
    assert_eq!(again.effective_patch, first.effective_patch);
    assert!(again.replayed);
    // Same key, different payload: conflict, nothing new.
    let mut different = prepare_request(&g, "prep-idem", None, vec![add("b")]);
    different.scope.request_digest = ContentId::for_bytes(b"different");
    assert!(matches!(
        wf.prepare(&different).await,
        Err(LedgerError::IdempotencyConflict)
    ));
    let c = counts(&store, &g).await;
    assert_eq!((c.proposals, c.idempotency), (1, 1));

    // Two replicas prepare the same key simultaneously: one candidate identity.
    let replica_a = PostgresLedgerStore::connect(&database_url(), V1Binding::Reject)
        .await
        .unwrap();
    let replica_b = PostgresLedgerStore::connect(&database_url(), V1Binding::Reject)
        .await
        .unwrap();
    let indexed_before = indexed_commits(&store, &g).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut handles = Vec::new();
    let shared_request = prepare_request(&g, "prep-race", None, vec![add("race")]);
    for replica in [replica_a, replica_b] {
        let barrier = Arc::clone(&barrier);
        let request = shared_request.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            replica.workflows().prepare(&request).await
        }));
    }
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap().unwrap());
    }
    assert_eq!(results[0].candidate, results[1].candidate, "{results:?}");
    assert_eq!(results[0].proposal_id, results[1].proposal_id);
    assert_eq!(
        results.iter().filter(|r| !r.replayed).count(),
        1,
        "exactly one winner"
    );
    let candidates = sqlx::query(
        "SELECT count(*) AS n FROM proposals WHERE graph_id = $1 AND requested_patch_id = $2",
    )
    .bind(g.to_string())
    .bind(Patch::new([add("race")]).unwrap().id().to_string())
    .fetch_one(store.pool())
    .await
    .unwrap();
    let n: i64 = candidates.get("n");
    assert_eq!(n, 1, "the loser's candidate was rolled back, not published");
    assert_eq!(
        indexed_commits(&store, &g).await,
        indexed_before + 1,
        "exactly one candidate commit was indexed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn acceptance_is_idempotent_and_serialized_across_replicas() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    let p1 = wf
        .prepare(&prepare_request(&g, "prep-1", None, vec![add("a")]))
        .await
        .unwrap();
    let request = accept_request(&g, "acc-1", None, &p1.candidate);
    let first = wf.accept(&request).await.unwrap();
    // Lost response + retry returns the original outcome: no second event/decision/outbox.
    let again = wf.accept(&request).await.unwrap();
    assert_eq!(
        (
            again.head.clone(),
            again.ref_version,
            again.decision_id,
            again.ref_event_id,
            again.outbox_id
        ),
        (
            first.head.clone(),
            first.ref_version,
            first.decision_id,
            first.ref_event_id,
            first.outbox_id
        )
    );
    assert!(again.replayed);
    let c = counts(&store, &g).await;
    assert_eq!((c.events, c.decisions, c.outbox), (1, 1, 1));
    // Same key, different payload.
    let mut different = request.clone();
    different.scope.request_digest = ContentId::for_bytes(b"other");
    assert!(matches!(
        wf.accept(&different).await,
        Err(LedgerError::IdempotencyConflict)
    ));

    // Same-HEAD competition: N replicas each accept a distinct valid child of HEAD.
    let n = 6;
    let mut candidates = Vec::new();
    for i in 0..n {
        let p = wf
            .prepare(&prepare_request(
                &g,
                &format!("child-{i}"),
                Some(p1.candidate.clone()),
                vec![add(&format!("c{i}"))],
            ))
            .await
            .unwrap();
        candidates.push(p.candidate);
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let mut handles = Vec::new();
    for (i, candidate) in candidates.iter().enumerate() {
        let replica = PostgresLedgerStore::connect(&database_url(), V1Binding::Reject)
            .await
            .unwrap();
        let barrier = Arc::clone(&barrier);
        let request = accept_request(
            &g,
            &format!("acc-child-{i}"),
            Some(p1.candidate.clone()),
            candidate,
        );
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            replica.workflows().accept(&request).await
        }));
    }
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }
    let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(LedgerError::HeadChanged { .. })))
            .count(),
        n - 1,
        "{results:?}"
    );
    assert_eq!(winners[0].ref_version, 2);
    assert_eq!(
        ref_row(&store, &g).await,
        Some((winners[0].head.clone(), 2))
    );
    let c = counts(&store, &g).await;
    assert_eq!(
        (c.events, c.accepted, c.outbox),
        (2, 2, 2),
        "one accepted advance only"
    );

    // Same key + same payload from many replicas concurrently: one logical operation.
    let p_next = wf
        .prepare(&prepare_request(
            &g,
            "prep-3",
            Some(winners[0].head.clone()),
            vec![add("next")],
        ))
        .await
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let replica = PostgresLedgerStore::connect(&database_url(), V1Binding::Reject)
            .await
            .unwrap();
        let barrier = Arc::clone(&barrier);
        let request = accept_request(
            &g,
            "acc-shared-key",
            Some(winners[0].head.clone()),
            &p_next.candidate,
        );
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            replica.workflows().accept(&request).await
        }));
    }
    let mut outcomes = Vec::new();
    for handle in handles {
        outcomes.push(handle.await.unwrap().unwrap());
    }
    assert!(
        outcomes
            .iter()
            .all(|o| o.ref_version == 3 && o.head == p_next.candidate),
        "{outcomes:?}"
    );
    assert_eq!(outcomes.iter().filter(|o| !o.replayed).count(), 1);
    let first_ids = (
        outcomes[0].decision_id,
        outcomes[0].ref_event_id,
        outcomes[0].outbox_id,
    );
    assert!(
        outcomes
            .iter()
            .all(|o| (o.decision_id, o.ref_event_id, o.outbox_id) == first_ids),
        "every replay names the same decision, event and outbox row: {outcomes:?}"
    );
    let c = counts(&store, &g).await;
    assert_eq!(
        (c.events, c.accepted, c.outbox),
        (3, 3, 3),
        "one acceptance effect"
    );
    // A late replay of the very first acceptance still reports its own outcome.
    let late = wf.accept(&request).await.unwrap();
    assert!(late.replayed);
    assert_eq!((late.head, late.ref_version), (p1.candidate.clone(), 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn concurrent_genesis_accepts_replay_for_the_same_key_and_race_for_different_candidates() {
    let store = store().await;
    let wf = store.workflows();
    // Same key, same candidate, four replicas, no ref row to lock: exactly one wins, the
    // rest replay the identical outcome (advisory lock on the idempotency scope).
    let g = graph(&store, GraphStatus::Active).await;
    let p = wf
        .prepare(&prepare_request(&g, "p", None, vec![add("a")]))
        .await
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let replica = PostgresLedgerStore::connect(&database_url(), V1Binding::Reject)
            .await
            .unwrap();
        let barrier = Arc::clone(&barrier);
        let request = accept_request(&g, "genesis-shared", None, &p.candidate);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            replica.workflows().accept(&request).await
        }));
    }
    let mut outcomes = Vec::new();
    for handle in handles {
        outcomes.push(handle.await.unwrap().unwrap());
    }
    assert_eq!(
        outcomes.iter().filter(|o| !o.replayed).count(),
        1,
        "{outcomes:?}"
    );
    let ids = (
        outcomes[0].decision_id,
        outcomes[0].ref_event_id,
        outcomes[0].outbox_id,
    );
    assert!(
        outcomes
            .iter()
            .all(|o| { (o.decision_id, o.ref_event_id, o.outbox_id) == ids && o.ref_version == 1 })
    );
    let c = counts(&store, &g).await;
    assert_eq!(
        (c.events, c.accepted, c.outbox, c.idempotency),
        (1, 1, 1, 2)
    );

    // Different keys, different genesis candidates on an empty ref: one winner, the rest
    // see the winner's head.
    let g2 = graph(&store, GraphStatus::Active).await;
    let mut candidates = Vec::new();
    for i in 0..4 {
        let p = wf
            .prepare(&prepare_request(
                &g2,
                &format!("g{i}"),
                None,
                vec![add(&format!("g{i}"))],
            ))
            .await
            .unwrap();
        candidates.push(p.candidate);
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let mut handles = Vec::new();
    for (i, candidate) in candidates.iter().enumerate() {
        let replica = PostgresLedgerStore::connect(&database_url(), V1Binding::Reject)
            .await
            .unwrap();
        let barrier = Arc::clone(&barrier);
        let request = accept_request(&g2, &format!("acc-g{i}"), None, candidate);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            replica.workflows().accept(&request).await
        }));
    }
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }
    let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    let winner_head = winners[0].head.clone();
    for r in &results {
        if let Err(e) = r {
            assert!(
                matches!(e, LedgerError::HeadChanged { expected: None, actual: Some(a) } if *a == winner_head),
                "{e}"
            );
        }
    }
    assert_eq!(ref_row(&store, &g2).await, Some((winner_head, 1)));
    let c = counts(&store, &g2).await;
    assert_eq!((c.events, c.accepted, c.outbox), (1, 1, 1));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn bad_lineage_other_graph_and_unknown_candidates_have_no_workflow_side_effects() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let other = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    // A genesis proposal prepared before the ref exists (a "sideways" candidate later).
    let sideways = wf
        .prepare(&prepare_request(&g, "side", None, vec![add("s")]))
        .await
        .unwrap();
    let p1 = wf
        .prepare(&prepare_request(&g, "p1", None, vec![add("a")]))
        .await
        .unwrap();
    wf.accept(&accept_request(&g, "a1", None, &p1.candidate))
        .await
        .unwrap();
    // Two children of p1: one becomes HEAD, the other is a stale sibling (wrong first parent).
    let sibling = wf
        .prepare(&prepare_request(
            &g,
            "sib",
            Some(p1.candidate.clone()),
            vec![add("x")],
        ))
        .await
        .unwrap();
    let p2 = wf
        .prepare(&prepare_request(
            &g,
            "p2",
            Some(p1.candidate.clone()),
            vec![add("b")],
        ))
        .await
        .unwrap();
    wf.accept(&accept_request(
        &g,
        "a2",
        Some(p1.candidate.clone()),
        &p2.candidate,
    ))
    .await
    .unwrap();
    let foreign = wf
        .prepare(&prepare_request(&other, "fp", None, vec![add("f")]))
        .await
        .unwrap();
    let head = p2.candidate.clone();
    let before = counts(&store, &g).await;
    let ref_before = ref_row(&store, &g).await;

    // Each case names the rule that must fire, so a test cannot pass for the wrong reason.
    let cases: Vec<(&str, CommitId, &str)> = vec![
        (
            "historical ancestor (already accepted)",
            p1.candidate.clone(),
            "terminal decision",
        ),
        (
            "sideways genesis",
            sideways.candidate.clone(),
            "different ref or expected head",
        ),
        (
            "wrong first parent",
            sibling.candidate.clone(),
            "different ref or expected head",
        ),
        (
            "foreign graph",
            foreign.candidate.clone(),
            "belongs to another graph",
        ),
        (
            "unknown content id",
            CommitId(ContentId::for_bytes(b"never prepared")),
            "not an indexed commit",
        ),
        (
            "non-commit content id",
            CommitId(p1.effective_patch.0.clone()),
            "not an indexed commit",
        ),
    ];
    for (i, (name, candidate, rule)) in cases.iter().enumerate() {
        let result = wf
            .accept(&accept_request(
                &g,
                &format!("bad-{i}"),
                Some(head.clone()),
                candidate,
            ))
            .await;
        match result {
            Err(LedgerError::LineageMismatch(m)) => assert!(m.contains(rule), "{name}: {m}"),
            other => panic!("{name}: {other:?}"),
        }
    }
    // A stale expected head is HEAD_CHANGED (the ref moved), never a lineage error.
    let stale = wf
        .accept(&accept_request(
            &g,
            "stale",
            Some(p1.candidate.clone()),
            &sibling.candidate,
        ))
        .await;
    assert!(
        matches!(stale, Err(LedgerError::HeadChanged { .. })),
        "{stale:?}"
    );
    // A rejected candidate cannot be accepted afterwards, and vice versa.
    wf.reject(&RejectRequest {
        scope: scope(&g, "rej-sib", b"reject sibling"),
        branch: "main".into(),
        candidate: sibling.candidate.clone(),
        reason: "stale sibling".into(),
    })
    .await
    .unwrap();
    let redecide = wf
        .accept(&accept_request(
            &g,
            "redecide",
            Some(head.clone()),
            &sibling.candidate,
        ))
        .await;
    assert!(
        matches!(redecide, Err(LedgerError::LineageMismatch(ref m)) if m.contains("terminal decision")),
        "{redecide:?}"
    );

    let after = counts(&store, &g).await;
    assert_eq!(ref_row(&store, &g).await, ref_before, "ref untouched");
    assert_eq!(
        (after.events, after.accepted, after.outbox),
        (before.events, before.accepted, before.outbox)
    );
    assert_eq!(
        after.decisions,
        before.decisions + 1,
        "only the explicit rejection was recorded"
    );
    assert_eq!(
        after.idempotency,
        before.idempotency + 1,
        "only the rejection stored a result"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn normal_acceptance_requires_an_active_graph() {
    let store = store().await;
    let wf = store.workflows();
    let active = graph(&store, GraphStatus::Active).await;
    let p = wf
        .prepare(&prepare_request(&active, "p", None, vec![add("a")]))
        .await
        .unwrap();
    for status in [
        GraphStatus::Importing,
        GraphStatus::Archived,
        GraphStatus::Bootstrap,
    ] {
        let g = graph(&store, status).await;
        let prep = wf
            .prepare(&prepare_request(&g, "p", None, vec![add("a")]))
            .await;
        assert!(
            matches!(prep, Err(LedgerError::GraphNotActive { .. })),
            "{status:?}: {prep:?}"
        );
        let acc = wf
            .accept(&accept_request(&g, "a", None, &p.candidate))
            .await;
        assert!(
            matches!(acc, Err(LedgerError::GraphNotActive { .. })),
            "{status:?}: {acc:?}"
        );
        let c = counts(&store, &g).await;
        assert_eq!(
            (c.events, c.decisions, c.outbox, c.idempotency, c.proposals),
            (0, 0, 0, 0, 0)
        );
    }
    // The bootstrap graph itself (used by the raw v1 write path) is not accepted through
    // the workflow either: another tenant sees it as unknown, its own (non-production)
    // tenant sees it as not active.
    let bootstrap = GraphId::new("default").unwrap();
    let prep = wf
        .prepare(&prepare_request(&bootstrap, "p", None, vec![add("a")]))
        .await;
    assert!(
        matches!(prep, Err(LedgerError::UnknownGraph(_))),
        "{prep:?}"
    );
    let mut as_bootstrap_tenant = prepare_request(&bootstrap, "p", None, vec![add("a")]);
    as_bootstrap_tenant.scope.principal.tenant_id = TenantId::new("bootstrap").unwrap();
    let prep = wf.prepare(&as_bootstrap_tenant).await;
    assert!(
        matches!(prep, Err(LedgerError::GraphNotActive { ref status, .. }) if status == "bootstrap"),
        "{prep:?}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn every_injected_failure_before_commit_leaves_no_mutable_effect_and_retry_succeeds() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    let p1 = wf
        .prepare(&prepare_request(&g, "p1", None, vec![add("a")]))
        .await
        .unwrap();
    wf.accept(&accept_request(&g, "a1", None, &p1.candidate))
        .await
        .unwrap();
    let p2 = wf
        .prepare(&prepare_request(
            &g,
            "p2",
            Some(p1.candidate.clone()),
            vec![add("b")],
        ))
        .await
        .unwrap();
    let baseline = counts(&store, &g).await;
    let ref_baseline = ref_row(&store, &g).await;

    let accept_points = [
        FailPoint::AfterIdempotencyCheck,
        FailPoint::AfterLineageValidation,
        FailPoint::AfterRefUpdate,
        FailPoint::AfterRefEvent,
        FailPoint::AfterDecision,
        FailPoint::AfterOutbox,
        FailPoint::BeforeCommit,
    ];
    for point in accept_points {
        let faulty = wf.clone().with_failpoint(point);
        let result = faulty
            .accept(&accept_request(
                &g,
                "a2",
                Some(p1.candidate.clone()),
                &p2.candidate,
            ))
            .await;
        assert!(
            matches!(result, Err(LedgerError::Storage(ref m)) if m.contains("injected")),
            "{point:?}: {result:?}"
        );
        let c = counts(&store, &g).await;
        assert_eq!(
            ref_row(&store, &g).await,
            ref_baseline,
            "{point:?}: ref moved"
        );
        assert_eq!(
            (c.events, c.decisions, c.outbox, c.idempotency),
            (
                baseline.events,
                baseline.decisions,
                baseline.outbox,
                baseline.idempotency
            ),
            "{point:?}: partial workflow visible"
        );
    }
    // Retry without the fault: the same request succeeds exactly once.
    let accepted = wf
        .accept(&accept_request(
            &g,
            "a2",
            Some(p1.candidate.clone()),
            &p2.candidate,
        ))
        .await
        .unwrap();
    assert_eq!(accepted.ref_version, 2);
    assert!(!accepted.replayed);
    // Lost response after COMMIT: the retry replays the committed outcome.
    let replay = wf
        .accept(&accept_request(
            &g,
            "a2",
            Some(p1.candidate.clone()),
            &p2.candidate,
        ))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(
        (replay.decision_id, replay.ref_event_id, replay.outbox_id),
        (
            accepted.decision_id,
            accepted.ref_event_id,
            accepted.outbox_id
        )
    );
    let c = counts(&store, &g).await;
    assert_eq!(
        (c.events, c.accepted, c.outbox),
        (
            baseline.events + 1,
            baseline.accepted + 1,
            baseline.outbox + 1
        )
    );

    // Prepare failpoints: no proposal, no candidate object, no idempotency row.
    let prepare_points = [
        FailPoint::AfterIdempotencyCheck,
        FailPoint::AfterLineageValidation,
        FailPoint::AfterDecision,
        FailPoint::BeforeCommit,
    ];
    let request = prepare_request(&g, "p3", Some(p2.candidate.clone()), vec![add("c")]);
    let before = counts(&store, &g).await;
    let indexed_before = indexed_commits(&store, &g).await;
    for point in prepare_points {
        let faulty = wf.clone().with_failpoint(point);
        let result = faulty.prepare(&request).await;
        assert!(
            matches!(result, Err(LedgerError::Storage(ref m)) if m.contains("injected")),
            "{point:?}: {result:?}"
        );
        let c = counts(&store, &g).await;
        assert_eq!(
            (c.proposals, c.idempotency),
            (before.proposals, before.idempotency),
            "{point:?}"
        );
        assert_eq!(
            indexed_commits(&store, &g).await,
            indexed_before,
            "{point:?}: candidate leaked"
        );
    }
    let prepared = wf.prepare(&request).await.unwrap();
    assert!(!prepared.replayed);
    assert!(
        store
            .immutable()
            .exists(&prepared.candidate.0)
            .await
            .unwrap()
    );
    assert_eq!(indexed_commits(&store, &g).await, indexed_before + 1);

    // Reject failpoints: no decision, no idempotency result; then the retry succeeds once.
    let reject = RejectRequest {
        scope: scope(&g, "rej-fault", b"reject p3"),
        branch: "main".into(),
        candidate: prepared.candidate.clone(),
        reason: "fault test".into(),
    };
    let before = counts(&store, &g).await;
    for point in [
        FailPoint::AfterIdempotencyCheck,
        FailPoint::AfterDecision,
        FailPoint::BeforeCommit,
    ] {
        let result = wf.clone().with_failpoint(point).reject(&reject).await;
        assert!(
            matches!(result, Err(LedgerError::Storage(_))),
            "{point:?}: {result:?}"
        );
        let c = counts(&store, &g).await;
        assert_eq!(
            (c.decisions, c.idempotency),
            (before.decisions, before.idempotency),
            "{point:?}"
        );
    }
    let rejected = wf.reject(&reject).await.unwrap();
    assert!(!rejected.replayed);
    assert!(wf.reject(&reject).await.unwrap().replayed);

    // Genesis failure after the ref insert leaves no ref row at all.
    let g2 = graph(&store, GraphStatus::Active).await;
    let genesis = wf
        .prepare(&prepare_request(&g2, "g", None, vec![add("g")]))
        .await
        .unwrap();
    let result = wf
        .clone()
        .with_failpoint(FailPoint::AfterRefUpdate)
        .accept(&accept_request(&g2, "ga", None, &genesis.candidate))
        .await;
    assert!(matches!(result, Err(LedgerError::Storage(_))));
    assert_eq!(
        ref_row(&store, &g2).await,
        None,
        "no ref row after a failed genesis"
    );
    let ok = wf
        .accept(&accept_request(&g2, "ga", None, &genesis.candidate))
        .await
        .unwrap();
    assert_eq!(
        ref_row(&store, &g2).await,
        Some((genesis.candidate.clone(), ok.ref_version))
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn rejection_and_supersession_are_recorded_without_moving_the_ref() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    let p1 = wf
        .prepare(&prepare_request(&g, "p1", None, vec![add("a")]))
        .await
        .unwrap();
    let p1_rival = wf
        .prepare(&prepare_request(&g, "p1r", None, vec![add("r")]))
        .await
        .unwrap();
    wf.accept(&accept_request(&g, "a1", None, &p1.candidate))
        .await
        .unwrap();
    let candidate = wf
        .prepare(&prepare_request(
            &g,
            "p2",
            Some(p1.candidate.clone()),
            vec![add("b")],
        ))
        .await
        .unwrap();
    let before = counts(&store, &g).await;
    let request = RejectRequest {
        scope: scope(&g, "rej", b"reject p2"),
        branch: "main".into(),
        candidate: candidate.candidate.clone(),
        reason: "reviewer declined".into(),
    };
    let rejected = wf.reject(&request).await.unwrap();
    assert!(!rejected.replayed);
    let replay = wf.reject(&request).await.unwrap();
    assert_eq!(replay.decision_id, rejected.decision_id);
    assert!(replay.replayed);
    let after = counts(&store, &g).await;
    assert_eq!(
        ref_row(&store, &g).await,
        Some((p1.candidate.clone(), 1)),
        "ref did not move"
    );
    assert_eq!(
        (after.events, after.outbox),
        (before.events, before.outbox),
        "no accepted-state event"
    );
    assert_eq!(after.decisions, before.decisions + 1);
    let row = sqlx::query(
        "SELECT decision, reason, validation_ids, ref_event_id, proposal_id FROM decisions WHERE decision_id = $1",
    )
    .bind(rejected.decision_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    let decision: String = row.get("decision");
    let reason: Option<String> = row.get("reason");
    let validation_ids: Vec<String> = row.get("validation_ids");
    let ref_event_id: Option<i64> = row.get("ref_event_id");
    let proposal_id: Option<i64> = row.get("proposal_id");
    assert_eq!(
        (
            decision.as_str(),
            reason.as_deref(),
            validation_ids.len(),
            ref_event_id,
            proposal_id
        ),
        (
            "rejected",
            Some("reviewer declined"),
            0,
            None,
            Some(candidate.proposal_id)
        )
    );

    // Supersession is explicit: the rival genesis proposal's expected head (none) no
    // longer matches the ref, so it may be marked superseded; a current proposal may not.
    let superseded = wf
        .mark_superseded(
            &principal("curator"),
            &g,
            p1_rival.proposal_id,
            "another genesis won",
            None,
        )
        .await
        .unwrap();
    let row = sqlx::query("SELECT decision FROM decisions WHERE decision_id = $1")
        .bind(superseded)
        .fetch_one(store.pool())
        .await
        .unwrap();
    let decision: String = row.get("decision");
    assert_eq!(decision, "superseded");
    let current = wf
        .prepare(&prepare_request(
            &g,
            "p3",
            Some(p1.candidate.clone()),
            vec![add("c")],
        ))
        .await
        .unwrap();
    let still_current = wf
        .mark_superseded(
            &principal("curator"),
            &g,
            current.proposal_id,
            "not really",
            None,
        )
        .await;
    assert!(
        matches!(still_current, Err(LedgerError::LineageMismatch(_))),
        "{still_current:?}"
    );
    // A superseded proposal cannot be accepted or rejected any more: the terminal-decision
    // rule fires (its binding to expected_head = None would also refuse it).
    let late = wf
        .reject(&RejectRequest {
            scope: scope(&g, "late-rej", b"late"),
            branch: "main".into(),
            candidate: p1_rival.candidate.clone(),
            reason: "too late".into(),
        })
        .await;
    assert!(
        matches!(late, Err(LedgerError::LineageMismatch(ref m)) if m.contains("terminal decision")),
        "{late:?}"
    );
    // Superseding an already-accepted proposal is refused as a terminal-decision conflict.
    let accepted_again = wf
        .mark_superseded(&principal("curator"), &g, p1.proposal_id, "nope", None)
        .await;
    assert!(
        matches!(accepted_again, Err(LedgerError::LineageMismatch(ref m)) if m.contains("terminal decision")),
        "{accepted_again:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn concurrent_accept_and_reject_of_one_candidate_yield_exactly_one_decision() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    let p = wf
        .prepare(&prepare_request(&g, "p", None, vec![add("a")]))
        .await
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let accepting = PostgresLedgerStore::connect(&database_url(), V1Binding::Reject)
        .await
        .unwrap();
    let rejecting = PostgresLedgerStore::connect(&database_url(), V1Binding::Reject)
        .await
        .unwrap();
    let (b1, b2) = (Arc::clone(&barrier), Arc::clone(&barrier));
    let accept_req = accept_request(&g, "acc", None, &p.candidate);
    let reject_req = RejectRequest {
        scope: scope(&g, "rej", b"reject"),
        branch: "main".into(),
        candidate: p.candidate.clone(),
        reason: "declined".into(),
    };
    let a = tokio::spawn(async move {
        b1.wait().await;
        accepting.workflows().accept(&accept_req).await
    });
    let r = tokio::spawn(async move {
        b2.wait().await;
        rejecting.workflows().reject(&reject_req).await
    });
    let (a, r) = (a.await.unwrap(), r.await.unwrap());
    let decided = usize::from(a.is_ok()) + usize::from(r.is_ok());
    assert_eq!(
        decided, 1,
        "exactly one decision: accept={a:?} reject={r:?}"
    );
    for e in [
        a.err().map(|e| e.to_string()),
        r.err().map(|e| e.to_string()),
    ]
    .into_iter()
    .flatten()
    {
        assert!(e.contains("LINEAGE_MISMATCH"), "{e}");
    }
    let c = counts(&store, &g).await;
    assert_eq!(c.decisions, 1);
    let row = sqlx::query("SELECT count(*) AS n FROM decisions WHERE candidate_commit = $1")
        .bind(p.candidate.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    let n: i64 = row.get("n");
    assert_eq!(n, 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn repository_refuses_foreign_tenants_bad_branches_and_raw_ref_movement_on_active_graphs() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    let p = wf
        .prepare(&prepare_request(&g, "p", None, vec![add("a")]))
        .await
        .unwrap();
    // A principal of another tenant sees the graph as unknown (no status or head leaks).
    let mut foreign = prepare_request(&g, "fp", None, vec![add("f")]);
    foreign.scope.principal.tenant_id = TenantId::new("tenant-other").unwrap();
    assert!(matches!(
        wf.prepare(&foreign).await,
        Err(LedgerError::UnknownGraph(_))
    ));
    let mut foreign_accept = accept_request(&g, "fa", None, &p.candidate);
    foreign_accept.scope.principal.tenant_id = TenantId::new("tenant-other").unwrap();
    assert!(matches!(
        wf.accept(&foreign_accept).await,
        Err(LedgerError::UnknownGraph(_))
    ));
    let foreign_supersede = wf
        .mark_superseded(
            &AuthenticatedPrincipal {
                tenant_id: TenantId::new("tenant-other").unwrap(),
                ..principal("intruder")
            },
            &g,
            p.proposal_id,
            "x",
            None,
        )
        .await;
    assert!(matches!(
        foreign_supersede,
        Err(LedgerError::UnknownGraph(_))
    ));
    // Bounds are typed refusals before any work.
    let mut bad_branch = accept_request(&g, "bb", None, &p.candidate);
    bad_branch.branch = String::new();
    assert!(matches!(
        wf.accept(&bad_branch).await,
        Err(LedgerError::InvalidIdentifier {
            field: "branch",
            ..
        })
    ));
    let mut long_branch = prepare_request(&g, "lb", None, vec![add("z")]);
    long_branch.branch = "x".repeat(129);
    assert!(matches!(
        wf.prepare(&long_branch).await,
        Err(LedgerError::InvalidIdentifier {
            field: "branch",
            ..
        })
    ));
    let mut bad_key = accept_request(&g, "", None, &p.candidate);
    bad_key.scope.idempotency_key = String::new();
    assert!(matches!(
        wf.accept(&bad_key).await,
        Err(LedgerError::InvalidIdentifier {
            field: "idempotency_key",
            ..
        })
    ));
    let long_reason = RejectRequest {
        scope: scope(&g, "lr", b"lr"),
        branch: "main".into(),
        candidate: p.candidate.clone(),
        reason: "r".repeat(4097),
    };
    assert!(matches!(
        wf.reject(&long_reason).await,
        Err(LedgerError::InvalidIdentifier {
            field: "reason",
            ..
        })
    ));
    let c = counts(&store, &g).await;
    assert_eq!(
        (c.events, c.decisions, c.idempotency),
        (0, 0, 1),
        "only the prepare left a result"
    );
    // The raw ref primitive cannot move an active graph's ref at all.
    let raw = PgRefStore::with_ref_migrated(store.pool().clone(), g.as_str(), "main");
    assert!(matches!(
        raw.compare_and_set(None, &p.candidate).await,
        Err(LedgerError::InvalidCommit(_))
    ));
    assert_eq!(ref_row(&store, &g).await, None);
    // The schema refuses a protection flip and a ref born at a version other than 1.
    wf.accept(&accept_request(&g, "acc", None, &p.candidate))
        .await
        .unwrap();
    let flip =
        sqlx::query("UPDATE refs SET protected = false WHERE graph_id = $1 AND branch = 'main'")
            .bind(g.to_string())
            .execute(store.pool())
            .await;
    assert!(
        matches!(flip, Err(sqlx::Error::Database(ref e)) if e.code().as_deref() == Some("23000")),
        "{flip:?}"
    );
    let born_old = sqlx::query(
        "INSERT INTO refs (graph_id, branch, head, version) VALUES ($1, 'other', $2, 5)",
    )
    .bind(g.to_string())
    .bind(p.candidate.to_string())
    .execute(store.pool())
    .await;
    assert!(
        matches!(born_old, Err(sqlx::Error::Database(ref e)) if e.code().as_deref() == Some("23000")),
        "{born_old:?}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn database_level_invariants_hold_independently_of_repository_code() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    let p1 = wf
        .prepare(&prepare_request(&g, "p1", None, vec![add("a")]))
        .await
        .unwrap();
    let a1 = wf
        .accept(&accept_request(&g, "a1", None, &p1.candidate))
        .await
        .unwrap();
    let pool = store.pool();
    let sqlstate = |r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>| match r {
        Err(sqlx::Error::Database(e)) => e.code().map(|c| c.to_string()).unwrap_or_default(),
        other => panic!("expected a database refusal, got {other:?}"),
    };

    // refs composite FK: an unknown head, and a head from another graph, are refused.
    let unknown = sqlx::query("INSERT INTO refs (graph_id, branch, head) VALUES ($1, 'x', $2)")
        .bind(g.to_string())
        .bind(ContentId::for_bytes(b"nope").to_string())
        .execute(pool)
        .await;
    assert_eq!(sqlstate(unknown), "23503");
    let other = graph(&store, GraphStatus::Active).await;
    let foreign = wf
        .prepare(&prepare_request(&other, "fp", None, vec![add("f")]))
        .await
        .unwrap();
    let cross = sqlx::query("INSERT INTO refs (graph_id, branch, head) VALUES ($1, 'x', $2)")
        .bind(g.to_string())
        .bind(foreign.candidate.to_string())
        .execute(pool)
        .await;
    assert_eq!(sqlstate(cross), "23503");

    // version: must bump by exactly one with the head, and only with the head.
    let p2 = wf
        .prepare(&prepare_request(
            &g,
            "p2",
            Some(p1.candidate.clone()),
            vec![add("b")],
        ))
        .await
        .unwrap();
    for (sql, why) in [
        (
            "UPDATE refs SET head = $2 WHERE graph_id = $1 AND branch = 'main'",
            "no bump",
        ),
        (
            "UPDATE refs SET head = $2, version = version + 2 WHERE graph_id = $1 AND branch = 'main'",
            "bump by two",
        ),
        (
            "UPDATE refs SET version = version + 1 WHERE graph_id = $1 AND branch = 'main' AND head <> $2",
            "bump without head",
        ),
    ] {
        let r = sqlx::query(sql)
            .bind(g.to_string())
            .bind(p2.candidate.to_string())
            .execute(pool)
            .await;
        assert_eq!(sqlstate(r), "23000", "{why}");
    }

    // Audit tables are immutable; the outbox keeps its identity but delivery may change.
    for sql in [
        "UPDATE ref_events SET reason = 'x' WHERE event_id = $1",
        "DELETE FROM ref_events WHERE event_id = $1",
    ] {
        let r = sqlx::query(sql).bind(a1.ref_event_id).execute(pool).await;
        assert_eq!(sqlstate(r), "23000", "{sql}");
    }
    for sql in [
        "UPDATE decisions SET decision = 'rejected' WHERE decision_id = $1",
        "DELETE FROM decisions WHERE decision_id = $1",
    ] {
        let r = sqlx::query(sql).bind(a1.decision_id).execute(pool).await;
        assert_eq!(sqlstate(r), "23000", "{sql}");
    }
    let r = sqlx::query("UPDATE projection_outbox SET ref_version = 99 WHERE outbox_id = $1")
        .bind(a1.outbox_id)
        .execute(pool)
        .await;
    assert_eq!(sqlstate(r), "23000");
    sqlx::query(
        "UPDATE projection_outbox SET delivered_at = now(), attempts = 1 WHERE outbox_id = $1",
    )
    .bind(a1.outbox_id)
    .execute(pool)
    .await
    .unwrap();
    let r = sqlx::query("DELETE FROM projection_outbox WHERE outbox_id = $1")
        .bind(a1.outbox_id)
        .execute(pool)
        .await;
    assert_eq!(sqlstate(r), "23000");
    // An outbox row cannot reference a ref version that never happened.
    let r = sqlx::query(
        "INSERT INTO projection_outbox (graph_id, branch, commit_id, ref_version, event_kind, ref_event_id) \
         VALUES ($1, 'main', $2, 42, 'ref_advanced', $3)",
    )
    .bind(g.to_string())
    .bind(p1.candidate.to_string())
    .bind(a1.ref_event_id)
    .execute(pool)
    .await;
    // Both the one-outbox-row-per-event uniqueness and the version FK forbid this; which
    // one PostgreSQL reports first is an implementation detail.
    let code = sqlstate(r);
    assert!(code == "23503" || code == "23505", "{code}");
    // A nonexistent event id with a version that never happened is refused by the FKs
    // onto ref_events (either the event or the version FK reports first).
    let r = sqlx::query(
        "INSERT INTO projection_outbox (graph_id, branch, commit_id, ref_version, event_kind, ref_event_id) \
         VALUES ($1, 'main', $2, 42, 'ref_advanced', $3)",
    )
    .bind(g.to_string())
    .bind(p1.candidate.to_string())
    .bind(a1.ref_event_id + 1_000_000)
    .execute(pool)
    .await;
    assert_eq!(sqlstate(r), "23503");
    // Idempotency scope is unique.
    let r = sqlx::query(
        "INSERT INTO idempotency (tenant_id, principal_id, principal_type, graph_id, operation, idempotency_key, request_digest, result_kind) \
         VALUES ('tenant-wf', 'urn:sculpin:agent:curator', 'agent', $1, 'accept', 'a1', $2, 'accepted')",
    )
    .bind(g.to_string())
    .bind(ContentId::for_bytes(b"x").to_string())
    .execute(pool)
    .await;
    assert_eq!(sqlstate(r), "23505");
    let r = sqlx::query("DELETE FROM idempotency WHERE graph_id = $1")
        .bind(g.to_string())
        .execute(pool)
        .await;
    assert_eq!(sqlstate(r), "23000");
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn idempotency_is_scoped_by_the_complete_actor_and_correlation_is_recorded() {
    let store = store().await;
    let g = graph(&store, GraphStatus::Active).await;
    let wf = store.workflows();
    let base = prepare_request(&g, "shared-key", None, vec![add("a")]);
    let first = wf.prepare(&base).await.unwrap();
    // Same complete actor + same key + same request → replay.
    let again = wf.prepare(&base).await.unwrap();
    assert!(again.replayed && again.candidate == first.candidate);
    // Same principal id, different principal type → an independent namespace: the request
    // (same key, same payload) is a *new* prepare, not a replay.
    let mut as_service = base.clone();
    as_service.scope.principal.principal_type = PrincipalType::Service;
    let service = wf.prepare(&as_service).await.unwrap();
    assert!(!service.replayed);
    assert_ne!(
        service.candidate, first.candidate,
        "different actor ⇒ different candidate"
    );
    // Same principal acting on behalf of A vs B → independent namespaces.
    let mut for_a = base.clone();
    for_a.scope.principal.on_behalf_of = Some(PrincipalId::new("urn:sculpin:human:a").unwrap());
    let mut for_b = base.clone();
    for_b.scope.principal.on_behalf_of = Some(PrincipalId::new("urn:sculpin:human:b").unwrap());
    let a = wf.prepare(&for_a).await.unwrap();
    let b = wf.prepare(&for_b).await.unwrap();
    assert!(!a.replayed && !b.replayed);
    assert_ne!(a.candidate, b.candidate);
    assert!(
        wf.prepare(&for_a).await.unwrap().replayed,
        "delegated actor replays itself"
    );
    assert_eq!(wf.prepare(&for_a).await.unwrap().candidate, a.candidate);
    // Same complete actor + same key + different request → conflict.
    let mut different = base.clone();
    different.scope.request_digest = ContentId::for_bytes(b"something else");
    assert!(matches!(
        wf.prepare(&different).await,
        Err(LedgerError::IdempotencyConflict)
    ));
    let c = counts(&store, &g).await;
    assert_eq!((c.proposals, c.idempotency), (4, 4));
    let rows = sqlx::query(
        "SELECT principal_type, on_behalf_of FROM idempotency WHERE graph_id = $1 \
         AND idempotency_key = 'shared-key' ORDER BY principal_type, on_behalf_of NULLS FIRST",
    )
    .bind(g.to_string())
    .fetch_all(store.pool())
    .await
    .unwrap();
    let scopes: Vec<(String, Option<String>)> = rows
        .iter()
        .map(|r| (r.get("principal_type"), r.get("on_behalf_of")))
        .collect();
    assert_eq!(
        scopes,
        vec![
            ("agent".to_owned(), None),
            ("agent".to_owned(), Some("urn:sculpin:human:a".to_owned())),
            ("agent".to_owned(), Some("urn:sculpin:human:b".to_owned())),
            ("service".to_owned(), None),
        ]
    );
    // NULL delegation is a canonical absence: a second row for the same scope is refused.
    let dup = sqlx::query(
        "INSERT INTO idempotency (tenant_id, principal_id, principal_type, on_behalf_of, graph_id, \
         operation, idempotency_key, request_digest, result_kind) \
         VALUES ('tenant-wf', 'urn:sculpin:agent:curator', 'agent', NULL, $1, 'prepare', \
         'shared-key', $2, 'prepared')",
    )
    .bind(g.to_string())
    .bind(ContentId::for_bytes(b"x").to_string())
    .execute(store.pool())
    .await;
    assert!(
        matches!(dup, Err(sqlx::Error::Database(ref e)) if e.code().as_deref() == Some("23505")),
        "{dup:?}"
    );

    // Correlation ids land on the record the request created, and a retry with another
    // correlation id replays without changing the stored one.
    let stored_correlation = |proposal_id: i64| {
        let pool = store.pool().clone();
        async move {
            let row = sqlx::query("SELECT correlation_id FROM proposals WHERE proposal_id = $1")
                .bind(proposal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
            let corr: Option<String> = row.get("correlation_id");
            corr
        }
    };
    assert_eq!(
        stored_correlation(first.proposal_id).await.as_deref(),
        Some("corr-shared-key")
    );
    let mut retry = base.clone();
    retry.scope.correlation_id = Some("corr-retry".into());
    assert!(wf.prepare(&retry).await.unwrap().replayed);
    assert_eq!(
        stored_correlation(first.proposal_id).await.as_deref(),
        Some("corr-shared-key"),
        "creator's correlation retained"
    );
    let accepted = wf
        .accept(&accept_request(&g, "acc", None, &first.candidate))
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT e.correlation_id AS ec, d.correlation_id AS dc FROM ref_events e \
         JOIN decisions d ON d.ref_event_id = e.event_id WHERE e.event_id = $1",
    )
    .bind(accepted.ref_event_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    let (ec, dc): (Option<String>, Option<String>) = (row.get("ec"), row.get("dc"));
    assert_eq!(
        (ec.as_deref(), dc.as_deref()),
        (Some("corr-acc"), Some("corr-acc"))
    );

    // Tenant integrity is a schema fact: an audit row whose graph belongs to another
    // tenant is refused by PostgreSQL itself.
    let cross = sqlx::query(
        "INSERT INTO idempotency (tenant_id, principal_id, principal_type, on_behalf_of, graph_id, \
         operation, idempotency_key, request_digest, result_kind) \
         VALUES ('tenant-other', 'urn:sculpin:agent:x', 'agent', NULL, $1, 'prepare', 'k', $2, 'prepared')",
    )
    .bind(g.to_string())
    .bind(ContentId::for_bytes(b"x").to_string())
    .execute(store.pool())
    .await;
    assert!(
        matches!(cross, Err(sqlx::Error::Database(ref e)) if e.code().as_deref() == Some("23503")
            && e.constraint() == Some("idempotency_graph_tenant_fk")),
        "{cross:?}"
    );
    let cross_decision = sqlx::query(
        "INSERT INTO decisions (proposal_id, graph_id, branch, candidate_commit, decision, tenant_id, \
         principal_id, principal_type, reason, validation_ids) \
         VALUES (NULL, $1, 'main', $2, 'rejected', 'tenant-other', 'urn:sculpin:agent:x', 'agent', 'r', '{}')",
    )
    .bind(g.to_string())
    .bind(service.candidate.to_string())
    .execute(store.pool())
    .await;
    assert!(
        matches!(cross_decision, Err(sqlx::Error::Database(ref e)) if e.code().as_deref() == Some("23503")
            && e.constraint() == Some("decisions_graph_tenant_fk")),
        "{cross_decision:?}"
    );
}
