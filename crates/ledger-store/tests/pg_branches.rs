//! Real-PostgreSQL evidence for named branches (ADR-0022; Plan 0008): creation from the
//! source head and from reachable history (unreachable, foreign and unknown points fail
//! closed), tombstone deletion and restore with head/version preserved, idempotency of every
//! lifecycle operation, policy enforcement inside the acceptance transaction, `main`
//! protection, the database guards behind all of it, and the lifecycle races (accept vs
//! delete, restore vs accept, delete vs prepare) under real row locks. Every test is
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

#[tokio::test]
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
        // restore vs accept of a fresh proposal: the accept succeeds only on an active branch.
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
        // delete vs prepare-then-accept: exactly one of "accepted on an active branch" or
        // "refused as deleted".
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
    // A deleted branch's head never moves (the movement guard of 0012, before 0009's).
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
