//! Real-PostgreSQL evidence for named branches (ADR-0022; Plan 0008): creation from the
//! source head and from reachable history (unreachable, foreign and unknown points fail
//! closed), tombstone deletion and restore with head/version preserved, idempotency of every
//! lifecycle operation, policy enforcement inside the acceptance transaction, `main`
//! protection, the database guards behind all of it (including raw SQL racing an
//! uncommitted tombstone), and the lifecycle races (accept vs delete, restore vs accept,
//! delete vs prepare) forced to interleave in both orders under real row locks. Every test is
//! `#[ignore]` and runs through the PostgreSQL suites with `LEDGER_TEST_DATABASE_URL`.
#![cfg(feature = "postgres")]

use ledger_core::{
    AuthenticatedPrincipal, CommitId, ContentId, GraphId, LedgerError, PrincipalId, PrincipalType,
    TenantId,
};
use ledger_rdf::{Operation, OperationKind, Patch};
use ledger_store::{
    AcceptRequest, BranchLifecycleRequest, BranchPolicy, CreateBranchRequest, GraphStatus,
    NewGraph, PostgresLedgerStore, PrepareRequest, RejectRequest, RequestScope, TraversalLimits,
    V1Binding, ValidationPolicy,
};
use std::{
    sync::Arc,
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
    format!("{prefix}-{}-{nanos}", std::process::id())
}

async fn store() -> PostgresLedgerStore {
    PostgresLedgerStore::connect_and_migrate(&database_url(), V1Binding::Reject)
        .await
        .unwrap()
}

async fn graph(store: &PostgresLedgerStore, tenant: &str) -> GraphId {
    let id = GraphId::new(unique("br")).unwrap();
    store
        .graphs()
        .create(&NewGraph {
            graph_id: id.clone(),
            tenant_id: TenantId::new(tenant).unwrap(),
            knowledge_base_id: Some(unique("kb")),
            purpose: None,
            status: GraphStatus::Active,
        })
        .await
        .unwrap();
    id
}

fn actor(tenant: &str, principal: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal {
        principal_id: PrincipalId::new(format!("urn:it:{principal}")).unwrap(),
        principal_type: PrincipalType::Agent,
        tenant_id: TenantId::new(tenant).unwrap(),
        on_behalf_of: None,
    }
}

fn scope_as(
    principal: AuthenticatedPrincipal,
    graph: &GraphId,
    key: &str,
    digest: &str,
) -> RequestScope {
    RequestScope {
        principal,
        graph: graph.clone(),
        idempotency_key: key.to_owned(),
        request_digest: ContentId::for_bytes(digest.as_bytes()),
        correlation_id: None,
    }
}

fn scope(graph: &GraphId, key: &str) -> RequestScope {
    scope_as(actor("tenant-a", "curator"), graph, key, key)
}

fn limits() -> TraversalLimits {
    TraversalLimits::DEFAULT
}

/// Prepare and accept one quad on `branch` from `head`; returns the new head.
async fn accept_on(
    store: &PostgresLedgerStore,
    graph: &GraphId,
    branch: &str,
    head: Option<CommitId>,
    value: &str,
) -> Result<CommitId, LedgerError> {
    let candidate = prepare_on(store, graph, branch, head.clone(), value).await?;
    accept_candidate(
        store,
        graph,
        branch,
        head,
        &candidate,
        &format!("a-{value}"),
    )
    .await?;
    Ok(candidate)
}

async fn prepare_on(
    store: &PostgresLedgerStore,
    graph: &GraphId,
    branch: &str,
    head: Option<CommitId>,
    value: &str,
) -> Result<CommitId, LedgerError> {
    let prepared = store
        .workflows()
        .prepare(&PrepareRequest {
            scope: scope(graph, &format!("p-{value}")),
            branch: branch.into(),
            expected_head: head,
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
        .await?;
    Ok(prepared.candidate)
}

async fn accept_candidate(
    store: &PostgresLedgerStore,
    graph: &GraphId,
    branch: &str,
    head: Option<CommitId>,
    candidate: &CommitId,
    key: &str,
) -> Result<(), LedgerError> {
    store
        .workflows()
        .accept(&AcceptRequest {
            scope: scope(graph, key),
            branch: branch.into(),
            expected_head: head,
            candidate: candidate.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await
        .map(|_| ())
}

fn create(
    graph: &GraphId,
    key: &str,
    name: &str,
    source: &str,
    from: Option<&CommitId>,
) -> CreateBranchRequest {
    CreateBranchRequest {
        scope: scope(graph, key),
        name: name.into(),
        source: source.into(),
        from_commit: from.cloned(),
        policy: BranchPolicy::default(),
    }
}

fn lifecycle(graph: &GraphId, key: &str, name: &str) -> BranchLifecycleRequest {
    BranchLifecycleRequest {
        scope: scope(graph, key),
        name: name.into(),
        reason: Some(format!("{key} reason")),
    }
}

/// `main` with a linear history C1 → … → Cn; returns the commits in order.
async fn main_history(store: &PostgresLedgerStore, graph: &GraphId, n: usize) -> Vec<CommitId> {
    let mut commits = Vec::new();
    let mut head = None;
    for i in 1..=n {
        let c = accept_on(
            store,
            graph,
            "main",
            head.clone(),
            &format!("{graph}-main-{i}"),
        )
        .await
        .unwrap();
        head = Some(c.clone());
        commits.push(c);
    }
    commits
}

fn tenant() -> TenantId {
    TenantId::new("tenant-a").unwrap()
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn branches_are_created_from_the_head_or_reachable_history_and_never_elsewhere() {
    let store = store().await;
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 5).await;
    let repo = store.workflows();
    // main was born by genesis and is a protected branch.
    let main = repo.branch(&tenant(), &g, "main").await.unwrap().unwrap();
    assert_eq!(
        (
            main.origin.as_str(),
            main.status.as_str(),
            main.policy.protected,
            main.version
        ),
        ("genesis", "active", true, 5)
    );
    // branch-A from the head, branch-B from historical C2: O(1), version 1, no data copied.
    let a = repo
        .create_branch(&create(&g, "ca", "branch-A", "main", None), limits())
        .await
        .unwrap();
    assert_eq!(
        (a.event.head.clone(), a.event.ref_version, a.replayed),
        (c[4].clone(), 1, false)
    );
    let b = repo
        .create_branch(
            &create(&g, "cb", "agent/task-17", "main", Some(&c[1])),
            limits(),
        )
        .await
        .unwrap();
    assert_eq!(b.event.head, c[1]);
    let info = repo
        .branch(&tenant(), &g, "agent/task-17")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            info.origin.as_str(),
            info.source_branch.as_deref(),
            info.source_commit.as_ref(),
            info.version,
            info.policy.protected
        ),
        ("created", Some("main"), Some(&c[1]), 1, false)
    );
    // main did not move.
    assert_eq!(
        store.ref_head(&g, "main").await.unwrap(),
        Some((c[4].clone(), 5))
    );
    // From a branch as the source, back into its own history.
    let from_branch = repo
        .create_branch(
            &create(&g, "cc", "branch-C", "agent/task-17", Some(&c[0])),
            limits(),
        )
        .await
        .unwrap();
    assert_eq!(from_branch.event.head, c[0]);
    // An unreachable commit of the same graph (a commit of branch-A beyond C5 is not reachable
    // from agent/task-17 at C2), a foreign graph's commit, and an unknown commit: one error.
    let beyond = accept_on(
        &store,
        &g,
        "branch-A",
        Some(c[4].clone()),
        &format!("{g}-a-1"),
    )
    .await
    .unwrap();
    let other = graph(&store, "tenant-a").await;
    let foreign = main_history(&store, &other, 1).await;
    let unknown = CommitId(ContentId::for_bytes(b"no such commit"));
    for (key, point) in [("u1", &beyond), ("u2", &foreign[0]), ("u3", &unknown)] {
        let e = repo
            .create_branch(
                &create(&g, key, &format!("bad-{key}"), "agent/task-17", Some(point)),
                limits(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(e, LedgerError::BranchPointUnreachable),
            "{key}: {e}"
        );
    }
    // Nothing was created for the failed attempts.
    for key in ["u1", "u2", "u3"] {
        assert!(
            repo.branch(&tenant(), &g, &format!("bad-{key}"))
                .await
                .unwrap()
                .is_none()
        );
    }
    // A bounded search: the visit limit is enforced.
    let e = repo
        .create_branch(
            &create(&g, "lim", "limited", "main", Some(&c[0])),
            TraversalLimits {
                max_visited: 2,
                deadline: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(e, LedgerError::ResourceLimit(_)), "{e}");
    // Names are unique forever; `main` is never created; an unknown source is not found.
    assert!(matches!(
        repo.create_branch(&create(&g, "dup", "branch-A", "main", None), limits())
            .await
            .unwrap_err(),
        LedgerError::BranchExists(_)
    ));
    assert!(matches!(
        repo.create_branch(&create(&g, "m", "main", "branch-A", None), limits())
            .await
            .unwrap_err(),
        LedgerError::BranchPolicyViolation(_)
    ));
    assert!(matches!(
        repo.create_branch(&create(&g, "ns", "x", "no-such-branch", None), limits())
            .await
            .unwrap_err(),
        LedgerError::BranchNotFound(_)
    ));
    // Another tenant cannot see the graph at all (non-disclosing).
    let foreign_scope = CreateBranchRequest {
        scope: scope_as(actor("tenant-b", "intruder"), &g, "fx", "fx"),
        ..create(&g, "fx", "stolen", "main", None)
    };
    assert!(matches!(
        repo.create_branch(&foreign_scope, limits())
            .await
            .unwrap_err(),
        LedgerError::UnknownGraph(_)
    ));
    assert!(matches!(
        repo.branch(&TenantId::new("tenant-b").unwrap(), &g, "main")
            .await
            .unwrap_err(),
        LedgerError::UnknownGraph(_)
    ));
    // Genesis only for main: an unknown non-main branch cannot be proposed onto.
    assert!(matches!(
        prepare_on(&store, &g, "orphan", None, &format!("{g}-orphan"))
            .await
            .unwrap_err(),
        LedgerError::BranchNotFound(_)
    ));
    // The ledger's own verifier is clean.
    let report = ledger_store::verify::run(store.pool()).await.unwrap();
    assert!(
        report.is_clean(),
        "{:?}",
        report
            .checks
            .iter()
            .filter(|c| c.violations > 0)
            .map(|c| (c.name, &c.sample))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_multi_step_cognitive_workflow_runs_on_a_branch_while_main_stays_put() {
    let store = store().await;
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 1).await;
    let c100 = c[0].clone();
    let repo = store.workflows();
    repo.create_branch(&create(&g, "t17", "agent/task-17", "main", None), limits())
        .await
        .unwrap();
    let mut head = c100.clone();
    let mut heads = Vec::new();
    for step in 1..=3 {
        head = accept_on(
            &store,
            &g,
            "agent/task-17",
            Some(head.clone()),
            &format!("{g}-t17-{step}"),
        )
        .await
        .unwrap();
        heads.push(head.clone());
    }
    assert_eq!(
        store.ref_head(&g, "main").await.unwrap(),
        Some((c100.clone(), 1))
    );
    assert_eq!(
        store.ref_head(&g, "agent/task-17").await.unwrap(),
        Some((heads[2].clone(), 4))
    );
    // Deterministic historical reconstruction at every step.
    for (i, h) in heads.iter().enumerate() {
        let state = store
            .workflows()
            .reconstruct(h, store.workflows().limits())
            .await
            .unwrap();
        assert_eq!(
            state.len(),
            i + 2,
            "C10{} holds main's quad plus {} branch quads",
            i + 1,
            i + 1
        );
        assert_eq!(
            state,
            store
                .workflows()
                .reconstruct(h, store.workflows().limits())
                .await
                .unwrap()
        );
    }
    // Lifecycle and movement histories are separate and complete.
    let (lifecycle, movements) = repo
        .branch_history(&tenant(), &g, "agent/task-17", 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lifecycle.len(), 1);
    assert_eq!(
        (
            lifecycle[0].operation.as_str(),
            lifecycle[0].source_branch.as_deref(),
            lifecycle[0].source_commit.as_ref(),
            lifecycle[0].principal_id.as_str()
        ),
        ("created", Some("main"), Some(&c100), "urn:it:curator")
    );
    assert_eq!(
        movements.iter().map(|m| m.new_version).collect::<Vec<_>>(),
        vec![4, 3, 2, 1]
    );
    assert_eq!(movements[3].operation, "genesis");
    assert_eq!(movements[3].new_head, c100);
    let log = repo
        .first_parent_history(&tenant(), &g, &heads[2], 10, limits())
        .await
        .unwrap();
    assert_eq!(
        log,
        vec![
            heads[2].clone(),
            heads[1].clone(),
            heads[0].clone(),
            c100.clone()
        ]
    );
    // Cross-branch acceptance is refused: a candidate prepared on the branch cannot be
    // accepted onto main.
    let candidate = prepare_on(
        &store,
        &g,
        "agent/task-17",
        Some(heads[2].clone()),
        &format!("{g}-t17-x"),
    )
    .await
    .unwrap();
    let e = accept_candidate(&store, &g, "main", Some(c100.clone()), &candidate, "cross")
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            LedgerError::LineageMismatch(_) | LedgerError::HeadChanged { .. }
        ),
        "{e}"
    );
    assert_eq!(store.ref_head(&g, "main").await.unwrap(), Some((c100, 1)));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn deletion_is_a_tombstone_and_restore_keeps_head_and_version() {
    let store = store().await;
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 1).await;
    let repo = store.workflows();
    repo.create_branch(&create(&g, "c", "work", "main", None), limits())
        .await
        .unwrap();
    let h1 = accept_on(&store, &g, "work", Some(c[0].clone()), &format!("{g}-w1"))
        .await
        .unwrap();
    // A proposal prepared before deletion.
    let pending = prepare_on(&store, &g, "work", Some(h1.clone()), &format!("{g}-w2"))
        .await
        .unwrap();
    let deleted = repo
        .delete_branch(&lifecycle(&g, "d1", "work"))
        .await
        .unwrap();
    assert_eq!(
        (
            deleted.event.operation.as_str(),
            deleted.event.status_after.as_str(),
            deleted.event.lifecycle_version,
            deleted.event.head.clone(),
            deleted.event.ref_version
        ),
        ("deleted", "deleted", 2, h1.clone(), 2)
    );
    // Reads keep working; writes are refused.
    let info = repo.branch(&tenant(), &g, "work").await.unwrap().unwrap();
    assert_eq!(
        (info.status.as_str(), info.head.clone(), info.version),
        ("deleted", h1.clone(), 2)
    );
    assert_eq!(
        store
            .workflows()
            .reconstruct(&h1, store.workflows().limits())
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(matches!(
        prepare_on(&store, &g, "work", Some(h1.clone()), &format!("{g}-w3"))
            .await
            .unwrap_err(),
        LedgerError::BranchDeleted(_)
    ));
    assert!(matches!(
        accept_candidate(
            &store,
            &g,
            "work",
            Some(h1.clone()),
            &pending,
            "acc-deleted"
        )
        .await
        .unwrap_err(),
        LedgerError::BranchDeleted(_)
    ));
    // Idempotency: the same key replays; a new key conflicts on state; another request under
    // the same key is an idempotency conflict.
    let again = repo
        .delete_branch(&lifecycle(&g, "d1", "work"))
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!(again.event, deleted.event);
    assert!(matches!(
        repo.delete_branch(&lifecycle(&g, "d2", "work"))
            .await
            .unwrap_err(),
        LedgerError::BranchStateConflict(_)
    ));
    let mut different = lifecycle(&g, "d1", "work");
    different.scope.request_digest = ContentId::for_bytes(b"another request");
    assert!(matches!(
        repo.delete_branch(&different).await.unwrap_err(),
        LedgerError::IdempotencyConflict
    ));
    // Restore: same head and version; the old proposal is accepted only on ordinary rules
    // (its expected head is still the head, so it goes through).
    let restored = repo
        .restore_branch(&lifecycle(&g, "r1", "work"))
        .await
        .unwrap();
    assert_eq!(
        (
            restored.event.lifecycle_version,
            restored.event.head.clone(),
            restored.event.ref_version
        ),
        (3, h1.clone(), 2)
    );
    assert!(
        repo.restore_branch(&lifecycle(&g, "r1", "work"))
            .await
            .unwrap()
            .replayed
    );
    assert!(matches!(
        repo.restore_branch(&lifecycle(&g, "r2", "work"))
            .await
            .unwrap_err(),
        LedgerError::BranchStateConflict(_)
    ));
    accept_candidate(
        &store,
        &g,
        "work",
        Some(h1.clone()),
        &pending,
        "acc-restored",
    )
    .await
    .unwrap();
    assert_eq!(
        store.ref_head(&g, "work").await.unwrap(),
        Some((pending.clone(), 3))
    );
    // main is never deleted or restored.
    assert!(matches!(
        repo.delete_branch(&lifecycle(&g, "dm", "main"))
            .await
            .unwrap_err(),
        LedgerError::BranchPolicyViolation(_)
    ));
    let (events, _) = repo
        .branch_history(&tenant(), &g, "work", 10)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        events
            .iter()
            .map(|e| e.operation.as_str())
            .collect::<Vec<_>>(),
        vec!["created", "deleted", "restored"]
    );
    assert_eq!(events[1].reason.as_deref(), Some("d1 reason"));
    let report = ledger_store::verify::run(store.pool()).await.unwrap();
    assert!(
        report.is_clean(),
        "{:?}",
        report
            .checks
            .iter()
            .filter(|c| c.violations > 0)
            .map(|c| (c.name, &c.sample))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_pending_proposal_on_a_deleted_branch_can_still_be_rejected() {
    let store = store().await;
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 1).await;
    let repo = store.workflows();
    repo.create_branch(&create(&g, "c", "work", "main", None), limits())
        .await
        .unwrap();
    let pending = prepare_on(&store, &g, "work", Some(c[0].clone()), &format!("{g}-r1"))
        .await
        .unwrap();
    repo.delete_branch(&lifecycle(&g, "d", "work"))
        .await
        .unwrap();
    repo.reject(&RejectRequest {
        scope: scope(&g, "rej"),
        branch: "work".into(),
        candidate: pending,
        reason: "closed with the deleted branch".into(),
        validation_id: None,
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn branch_policy_tightens_acceptance_inside_the_transaction() {
    let store = store().await;
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 1).await;
    let repo = store.workflows();
    let strict = CreateBranchRequest {
        policy: BranchPolicy {
            protected: false,
            require_validation: true,
            require_distinct_reviewer: false,
        },
        ..create(&g, "s", "strict", "main", None)
    };
    repo.create_branch(&strict, limits()).await.unwrap();
    let candidate = prepare_on(&store, &g, "strict", Some(c[0].clone()), &format!("{g}-s1"))
        .await
        .unwrap();
    assert!(matches!(
        accept_candidate(
            &store,
            &g,
            "strict",
            Some(c[0].clone()),
            &candidate,
            "s-acc"
        )
        .await
        .unwrap_err(),
        LedgerError::ValidationRequired
    ));
    let review = CreateBranchRequest {
        policy: BranchPolicy {
            protected: false,
            require_validation: false,
            require_distinct_reviewer: true,
        },
        ..create(&g, "rv", "reviewed", "main", None)
    };
    repo.create_branch(&review, limits()).await.unwrap();
    let candidate = prepare_on(
        &store,
        &g,
        "reviewed",
        Some(c[0].clone()),
        &format!("{g}-rv1"),
    )
    .await
    .unwrap();
    // The proposer cannot accept their own proposal…
    assert!(matches!(
        accept_candidate(
            &store,
            &g,
            "reviewed",
            Some(c[0].clone()),
            &candidate,
            "rv-self"
        )
        .await
        .unwrap_err(),
        LedgerError::BranchPolicyViolation(_)
    ));
    // …another reviewer can.
    store
        .workflows()
        .accept(&AcceptRequest {
            scope: scope_as(actor("tenant-a", "reviewer"), &g, "rv-other", "rv-other"),
            branch: "reviewed".into(),
            expected_head: Some(c[0].clone()),
            candidate,
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await
        .unwrap();
    // Protection is recorded on the ref and selects the strict delta policy.
    let protected = CreateBranchRequest {
        policy: BranchPolicy {
            protected: true,
            require_validation: false,
            require_distinct_reviewer: false,
        },
        ..create(&g, "p", "release", "main", None)
    };
    repo.create_branch(&protected, limits()).await.unwrap();
    assert!(
        repo.branch(&tenant(), &g, "release")
            .await
            .unwrap()
            .unwrap()
            .policy
            .protected
    );
    let work = repo.branch(&tenant(), &g, "strict").await.unwrap().unwrap();
    assert_eq!(work.policy, strict.policy);
}

/// Unforced accept-vs-delete rounds (either outcome valid, checked each round); the forced
/// interleavings of every lifecycle pair are in
/// `lifecycle_races_are_forced_to_interleave_in_both_orders`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn lifecycle_races_have_exactly_one_valid_outcome() {
    let store = Arc::new(store().await);
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 1).await;
    for round in 0..12 {
        let name = format!("race-{round}");
        store
            .workflows()
            .create_branch(
                &create(&g, &format!("c{round}"), &name, "main", None),
                limits(),
            )
            .await
            .unwrap();
        let candidate = prepare_on(
            &store,
            &g,
            &name,
            Some(c[0].clone()),
            &format!("{g}-race-{round}"),
        )
        .await
        .unwrap();
        // accept vs delete.
        let (s1, s2, g1, g2, n1, n2, cand) = (
            store.clone(),
            store.clone(),
            g.clone(),
            g.clone(),
            name.clone(),
            name.clone(),
            candidate.clone(),
        );
        let head = c[0].clone();
        let accept = tokio::spawn(async move {
            accept_candidate(&s1, &g1, &n1, Some(head), &cand, &format!("ra-{n1}")).await
        });
        let delete = tokio::spawn(async move {
            s2.workflows()
                .delete_branch(&lifecycle(&g2, &format!("rd-{n2}"), &n2))
                .await
        });
        let (accepted, deleted) = (accept.await.unwrap(), delete.await.unwrap());
        let deleted = deleted.expect("delete always succeeds: the branch was active");
        let (head, version) = store.ref_head(&g, &name).await.unwrap().unwrap();
        match accepted {
            // Accepted first: the delete saw the moved head.
            Ok(()) => {
                assert_eq!(
                    (head.clone(), version),
                    (candidate.clone(), 2),
                    "round {round}"
                );
                assert_eq!(
                    (deleted.event.head.clone(), deleted.event.ref_version),
                    (candidate.clone(), 2)
                );
            }
            // Deleted first: the acceptance was refused and nothing moved.
            Err(LedgerError::BranchDeleted(_)) => {
                assert_eq!((head.clone(), version), (c[0].clone(), 1), "round {round}");
                assert_eq!(
                    (deleted.event.head.clone(), deleted.event.ref_version),
                    (c[0].clone(), 1)
                );
            }
            Err(e) => panic!("round {round}: unexpected accept outcome {e}"),
        }
        // Sequential: no prepare on the tombstone; restore; prepare again.
        let fresh = prepare_on(&store, &g, &name, None, "unused").await;
        assert!(matches!(fresh, Err(LedgerError::BranchDeleted(_))));
        let (s3, g3, n3) = (store.clone(), g.clone(), name.clone());
        let restore = tokio::spawn(async move {
            s3.workflows()
                .restore_branch(&lifecycle(&g3, &format!("rr-{n3}"), &n3))
                .await
        });
        restore.await.unwrap().unwrap();
        let next = prepare_on(
            &store,
            &g,
            &name,
            Some(head.clone()),
            &format!("{g}-race-next-{round}"),
        )
        .await
        .unwrap();
        // A second accept vs delete round on the restored branch: exactly one of "accepted on
        // an active branch" or "refused as deleted".
        let (s4, s5, g4, g5, n4, n5, h4) = (
            store.clone(),
            store.clone(),
            g.clone(),
            g.clone(),
            name.clone(),
            name.clone(),
            head.clone(),
        );
        let next_c = next.clone();
        let accept = tokio::spawn(async move {
            accept_candidate(&s4, &g4, &n4, Some(h4), &next_c, &format!("ra2-{n4}")).await
        });
        let delete = tokio::spawn(async move {
            s5.workflows()
                .delete_branch(&lifecycle(&g5, &format!("rd2-{n5}"), &n5))
                .await
        });
        let (accepted, deleted) = (accept.await.unwrap(), delete.await.unwrap().unwrap());
        let (head_after, _) = store.ref_head(&g, &name).await.unwrap().unwrap();
        match accepted {
            Ok(()) => assert_eq!(deleted.event.head, next, "round {round}"),
            Err(LedgerError::BranchDeleted(_)) => assert_eq!(head_after, head, "round {round}"),
            Err(e) => panic!("round {round}: unexpected accept outcome {e}"),
        }
        assert_eq!(deleted.event.head, head_after);
    }
    let report = ledger_store::verify::run(store.pool()).await.unwrap();
    assert!(
        report.is_clean(),
        "{:?}",
        report
            .checks
            .iter()
            .filter(|c| c.violations > 0)
            .map(|c| (c.name, &c.sample))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn concurrent_creators_of_one_name_produce_exactly_one_branch() {
    let store = Arc::new(store().await);
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 3).await;
    let mut tasks = Vec::new();
    for i in 0..8 {
        let (s, g, point) = (store.clone(), g.clone(), c[i % 3].clone());
        tasks.push(tokio::spawn(async move {
            s.workflows()
                .create_branch(
                    &create(&g, &format!("k{i}"), "contested", "main", Some(&point)),
                    limits(),
                )
                .await
        }));
    }
    let mut won = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => won += 1,
            Err(LedgerError::BranchExists(_)) => {}
            Err(e) => panic!("unexpected {e}"),
        }
    }
    assert_eq!(won, 1);
    let (events, movements) = store
        .workflows()
        .branch_history(&tenant(), &g, "contested", 10)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((events.len(), movements.len()), (1, 1));
    assert_eq!(events[0].head, movements[0].new_head);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn the_database_enforces_the_branch_rules_even_for_the_owner() {
    let store = store().await;
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 1).await;
    store
        .workflows()
        .create_branch(&create(&g, "c", "work", "main", None), limits())
        .await
        .unwrap();
    store
        .workflows()
        .delete_branch(&lifecycle(&g, "d", "work"))
        .await
        .unwrap();
    let pool = store.pool();
    let refused = |sql: String| async move {
        let r = sqlx::query(&sql).execute(pool).await;
        match r {
            Err(sqlx::Error::Database(d)) => d.code().unwrap_or_default().into_owned(),
            Err(e) => panic!("{sql}: {e}"),
            Ok(_) => panic!("{sql} must be refused"),
        }
    };
    let (gs, c0) = (g.as_str().to_owned(), c[0].to_string());
    // Moving a deleted branch without an audit row is refused (0009's audit here; 0012's
    // deleted-branch guard is exercised with a genuine audited move in
    // `a_deleted_branch_head_never_moves_even_by_raw_sql_racing_the_delete`).
    let code = refused(format!(
        "UPDATE refs SET head = '{c0}', version = version + 1 WHERE graph_id = '{gs}' AND branch = 'work'"
    ))
    .await;
    assert!(code == "23000", "{code}");
    // Branches are never deleted, policy is immutable, status never changes without its event.
    assert_eq!(
        refused(format!(
            "DELETE FROM branches WHERE graph_id = '{gs}' AND branch = 'work'"
        ))
        .await,
        "23000"
    );
    assert_eq!(refused(format!("UPDATE branches SET require_validation = true WHERE graph_id = '{gs}' AND branch = 'work'")).await, "23000");
    assert_eq!(refused(format!("UPDATE branches SET status = 'active', lifecycle_version = lifecycle_version + 1 WHERE graph_id = '{gs}' AND branch = 'work'")).await, "23000");
    assert_eq!(
        refused(format!(
            "UPDATE branches SET status = 'active' WHERE graph_id = '{gs}' AND branch = 'work'"
        ))
        .await,
        "23000"
    );
    // Lifecycle events are write-once and must describe a real state.
    assert_eq!(
        refused(format!(
            "UPDATE branch_events SET reason = 'rewritten' WHERE graph_id = '{gs}'"
        ))
        .await,
        "23000"
    );
    assert_eq!(
        refused(format!("DELETE FROM branch_events WHERE graph_id = '{gs}'")).await,
        "23000"
    );
    assert_eq!(refused(format!(
        "INSERT INTO branch_events (graph_id, branch, tenant_id, lifecycle_version, operation, status_after, head, ref_version, principal_id, principal_type) \
         VALUES ('{gs}', 'work', 'tenant-a', 3, 'restored', 'active', '{c0}', 1, 'urn:x', 'agent')"
    )).await, "23000");
    // main: always protected, never deleted, never 'created'.
    assert_eq!(refused(format!("UPDATE branches SET status = 'deleted', lifecycle_version = lifecycle_version + 1 WHERE graph_id = '{gs}' AND branch = 'main'")).await, "23514");
    // An unknown non-main branch of an active graph receives no proposal, even directly.
    assert_eq!(refused(format!(
        "INSERT INTO proposals (graph_id, branch, tenant_id, principal_id, principal_type, expected_head, requested_patch_id, effective_patch_id, candidate_commit) \
         VALUES ('{gs}', 'nowhere', 'tenant-a', 'urn:x', 'agent', NULL, 'sha256:{z}', 'sha256:{z}', '{c0}')",
        z = "0".repeat(64)
    )).await, "23000");
}

/// Backends currently blocked by `holder`, directly or transitively (a second waiter on a
/// row queues behind the first waiter's tuple lock, not behind the holder itself).
async fn waiting_on(pool: &sqlx::PgPool, holder: i32) -> i64 {
    sqlx::query_scalar(
        "WITH RECURSIVE w(pid) AS ( \
             SELECT pid FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)) \
             UNION SELECT a.pid FROM pg_stat_activity a JOIN w ON w.pid = ANY(pg_blocking_pids(a.pid))) \
         SELECT count(*) FROM w",
    )
    .bind(holder)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Wait until `n` backends are blocked by `holder` (panics after 20 s: the interleaving the
/// test claims never happened).
async fn until_waiting(pool: &sqlx::PgPool, holder: i32, n: i64) {
    for _ in 0..400 {
        if waiting_on(pool, holder).await >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{n} backends never queued behind {holder}");
}

/// An owner transaction holding the branch row exclusively, so the lifecycle operations
/// spawned next queue behind it in a known order (PostgreSQL grants tuple locks in queue
/// order) and genuinely overlap.
async fn hold_branch(
    pool: &sqlx::PgPool,
    graph: &GraphId,
    branch: &str,
) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
    let mut tx = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM branches WHERE graph_id = $1 AND branch = $2 FOR UPDATE")
        .bind(graph.as_str())
        .bind(branch)
        .execute(&mut *tx)
        .await
        .unwrap();
    (tx, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn lifecycle_races_are_forced_to_interleave_in_both_orders() {
    let store = Arc::new(store().await);
    let pool = store.pool().clone();
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 1).await;
    for accept_first in [true, false] {
        // accept vs delete: both queued behind the held branch row.
        let name = format!("forced-ad-{accept_first}");
        store
            .workflows()
            .create_branch(
                &create(&g, &format!("c-{name}"), &name, "main", None),
                limits(),
            )
            .await
            .unwrap();
        let cand = prepare_on(
            &store,
            &g,
            &name,
            Some(c[0].clone()),
            &format!("{g}-{name}"),
        )
        .await
        .unwrap();
        let (hold, pid) = hold_branch(&pool, &g, &name).await;
        let spawn_accept =
            |s: Arc<PostgresLedgerStore>, g: GraphId, n: String, h: CommitId, cand: CommitId| {
                tokio::spawn(async move {
                    accept_candidate(&s, &g, &n, Some(h), &cand, &format!("fa-{n}")).await
                })
            };
        let spawn_delete = |s: Arc<PostgresLedgerStore>, g: GraphId, n: String| {
            tokio::spawn(async move {
                s.workflows()
                    .delete_branch(&lifecycle(&g, &format!("fd-{n}"), &n))
                    .await
            })
        };
        let (accept, delete) = if accept_first {
            let a = spawn_accept(
                store.clone(),
                g.clone(),
                name.clone(),
                c[0].clone(),
                cand.clone(),
            );
            until_waiting(&pool, pid, 1).await;
            let d = spawn_delete(store.clone(), g.clone(), name.clone());
            until_waiting(&pool, pid, 2).await;
            (a, d)
        } else {
            let d = spawn_delete(store.clone(), g.clone(), name.clone());
            until_waiting(&pool, pid, 1).await;
            let a = spawn_accept(
                store.clone(),
                g.clone(),
                name.clone(),
                c[0].clone(),
                cand.clone(),
            );
            until_waiting(&pool, pid, 2).await;
            (a, d)
        };
        hold.commit().await.unwrap();
        let (accepted, deleted) = (accept.await.unwrap(), delete.await.unwrap().unwrap());
        let (head, version) = store.ref_head(&g, &name).await.unwrap().unwrap();
        if accept_first {
            accepted.unwrap();
            assert_eq!((head.clone(), version), (cand.clone(), 2));
        } else {
            assert!(
                matches!(accepted, Err(LedgerError::BranchDeleted(_))),
                "{accepted:?}"
            );
            assert_eq!((head.clone(), version), (c[0].clone(), 1));
        }
        assert_eq!(
            (deleted.event.head, deleted.event.ref_version),
            (head.clone(), version)
        );

        // restore vs accept of a proposal prepared before the deletion.
        let name = format!("forced-ra-{accept_first}");
        store
            .workflows()
            .create_branch(
                &create(&g, &format!("c-{name}"), &name, "main", None),
                limits(),
            )
            .await
            .unwrap();
        let cand = prepare_on(
            &store,
            &g,
            &name,
            Some(c[0].clone()),
            &format!("{g}-{name}"),
        )
        .await
        .unwrap();
        store
            .workflows()
            .delete_branch(&lifecycle(&g, &format!("d-{name}"), &name))
            .await
            .unwrap();
        let (hold, pid) = hold_branch(&pool, &g, &name).await;
        let spawn_restore = |s: Arc<PostgresLedgerStore>, g: GraphId, n: String| {
            tokio::spawn(async move {
                s.workflows()
                    .restore_branch(&lifecycle(&g, &format!("fr-{n}"), &n))
                    .await
            })
        };
        let (accept, restore) = if accept_first {
            let a = spawn_accept(
                store.clone(),
                g.clone(),
                name.clone(),
                c[0].clone(),
                cand.clone(),
            );
            until_waiting(&pool, pid, 1).await;
            let r = spawn_restore(store.clone(), g.clone(), name.clone());
            until_waiting(&pool, pid, 2).await;
            (a, r)
        } else {
            let r = spawn_restore(store.clone(), g.clone(), name.clone());
            until_waiting(&pool, pid, 1).await;
            let a = spawn_accept(
                store.clone(),
                g.clone(),
                name.clone(),
                c[0].clone(),
                cand.clone(),
            );
            until_waiting(&pool, pid, 2).await;
            (a, r)
        };
        hold.commit().await.unwrap();
        let (accepted, restored) = (accept.await.unwrap(), restore.await.unwrap().unwrap());
        assert_eq!(
            (restored.event.head.clone(), restored.event.ref_version),
            (c[0].clone(), 1)
        );
        let (head, version) = store.ref_head(&g, &name).await.unwrap().unwrap();
        if accept_first {
            // Queued ahead of the restore: it still saw the tombstone.
            assert!(
                matches!(accepted, Err(LedgerError::BranchDeleted(_))),
                "{accepted:?}"
            );
            assert_eq!((head, version), (c[0].clone(), 1));
        } else {
            accepted.unwrap();
            assert_eq!((head, version), (cand.clone(), 2));
        }

        // delete vs prepare.
        let name = format!("forced-dp-{accept_first}");
        store
            .workflows()
            .create_branch(
                &create(&g, &format!("c-{name}"), &name, "main", None),
                limits(),
            )
            .await
            .unwrap();
        let (hold, pid) = hold_branch(&pool, &g, &name).await;
        let spawn_prepare = |s: Arc<PostgresLedgerStore>, g: GraphId, n: String, h: CommitId| {
            tokio::spawn(
                async move { prepare_on(&s, &g, &n, Some(h), &format!("{g}-{n}-p")).await },
            )
        };
        let (prepare, delete) = if accept_first {
            let p = spawn_prepare(store.clone(), g.clone(), name.clone(), c[0].clone());
            until_waiting(&pool, pid, 1).await;
            let d = spawn_delete(store.clone(), g.clone(), name.clone());
            until_waiting(&pool, pid, 2).await;
            (p, d)
        } else {
            let d = spawn_delete(store.clone(), g.clone(), name.clone());
            until_waiting(&pool, pid, 1).await;
            let p = spawn_prepare(store.clone(), g.clone(), name.clone(), c[0].clone());
            until_waiting(&pool, pid, 2).await;
            (p, d)
        };
        hold.commit().await.unwrap();
        let (prepared, deleted) = (prepare.await.unwrap(), delete.await.unwrap().unwrap());
        assert_eq!(
            (deleted.event.head, deleted.event.ref_version),
            (c[0].clone(), 1)
        );
        let proposals: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM proposals WHERE graph_id = $1 AND branch = $2",
        )
        .bind(g.as_str())
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
        if accept_first {
            prepared.unwrap();
            assert_eq!(proposals, 1);
        } else {
            assert!(
                matches!(prepared, Err(LedgerError::BranchDeleted(_))),
                "{prepared:?}"
            );
            assert_eq!(proposals, 0);
        }
    }
    let report = ledger_store::verify::run(store.pool()).await.unwrap();
    assert!(
        report.is_clean(),
        "{:?}",
        report
            .checks
            .iter()
            .filter(|c| c.violations > 0)
            .map(|c| (c.name, &c.sample))
            .collect::<Vec<_>>()
    );
}

/// Move `branch` from `from` (version `v`) to the indexed commit `to` with a correct audit
/// row, as raw SQL in `tx` (what a runtime bypassing the repository could attempt).
async fn raw_move(
    tx: &mut sqlx::PgConnection,
    graph: &GraphId,
    branch: &str,
    from: &CommitId,
    v: i64,
    to: &CommitId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ref_events (graph_id, branch, old_head, new_head, old_version, new_version, operation, \
         tenant_id, principal_id, principal_type) VALUES ($1, $2, $3, $4, $5, $6, 'advance', 'tenant-a', 'urn:it:raw', 'agent')",
    )
    .bind(graph.as_str())
    .bind(branch)
    .bind(from.to_string())
    .bind(to.to_string())
    .bind(v)
    .bind(v + 1)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE refs SET head = $3, version = $4, updated_at = now() WHERE graph_id = $1 AND branch = $2")
        .bind(graph.as_str())
        .bind(branch)
        .bind(to.to_string())
        .bind(v + 1)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

fn db_message(e: sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(d) => format!("{} {}", d.code().unwrap_or_default(), d.message()),
        e => panic!("not a database error: {e}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_deleted_branch_head_never_moves_even_by_raw_sql_racing_the_delete() {
    let store = store().await;
    let pool = store.pool().clone();
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 1).await;
    for name in ["guard-control", "guard-deleted", "guard-race"] {
        store
            .workflows()
            .create_branch(
                &create(&g, &format!("c-{name}"), name, "main", None),
                limits(),
            )
            .await
            .unwrap();
    }
    let target = |n: &str| {
        let (store, g, c0, n) = (&store, &g, c[0].clone(), n.to_owned());
        async move {
            prepare_on(store, g, &n, Some(c0), &format!("{g}-{n}-t"))
                .await
                .unwrap()
        }
    };
    // Control: a genuine fast-forward with its audit row moves an active branch.
    let to = target("guard-control").await;
    let mut tx = pool.begin().await.unwrap();
    raw_move(&mut tx, &g, "guard-control", &c[0], 1, &to)
        .await
        .unwrap();
    // Evaluate the deferred audit triggers now: the move would commit (then undo it).
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    // The same move on a deleted branch is refused by 0012's guard (not by 0009's audit).
    let to = target("guard-deleted").await;
    store
        .workflows()
        .delete_branch(&lifecycle(&g, "d-guard", "guard-deleted"))
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let e = raw_move(&mut tx, &g, "guard-deleted", &c[0], 1, &to)
        .await
        .unwrap_err();
    let msg = db_message(e);
    assert!(
        msg.starts_with("23000") && msg.contains("is deleted; its head does not move"),
        "{msg}"
    );
    tx.rollback().await.unwrap();
    // Race: an uncommitted tombstone (owner, by hand) and a raw move. The move's guard waits
    // for the tombstone (FOR SHARE) instead of reading the pre-delete status, then refuses.
    let to = target("guard-race").await;
    let mut del = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *del)
        .await
        .unwrap();
    sqlx::query("UPDATE branches SET status = 'deleted', lifecycle_version = 2, updated_at = now() WHERE graph_id = $1 AND branch = 'guard-race'")
        .bind(g.as_str())
        .execute(&mut *del)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO branch_events (graph_id, branch, tenant_id, lifecycle_version, operation, status_after, head, ref_version, \
         principal_id, principal_type) VALUES ($1, 'guard-race', 'tenant-a', 2, 'deleted', 'deleted', $2, 1, 'urn:it:owner', 'service')",
    )
    .bind(g.as_str())
    .bind(c[0].to_string())
    .execute(&mut *del)
    .await
    .unwrap();
    let (p2, g2, c0, to2) = (pool.clone(), g.clone(), c[0].clone(), to.clone());
    let mover = tokio::spawn(async move {
        let mut tx = p2.begin().await.unwrap();
        let r = raw_move(&mut tx, &g2, "guard-race", &c0, 1, &to2).await;
        if r.is_ok() { tx.commit().await } else { r }
    });
    until_waiting(&pool, pid, 1).await;
    del.commit().await.unwrap();
    let msg = db_message(mover.await.unwrap().unwrap_err());
    assert!(
        msg.starts_with("23000") && msg.contains("is deleted"),
        "{msg}"
    );
    assert_eq!(
        store.ref_head(&g, "guard-race").await.unwrap().unwrap(),
        (c[0].clone(), 1)
    );
    // Likewise a raw proposal racing an uncommitted tombstone waits and is refused.
    store
        .workflows()
        .create_branch(&create(&g, "c-guard-p", "guard-p", "main", None), limits())
        .await
        .unwrap();
    let mut del = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *del)
        .await
        .unwrap();
    sqlx::query("UPDATE branches SET status = 'deleted', lifecycle_version = 2, updated_at = now() WHERE graph_id = $1 AND branch = 'guard-p'")
        .bind(g.as_str())
        .execute(&mut *del)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO branch_events (graph_id, branch, tenant_id, lifecycle_version, operation, status_after, head, ref_version, \
         principal_id, principal_type) VALUES ($1, 'guard-p', 'tenant-a', 2, 'deleted', 'deleted', $2, 1, 'urn:it:owner', 'service')",
    )
    .bind(g.as_str())
    .bind(c[0].to_string())
    .execute(&mut *del)
    .await
    .unwrap();
    let (p3, g3, c0, to3) = (pool.clone(), g.clone(), c[0].clone(), to.clone());
    let proposer = tokio::spawn(async move {
        sqlx::query(
            "INSERT INTO proposals (graph_id, branch, tenant_id, principal_id, principal_type, expected_head, requested_patch_id, \
             effective_patch_id, candidate_commit) SELECT graph_id, 'guard-p', tenant_id, 'urn:it:raw', 'agent', $2, requested_patch_id, \
             effective_patch_id, $3 FROM proposals WHERE graph_id = $1 AND candidate_commit = $3 LIMIT 1",
        )
        .bind(g3.as_str())
        .bind(c0.to_string())
        .bind(to3.to_string())
        .execute(&p3)
        .await
    });
    until_waiting(&pool, pid, 1).await;
    del.commit().await.unwrap();
    let msg = db_message(proposer.await.unwrap().unwrap_err());
    assert!(
        msg.starts_with("23000") && msg.contains("is deleted"),
        "{msg}"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn distinct_reviewer_means_distinct_accountable_parties() {
    let store = store().await;
    let g = graph(&store, "tenant-a").await;
    let c = main_history(&store, &g, 1).await;
    let review = CreateBranchRequest {
        policy: BranchPolicy {
            protected: false,
            require_validation: false,
            require_distinct_reviewer: true,
        },
        ..create(&g, "rv", "four-eyes", "main", None)
    };
    store
        .workflows()
        .create_branch(&review, limits())
        .await
        .unwrap();
    // Proposed by urn:it:curator (agent, no delegator).
    let candidate = prepare_on(
        &store,
        &g,
        "four-eyes",
        Some(c[0].clone()),
        &format!("{g}-fe"),
    )
    .await
    .unwrap();
    let accept_as = |who: AuthenticatedPrincipal, key: &str| {
        let (store, g, c0, candidate, key) =
            (&store, &g, c[0].clone(), candidate.clone(), key.to_owned());
        async move {
            store
                .workflows()
                .accept(&AcceptRequest {
                    scope: scope_as(who, g, &key, &key),
                    branch: "four-eyes".into(),
                    expected_head: Some(c0),
                    candidate,
                    reason: None,
                    validation: ValidationPolicy::NoValidation,
                })
                .await
        }
    };
    let mut same_with_delegator = actor("tenant-a", "curator");
    same_with_delegator.on_behalf_of = Some(PrincipalId::new("urn:it:boss").unwrap());
    let mut same_other_type = actor("tenant-a", "curator");
    same_other_type.principal_type = PrincipalType::Human;
    let mut agent_for_proposer = actor("tenant-a", "helper");
    agent_for_proposer.on_behalf_of = Some(PrincipalId::new("urn:it:curator").unwrap());
    for (who, key) in [
        (same_with_delegator, "fe-delegated"),
        (same_other_type, "fe-type"),
        (agent_for_proposer, "fe-for-proposer"),
    ] {
        let r = accept_as(who, key).await;
        assert!(
            matches!(r, Err(LedgerError::BranchPolicyViolation(_))),
            "{key}: {r:?}"
        );
    }
    // A different party (even acting for someone else) accepts.
    let mut other = actor("tenant-a", "reviewer");
    other.on_behalf_of = Some(PrincipalId::new("urn:it:lead").unwrap());
    accept_as(other, "fe-other").await.unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn main_stays_protected_and_activated_graphs_adopt_their_refs() {
    // Own database: the raw import below has no ref events for its imported head (Phase-1
    // import semantics), which the shared database's global verify must never see active.
    let base = database_url();
    let (head, query) = base
        .split_once('?')
        .map_or((base.as_str(), None), |(h, q)| (h, Some(q)));
    let name = unique("br_activation").replace('-', "_").to_lowercase();
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = format!("{}/{name}", &head[..head.rfind('/').unwrap()]);
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    let store = PostgresLedgerStore::connect_and_migrate(&url, V1Binding::Reject)
        .await
        .unwrap();
    let pool = store.pool().clone();
    // refs_main_protected: no role can record an unprotected main.
    let g = graph(&store, "tenant-a").await;
    let candidate = prepare_on(&store, &g, "main", None, &format!("{g}-root"))
        .await
        .unwrap();
    let e = sqlx::query(
        "INSERT INTO refs (graph_id, branch, head, protected) VALUES ($1, 'main', $2, false)",
    )
    .bind(g.as_str())
    .bind(candidate.to_string())
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(db_message(e).starts_with("23514"));
    // An importing graph gets a raw main; activation adopts it, and it can then be accepted onto.
    sqlx::query("UPDATE graphs SET status = 'importing' WHERE graph_id = $1")
        .bind(g.as_str())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO refs (graph_id, branch, head) VALUES ($1, 'main', $2)")
        .bind(g.as_str())
        .bind(candidate.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        store
            .workflows()
            .branch(&tenant(), &g, "main")
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE graphs SET status = 'active' WHERE graph_id = $1")
        .bind(g.as_str())
        .execute(&pool)
        .await
        .unwrap();
    let main = store
        .workflows()
        .branch(&tenant(), &g, "main")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            main.origin.as_str(),
            main.status.as_str(),
            main.lifecycle_version
        ),
        ("adopted", "active", 1)
    );
    let (events, _) = store
        .workflows()
        .branch_history(&tenant(), &g, "main", 10)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            events.len(),
            events[0].operation.as_str(),
            events[0].head.clone(),
            events[0].principal_id.as_str()
        ),
        (
            1,
            "adopted",
            candidate.clone(),
            "urn:sculpin:ledger:graph-activation"
        )
    );
    accept_on(
        &store,
        &g,
        "main",
        Some(candidate),
        &format!("{g}-after-activation"),
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn lifecycle_reasons_take_the_same_bound_as_decisions() {
    let store = store().await;
    let g = graph(&store, "tenant-a").await;
    main_history(&store, &g, 1).await;
    let repo = store.workflows();
    repo.create_branch(&create(&g, "c-long", "long-reason", "main", None), limits())
        .await
        .unwrap();
    // The longest reason the API accepts (MAX_REASON_BYTES) is stored, not a database error.
    let longest = "r".repeat(ledger_store::MAX_REASON_BYTES);
    let deleted = repo
        .delete_branch(&BranchLifecycleRequest {
            reason: Some(longest.clone()),
            ..lifecycle(&g, "d-long", "long-reason")
        })
        .await
        .unwrap();
    assert_eq!(deleted.event.reason.as_deref(), Some(longest.as_str()));
    // History is bounded by `limit`: the latest lifecycle events, oldest first among them.
    let (events, movements) = repo
        .branch_history(&tenant(), &g, "long-reason", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (events.len(), events[0].operation.as_str(), movements.len()),
        (1, "deleted", 1)
    );
    let (events, _) = repo
        .branch_history(&tenant(), &g, "long-reason", 10)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        events
            .iter()
            .map(|e| e.operation.as_str())
            .collect::<Vec<_>>(),
        ["created", "deleted"]
    );
    let too_long = repo
        .restore_branch(&BranchLifecycleRequest {
            reason: Some("r".repeat(ledger_store::MAX_REASON_BYTES + 1)),
            ..lifecycle(&g, "r-long", "long-reason")
        })
        .await;
    assert!(
        matches!(
            too_long,
            Err(LedgerError::InvalidIdentifier {
                field: "reason",
                ..
            })
        ),
        "{too_long:?}"
    );
}
