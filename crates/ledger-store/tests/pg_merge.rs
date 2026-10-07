//! Real-PostgreSQL evidence for Phase-5 merges (ADR-0023 integration commits, ADR-0024
//! preview / propose / apply): classification, side-effect-free preview, fast-forward and
//! divergent integration, strategies and conflicts, reconstruction of the integration commit,
//! repeated and two-way merges, staleness at propose and at apply, target-policy enforcement,
//! durable replay, the database guards that keep merge candidates on the merge path, and the
//! merge races forced to interleave (opposite merges, apply vs target acceptance, two applies
//! of one proposal). Every test is `#[ignore]` and runs with `LEDGER_TEST_DATABASE_URL`.
#![cfg(feature = "postgres")]

use ledger_core::{
    AuthenticatedPrincipal, CommitId, ContentId, GraphId, LedgerError, PrincipalId, PrincipalType,
    TenantId,
};
use ledger_rdf::{Operation, OperationKind, Patch, Quad};
use ledger_store::{
    AcceptRequest, ApplyMergeRequest, BranchLifecycleRequest, BranchPolicy, CreateBranchRequest,
    GraphStatus, MergeClass, MergePreview, MergeSpec, MergeStrategy, NewGraph, PostgresLedgerStore,
    PrepareRequest, ProposeMergeRequest, RequestScope, TraversalLimits, V1Binding,
    ValidationPolicy,
};
use std::{
    collections::BTreeSet,
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

async fn graph(store: &PostgresLedgerStore) -> GraphId {
    let id = GraphId::new(unique("mg")).unwrap();
    store
        .graphs()
        .create(&NewGraph {
            graph_id: id.clone(),
            tenant_id: TenantId::new("tenant-a").unwrap(),
            knowledge_base_id: Some(unique("kb")),
            purpose: None,
            status: GraphStatus::Active,
        })
        .await
        .unwrap();
    id
}

fn tenant() -> TenantId {
    TenantId::new("tenant-a").unwrap()
}

fn actor(principal: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal {
        principal_id: PrincipalId::new(format!("urn:it:{principal}")).unwrap(),
        principal_type: PrincipalType::Agent,
        tenant_id: tenant(),
        on_behalf_of: None,
    }
}

fn scope_as(principal: &str, graph: &GraphId, key: &str, digest: &str) -> RequestScope {
    RequestScope {
        principal: actor(principal),
        graph: graph.clone(),
        idempotency_key: key.to_owned(),
        request_digest: ContentId::for_bytes(digest.as_bytes()),
        correlation_id: None,
    }
}

fn scope(graph: &GraphId, key: &str) -> RequestScope {
    scope_as("curator", graph, key, key)
}

fn limits() -> TraversalLimits {
    TraversalLimits::DEFAULT
}

fn q(s: &str) -> Quad {
    s.parse().unwrap()
}

/// Prepare + accept one patch (adds / deletes of full quads) on `branch`; the new head.
async fn change(
    store: &PostgresLedgerStore,
    g: &GraphId,
    branch: &str,
    head: Option<CommitId>,
    adds: &[&str],
    deletes: &[&str],
) -> CommitId {
    let key = unique("c");
    let ops = adds
        .iter()
        .map(|a| Operation {
            kind: OperationKind::Add,
            quad: q(a),
        })
        .chain(deletes.iter().map(|d| Operation {
            kind: OperationKind::Delete,
            quad: q(d),
        }));
    let prepared = store
        .workflows()
        .prepare(&PrepareRequest {
            scope: scope(g, &format!("p-{key}")),
            branch: branch.into(),
            expected_head: head.clone(),
            requested: Patch::new(ops).unwrap(),
            activity: "cognitive-correction".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: key.clone(),
        })
        .await
        .unwrap();
    store
        .workflows()
        .accept(&AcceptRequest {
            scope: scope(g, &format!("a-{key}")),
            branch: branch.into(),
            expected_head: head,
            candidate: prepared.candidate.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await
        .unwrap();
    prepared.candidate
}

async fn branch(store: &PostgresLedgerStore, g: &GraphId, name: &str, policy: BranchPolicy) {
    store
        .workflows()
        .create_branch(
            &CreateBranchRequest {
                scope: scope(g, &format!("cb-{name}")),
                name: name.into(),
                source: "main".into(),
                from_commit: None,
                policy,
            },
            limits(),
        )
        .await
        .unwrap();
}

fn spec(source: &str, target: &str, strategy: MergeStrategy) -> MergeSpec {
    MergeSpec {
        source: source.into(),
        target: target.into(),
        strategy,
        base: None,
    }
}

async fn preview(store: &PostgresLedgerStore, g: &GraphId, s: &MergeSpec) -> MergePreview {
    store
        .workflows()
        .merge_preview(&tenant(), g, s, limits())
        .await
        .unwrap()
}

async fn propose(
    store: &PostgresLedgerStore,
    g: &GraphId,
    s: &MergeSpec,
    token: &str,
    key: &str,
) -> Result<ledger_store::MergeProposed, LedgerError> {
    store
        .workflows()
        .merge_propose(
            &ProposeMergeRequest {
                scope: scope(g, key),
                spec: s.clone(),
                preview_token: token.into(),
                message: "integrate".into(),
                evidence_refs: vec![],
            },
            limits(),
        )
        .await
}

async fn apply(
    store: &PostgresLedgerStore,
    g: &GraphId,
    proposal: i64,
    token: &str,
    who: &str,
    key: &str,
) -> Result<ledger_store::MergeApplied, LedgerError> {
    store
        .workflows()
        .merge_apply(&ApplyMergeRequest {
            scope: scope_as(who, g, key, key),
            proposal_id: proposal,
            preview_token: token.into(),
            reason: Some("merge".into()),
            validation: ValidationPolicy::NoValidation,
        })
        .await
}

async fn state(store: &PostgresLedgerStore, id: &CommitId) -> BTreeSet<Quad> {
    store
        .workflows()
        .reconstruct(id, &ledger_store::ReconstructionLimits::DEVELOPMENT)
        .await
        .unwrap()
}

async fn head(store: &PostgresLedgerStore, g: &GraphId, b: &str) -> (CommitId, i64) {
    store.ref_head(g, b).await.unwrap().unwrap()
}

/// Row counts of everything a merge could write, for this graph only (other tests write
/// concurrently to the shared database).
async fn counts(store: &PostgresLedgerStore, g: &GraphId) -> Vec<i64> {
    let mut out = Vec::new();
    for t in [
        "proposals",
        "merge_proposals",
        "decisions",
        "ref_events",
        "projection_outbox",
        "idempotency",
        "commit_index",
    ] {
        out.push(
            sqlx::query_scalar(&format!("SELECT count(*) FROM {t} WHERE graph_id = $1"))
                .bind(g.as_str())
                .fetch_one(store.pool())
                .await
                .unwrap(),
        );
    }
    out
}

async fn verify_clean(store: &PostgresLedgerStore) {
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

const A: &str = "<urn:a> <urn:p> \"1\" .";
const B: &str = "<urn:b> <urn:p> \"1\" .";

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn fast_forward_integrates_the_source_state_with_one_audited_commit() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "agent/ff", BranchPolicy::default()).await;
    let s1 = change(&store, &g, "agent/ff", Some(c1.clone()), &[B], &[]).await;
    let s2 = change(
        &store,
        &g,
        "agent/ff",
        Some(s1.clone()),
        &["<urn:a> <urn:p> \"2\" ."],
        &[A],
    )
    .await;
    let sp = spec("agent/ff", "main", MergeStrategy::Abort);
    // Preview is side-effect free.
    let before = counts(&store, &g).await;
    let p = preview(&store, &g, &sp).await;
    assert_eq!(counts(&store, &g).await, before);
    assert_eq!(p.class, MergeClass::FastForward);
    assert_eq!(
        (p.merge_base.clone(), p.ahead, p.behind),
        (Some(c1.clone()), 2, 0)
    );
    let token = p.preview_token.clone().unwrap();
    let proposed = propose(&store, &g, &sp, &token, "ff-propose")
        .await
        .unwrap();
    assert_eq!(
        (proposed.classification.as_str(), proposed.strategy.as_str()),
        ("fast_forward", "abort")
    );
    // Nothing moved on propose.
    assert_eq!(head(&store, &g, "main").await, (c1.clone(), 1));
    let applied = apply(
        &store,
        &g,
        proposed.proposal_id,
        &token,
        "reviewer",
        "ff-apply",
    )
    .await
    .unwrap();
    assert_eq!(applied.ref_version, 2);
    let (main, _) = head(&store, &g, "main").await;
    assert_eq!(main, proposed.candidate);
    // The integration commit reconstructs to the source state and the previewed digest.
    assert_eq!(state(&store, &main).await, state(&store, &s2).await);
    assert_eq!(
        ledger_rdf::state_digest(&state(&store, &main).await),
        p.merged_state_digest.unwrap()
    );
    let (op, parents): (String, i16) = sqlx::query_as(
        "SELECT e.operation, c.parent_count FROM ref_events e JOIN commit_index c ON c.id = e.new_head \
         WHERE e.graph_id = $1 AND e.branch = 'main' AND e.new_version = 2",
    )
    .bind(g.as_str())
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!((op.as_str(), parents), ("merge", 2));
    // Repeating the merge: contained. Merging back: no state change, nothing created.
    assert_eq!(
        preview(&store, &g, &sp).await.class,
        MergeClass::AlreadyContained
    );
    let back = preview(&store, &g, &spec("main", "agent/ff", MergeStrategy::Abort)).await;
    assert_eq!(back.class, MergeClass::NoChange);
    assert!(back.preview_token.is_none());
    assert!(matches!(
        propose(
            &store,
            &g,
            &spec("main", "agent/ff", MergeStrategy::Abort),
            "sha256:00",
            "back"
        )
        .await,
        Err(LedgerError::MergeNothingToDo(_))
    ));
    // Applying the same proposal again under a new key: reported as decided (not stale).
    let again = apply(
        &store,
        &g,
        proposed.proposal_id,
        &token,
        "reviewer",
        "ff-apply-2",
    )
    .await;
    assert!(
        matches!(&again, Err(LedgerError::LineageMismatch(m)) if m.contains("terminal decision (accepted")),
        "{again:?}"
    );
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn divergent_merges_follow_the_structural_rules_and_repeat_correctly() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A, B], &[]).await;
    branch(&store, &g, "agent/div", BranchPolicy::default()).await;
    // Independent changes: main edits <urn:a>, the branch adds <urn:c>.
    let m2 = change(
        &store,
        &g,
        "main",
        Some(c1.clone()),
        &["<urn:a> <urn:p> \"2\" ."],
        &[A],
    )
    .await;
    let s1 = change(
        &store,
        &g,
        "agent/div",
        Some(c1.clone()),
        &["<urn:c> <urn:p> \"x\" ."],
        &[],
    )
    .await;
    let sp = spec("agent/div", "main", MergeStrategy::Abort);
    let p = preview(&store, &g, &sp).await;
    assert_eq!(
        (p.class.clone(), p.merge_base.clone(), p.conflict_count),
        (MergeClass::Divergent, Some(c1.clone()), 0)
    );
    let token = p.preview_token.clone().unwrap();
    let proposed = propose(&store, &g, &sp, &token, "d-propose").await.unwrap();
    apply(
        &store,
        &g,
        proposed.proposal_id,
        &token,
        "reviewer",
        "d-apply",
    )
    .await
    .unwrap();
    let (main, _) = head(&store, &g, "main").await;
    let want: BTreeSet<Quad> = ["<urn:a> <urn:p> \"2\" .", B, "<urn:c> <urn:p> \"x\" ."]
        .iter()
        .map(|s| q(s))
        .collect();
    assert_eq!(state(&store, &main).await, want);
    // Parent order: [target, source].
    let parents: Vec<String> = sqlx::query_scalar(
        "SELECT parent_id FROM commit_parents WHERE commit_id = $1 ORDER BY position",
    )
    .bind(main.to_string())
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(parents, vec![m2.to_string(), s1.to_string()]);
    // The source advances; the next merge uses the old source head as base.
    let s2 = change(
        &store,
        &g,
        "agent/div",
        Some(s1.clone()),
        &["<urn:d> <urn:p> \"y\" ."],
        &[],
    )
    .await;
    let p2 = preview(&store, &g, &sp).await;
    assert_eq!(
        (p2.class.clone(), p2.merge_base.clone(), p2.ahead),
        (MergeClass::Divergent, Some(s1.clone()), 1)
    );
    let t2 = p2.preview_token.clone().unwrap();
    let pr2 = propose(&store, &g, &sp, &t2, "d-propose-2").await.unwrap();
    apply(&store, &g, pr2.proposal_id, &t2, "reviewer", "d-apply-2")
        .await
        .unwrap();
    let (main2, _) = head(&store, &g, "main").await;
    let want2: BTreeSet<Quad> = [
        "<urn:a> <urn:p> \"2\" .",
        B,
        "<urn:c> <urn:p> \"x\" .",
        "<urn:d> <urn:p> \"y\" .",
    ]
    .iter()
    .map(|s| q(s))
    .collect();
    assert_eq!(state(&store, &main2).await, want2);
    let _ = s2;
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn conflicts_abort_or_resolve_by_strategy_only() {
    let store = store().await;
    let g = graph(&store).await;
    let x = "<urn:s> <urn:p> \"X\" .";
    let c1 = change(&store, &g, "main", None, &[x, B], &[]).await;
    branch(&store, &g, "agent/conf", BranchPolicy::default()).await;
    // Same slot: main X->Y, branch X->Z; the branch also deletes B while main keeps it.
    change(
        &store,
        &g,
        "main",
        Some(c1.clone()),
        &["<urn:s> <urn:p> \"Y\" ."],
        &[x],
    )
    .await;
    change(
        &store,
        &g,
        "agent/conf",
        Some(c1.clone()),
        &["<urn:s> <urn:p> \"Z\" ."],
        &[x, B],
    )
    .await;
    let abort = preview(
        &store,
        &g,
        &spec("agent/conf", "main", MergeStrategy::Abort),
    )
    .await;
    assert_eq!(
        (abort.class.clone(), abort.conflict_count),
        (MergeClass::Conflicted, 1)
    );
    assert_eq!(abort.conflicts[0].key.subject, "<urn:s>");
    assert!(abort.preview_token.is_none());
    assert!(matches!(
        propose(
            &store,
            &g,
            &spec("agent/conf", "main", MergeStrategy::Abort),
            "sha256:00",
            "ab"
        )
        .await,
        Err(LedgerError::MergeConflict(1))
    ));
    for (strategy, slot) in [
        (MergeStrategy::TakeTarget, vec!["<urn:s> <urn:p> \"Y\" ."]),
        (MergeStrategy::TakeSource, vec!["<urn:s> <urn:p> \"Z\" ."]),
        (
            MergeStrategy::Union,
            vec!["<urn:s> <urn:p> \"Y\" .", "<urn:s> <urn:p> \"Z\" ."],
        ),
    ] {
        let p = preview(&store, &g, &spec("agent/conf", "main", strategy)).await;
        assert_eq!(p.class, MergeClass::Divergent, "{strategy:?}");
        // B was deleted only by the source (main kept it unchanged): it goes, in every
        // strategy; the conflicting slot follows the strategy.
        let want: BTreeSet<Quad> = slot.iter().map(|s| q(s)).collect();
        assert_eq!(
            p.merged_state_digest.clone().unwrap(),
            ledger_rdf::state_digest(&want),
            "{strategy:?}"
        );
    }
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn stale_previews_and_proposals_are_refused_never_recomputed() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "agent/st", BranchPolicy::default()).await;
    let s1 = change(&store, &g, "agent/st", Some(c1.clone()), &[B], &[]).await;
    let sp = spec("agent/st", "main", MergeStrategy::Abort);
    // Preview, then the target moves: propose is stale.
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    let m2 = change(
        &store,
        &g,
        "main",
        Some(c1.clone()),
        &["<urn:m> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    assert!(matches!(
        propose(&store, &g, &sp, &token, "st-1").await,
        Err(LedgerError::MergeStale(_))
    ));
    // Fresh preview + propose, then the source moves: apply is stale (stored heads).
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    let pr = propose(&store, &g, &sp, &token, "st-2").await.unwrap();
    let s2 = change(
        &store,
        &g,
        "agent/st",
        Some(s1.clone()),
        &["<urn:n> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    assert!(matches!(
        apply(&store, &g, pr.proposal_id, &token, "reviewer", "st-apply").await,
        Err(LedgerError::MergeStale(_))
    ));
    // A wrong token is stale too.
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    let pr = propose(&store, &g, &sp, &token, "st-3").await.unwrap();
    let bogus = format!("sha256:{}", "0".repeat(64));
    assert!(matches!(
        apply(&store, &g, pr.proposal_id, &bogus, "reviewer", "st-apply-2").await,
        Err(LedgerError::MergeStale(_))
    ));
    // The target moves after propose: stale.
    change(
        &store,
        &g,
        "main",
        Some(m2.clone()),
        &["<urn:o> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    assert!(matches!(
        apply(&store, &g, pr.proposal_id, &token, "reviewer", "st-apply-3").await,
        Err(LedgerError::MergeStale(_))
    ));
    // Source deleted after propose: stale; target deleted: refused as deleted.
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    let pr = propose(&store, &g, &sp, &token, "st-4").await.unwrap();
    store
        .workflows()
        .delete_branch(&BranchLifecycleRequest {
            scope: scope(&g, "del-src"),
            name: "agent/st".into(),
            reason: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        apply(&store, &g, pr.proposal_id, &token, "reviewer", "st-apply-4").await,
        Err(LedgerError::MergeStale(_))
    ));
    let _ = s2;
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn merge_candidates_take_only_the_merge_path_and_the_target_policy_governs() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    // A strict target and a lax source.
    store
        .workflows()
        .create_branch(
            &CreateBranchRequest {
                scope: scope(&g, "cb-strict"),
                name: "release".into(),
                source: "main".into(),
                from_commit: None,
                policy: BranchPolicy {
                    protected: false,
                    require_validation: true,
                    require_distinct_reviewer: true,
                },
            },
            limits(),
        )
        .await
        .unwrap();
    branch(&store, &g, "agent/lax", BranchPolicy::default()).await;
    change(&store, &g, "agent/lax", Some(c1.clone()), &[B], &[]).await;
    // The ordinary accept never installs a merge candidate (a lax target, so nothing else
    // refuses it first).
    branch(&store, &g, "plain", BranchPolicy::default()).await;
    let lax = spec("agent/lax", "plain", MergeStrategy::Abort);
    let lax_token = preview(&store, &g, &lax).await.preview_token.unwrap();
    let lax_pr = propose(&store, &g, &lax, &lax_token, "pol-lax")
        .await
        .unwrap();
    let ordinary = store
        .workflows()
        .accept(&AcceptRequest {
            scope: scope_as("reviewer", &g, "pol-accept", "pol-accept"),
            branch: "plain".into(),
            expected_head: Some(c1.clone()),
            candidate: lax_pr.candidate.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await;
    assert!(
        matches!(&ordinary, Err(LedgerError::LineageMismatch(m)) if m.contains("merge apply")),
        "{ordinary:?}"
    );
    let sp = spec("agent/lax", "release", MergeStrategy::Abort);
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    let pr = propose(&store, &g, &sp, &token, "pol-propose")
        .await
        .unwrap();
    // The target requires validation: the lax source does not lower it.
    assert!(matches!(
        apply(&store, &g, pr.proposal_id, &token, "reviewer", "pol-apply").await,
        Err(LedgerError::ValidationRequired)
    ));
    // The database refuses an `advance` event for a merge candidate, even for the owner.
    let raw = sqlx::query(
        "INSERT INTO ref_events (graph_id, branch, old_head, new_head, old_version, new_version, \
         operation, tenant_id, principal_id, principal_type) \
         VALUES ($1, 'release', $2, $3, 1, 2, 'advance', 'tenant-a', 'urn:it:raw', 'agent')",
    )
    .bind(g.as_str())
    .bind(c1.to_string())
    .bind(pr.candidate.to_string())
    .execute(store.pool())
    .await
    .unwrap_err();
    assert!(
        raw.to_string().contains("only a merge event installs it"),
        "{raw}"
    );
    // …and a `merge` event without a matching merge proposal.
    let other = change(
        &store,
        &g,
        "main",
        Some(c1.clone()),
        &["<urn:z> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    let raw = sqlx::query(
        "INSERT INTO ref_events (graph_id, branch, old_head, new_head, old_version, new_version, \
         operation, tenant_id, principal_id, principal_type) \
         VALUES ($1, 'main', $2, $3, 2, 3, 'merge', 'tenant-a', 'urn:it:raw', 'agent')",
    )
    .bind(g.as_str())
    .bind(other.to_string())
    .bind(c1.to_string())
    .execute(store.pool())
    .await
    .unwrap_err();
    assert!(
        raw.to_string().contains("no matching merge proposal"),
        "{raw}"
    );
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn completed_merge_requests_replay_before_any_recomputation() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "agent/rp", BranchPolicy::default()).await;
    let s1 = change(&store, &g, "agent/rp", Some(c1.clone()), &[B], &[]).await;
    let sp = spec("agent/rp", "main", MergeStrategy::Abort);
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    let pr = propose(&store, &g, &sp, &token, "rp-propose")
        .await
        .unwrap();
    let ap = apply(&store, &g, pr.proposal_id, &token, "reviewer", "rp-apply")
        .await
        .unwrap();
    // The source moves afterwards: replays still return the original results.
    change(
        &store,
        &g,
        "agent/rp",
        Some(s1.clone()),
        &["<urn:q> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    let again = propose(&store, &g, &sp, &token, "rp-propose")
        .await
        .unwrap();
    assert!(again.replayed && again.proposal_id == pr.proposal_id);
    let again = apply(&store, &g, pr.proposal_id, &token, "reviewer", "rp-apply")
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!((again.decision_id, again.head), (ap.decision_id, ap.head));
    // Same key, different request: conflict.
    let changed = store
        .workflows()
        .merge_propose(
            &ProposeMergeRequest {
                scope: scope_as("curator", &g, "rp-propose", "another request"),
                spec: sp.clone(),
                preview_token: token.clone(),
                message: "integrate".into(),
                evidence_refs: vec![],
            },
            limits(),
        )
        .await;
    assert!(matches!(changed, Err(LedgerError::IdempotencyConflict)));
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn criss_cross_needs_an_explicit_best_common_ancestor() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "x", BranchPolicy::default()).await;
    branch(&store, &g, "y", BranchPolicy::default()).await;
    let x1 = change(
        &store,
        &g,
        "x",
        Some(c1.clone()),
        &["<urn:x> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    let y1 = change(
        &store,
        &g,
        "y",
        Some(c1.clone()),
        &["<urn:y> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    // Criss-cross by deliberate historical branching: x2 = merge(y1 into x), and a branch
    // made at x1 merged into y gives y2 = [y1, x1].
    let sp = spec("y", "x", MergeStrategy::Abort);
    let t = preview(&store, &g, &sp).await.preview_token.unwrap();
    let pr = propose(&store, &g, &sp, &t, "cc-1").await.unwrap();
    apply(&store, &g, pr.proposal_id, &t, "reviewer", "cc-1a")
        .await
        .unwrap();
    store
        .workflows()
        .create_branch(
            &CreateBranchRequest {
                scope: scope(&g, "cb-w"),
                name: "w".into(),
                source: "x".into(),
                from_commit: Some(x1.clone()),
                policy: BranchPolicy::default(),
            },
            limits(),
        )
        .await
        .unwrap();
    let sw = spec("w", "y", MergeStrategy::Abort);
    let t = preview(&store, &g, &sw).await.preview_token.unwrap();
    let pr = propose(&store, &g, &sw, &t, "cc-2").await.unwrap();
    apply(&store, &g, pr.proposal_id, &t, "reviewer", "cc-2a")
        .await
        .unwrap();
    // Now x = [x1, y1] and y = [y1, x1]: two best common ancestors.
    let p = preview(&store, &g, &spec("y", "x", MergeStrategy::Abort)).await;
    let mut want = vec![x1.clone(), y1.clone()];
    want.sort();
    // Without a base the history is reported ambiguous. Both branches have the same state;
    // with base x1 the source side still changed something relative to that base (y's
    // commit), so the merge records an empty integration (ADR-0023: the resolution is kept
    // in history) whose merged state is exactly the target state.
    assert_eq!(p.class, MergeClass::AmbiguousMergeBase(want));
    let explicit = MergeSpec {
        base: Some(x1.clone()),
        ..spec("y", "x", MergeStrategy::Abort)
    };
    let (xh, _) = head(&store, &g, "x").await;
    let e = preview(&store, &g, &explicit).await;
    assert_eq!(e.class, MergeClass::Divergent);
    assert_eq!(
        e.merged_state_digest.unwrap(),
        ledger_rdf::state_digest(&state(&store, &xh).await)
    );
    let wrong = MergeSpec {
        base: Some(c1.clone()),
        ..spec("y", "x", MergeStrategy::Abort)
    };
    assert!(matches!(
        store
            .workflows()
            .merge_preview(&tenant(), &g, &wrong, limits())
            .await,
        Err(LedgerError::InvalidMergeBase(_))
    ));
    verify_clean(&store).await;
}

// ---- forced merge races --------------------------------------------------------------------

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

async fn until_waiting(pool: &sqlx::PgPool, holder: i32, n: i64) {
    for _ in 0..400 {
        if waiting_on(pool, holder).await >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{n} backends never queued behind {holder}");
}

/// An owner transaction holding the ref rows of `branches` exclusively.
async fn hold_refs(
    pool: &sqlx::PgPool,
    g: &GraphId,
    branches: &[&str],
) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
    let mut tx = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    for b in branches {
        sqlx::query("SELECT 1 FROM refs WHERE graph_id = $1 AND branch = $2 FOR UPDATE")
            .bind(g.as_str())
            .bind(*b)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    (tx, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn opposite_merges_serialize_and_never_create_a_criss_cross() {
    let store = Arc::new(store().await);
    let pool = store.pool().clone();
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "alpha", BranchPolicy::default()).await;
    branch(&store, &g, "beta", BranchPolicy::default()).await;
    change(
        &store,
        &g,
        "alpha",
        Some(c1.clone()),
        &["<urn:al> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    change(
        &store,
        &g,
        "beta",
        Some(c1.clone()),
        &["<urn:be> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    let ab = spec("alpha", "beta", MergeStrategy::Abort);
    let ba = spec("beta", "alpha", MergeStrategy::Abort);
    let tab = preview(&store, &g, &ab).await.preview_token.unwrap();
    let tba = preview(&store, &g, &ba).await.preview_token.unwrap();
    let pab = propose(&store, &g, &ab, &tab, "op-ab").await.unwrap();
    let pba = propose(&store, &g, &ba, &tba, "op-ba").await.unwrap();
    // Both applies queue behind an owner holding both refs, then run concurrently.
    let (hold, pid) = hold_refs(&pool, &g, &["alpha", "beta"]).await;
    let spawn = |proposal: i64, token: String, key: &'static str| {
        let (s, g) = (store.clone(), g.clone());
        tokio::spawn(async move { apply(&s, &g, proposal, &token, "reviewer", key).await })
    };
    let a = spawn(pab.proposal_id, tab.clone(), "op-ab-apply");
    until_waiting(&pool, pid, 1).await;
    let b = spawn(pba.proposal_id, tba.clone(), "op-ba-apply");
    until_waiting(&pool, pid, 2).await;
    hold.commit().await.unwrap();
    let results = [a.await.unwrap(), b.await.unwrap()];
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 1, "{results:?}");
    assert!(
        results
            .iter()
            .any(|r| matches!(r, Err(LedgerError::MergeStale(_)))),
        "{results:?}"
    );
    // Exactly one merge event across both branches; the loser's branch did not move.
    let merges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ref_events WHERE graph_id = $1 AND operation = 'merge'",
    )
    .bind(g.as_str())
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(merges, 1);
    let versions = (
        head(&store, &g, "alpha").await.1,
        head(&store, &g, "beta").await.1,
    );
    assert!(versions == (2, 3) || versions == (3, 2), "{versions:?}");
    // No criss-cross: the next merge between them has a unique base (or is contained).
    let next = preview(&store, &g, &ab).await.class;
    assert!(
        !matches!(next, MergeClass::AmbiguousMergeBase(_)),
        "{next:?}"
    );
    verify_clean(&store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn apply_racing_a_target_acceptance_or_itself_moves_the_target_once() {
    let store = Arc::new(store().await);
    let pool = store.pool().clone();
    for apply_first in [true, false] {
        let g = graph(&store).await;
        let c1 = change(&store, &g, "main", None, &[A], &[]).await;
        branch(&store, &g, "agent/r", BranchPolicy::default()).await;
        change(&store, &g, "agent/r", Some(c1.clone()), &[B], &[]).await;
        let sp = spec("agent/r", "main", MergeStrategy::Abort);
        let token = preview(&store, &g, &sp).await.preview_token.unwrap();
        let pr = propose(&store, &g, &sp, &token, "r-propose").await.unwrap();
        // A competing ordinary candidate on main.
        let competing = store
            .workflows()
            .prepare(&PrepareRequest {
                scope: scope(&g, "r-prep"),
                branch: "main".into(),
                expected_head: Some(c1.clone()),
                requested: Patch::new([Operation {
                    kind: OperationKind::Add,
                    quad: q("<urn:r> <urn:p> \"1\" ."),
                }])
                .unwrap(),
                activity: "cognitive-correction".into(),
                event_time: None,
                evidence_refs: vec![],
                source_system: None,
                message: "competing".into(),
            })
            .await
            .unwrap()
            .candidate;
        let (hold, pid) = hold_refs(&pool, &g, &["main"]).await;
        let merge = {
            let (s, g, t) = (store.clone(), g.clone(), token.clone());
            move || {
                tokio::spawn(async move {
                    apply(&s, &g, pr.proposal_id, &t, "reviewer", "r-apply").await
                })
            }
        };
        let accept = {
            let (s, g, c, h) = (store.clone(), g.clone(), competing.clone(), c1.clone());
            move || {
                tokio::spawn(async move {
                    s.workflows()
                        .accept(&AcceptRequest {
                            scope: scope(&g, "r-accept"),
                            branch: "main".into(),
                            expected_head: Some(h),
                            candidate: c,
                            reason: None,
                            validation: ValidationPolicy::NoValidation,
                        })
                        .await
                })
            }
        };
        let (m, a) = if apply_first {
            let m = merge();
            until_waiting(&pool, pid, 1).await;
            let a = accept();
            until_waiting(&pool, pid, 2).await;
            (m, a)
        } else {
            let a = accept();
            until_waiting(&pool, pid, 1).await;
            let m = merge();
            until_waiting(&pool, pid, 2).await;
            (m, a)
        };
        hold.commit().await.unwrap();
        let (m, a) = (m.await.unwrap(), a.await.unwrap());
        let (head_now, version) = head(&store, &g, "main").await;
        assert_eq!(version, 2, "exactly one movement");
        if apply_first {
            assert_eq!(m.unwrap().head, head_now);
            assert!(matches!(a, Err(LedgerError::HeadChanged { .. })), "{a:?}");
        } else {
            a.unwrap();
            assert_eq!(head_now, competing);
            assert!(matches!(m, Err(LedgerError::MergeStale(_))), "{m:?}");
        }
        // The same proposal applied twice (two keys) across "replicas": one wins.
        if apply_first {
            let again = apply(
                &store,
                &g,
                pr.proposal_id,
                &token,
                "reviewer",
                "r-apply-other",
            )
            .await;
            assert!(again.is_err());
        }
        verify_clean(&store).await;
    }
}

// ---- strategies applied, recorded resolutions, policy, staleness (review follow-ups) --------

/// A fresh graph with main C1 = {x (slot s), b}, a branch, and the conflicting changes main
/// X->Y and branch X->Z (plus the branch deleting b); returns (graph, c1).
async fn conflicting(store: &PostgresLedgerStore, name: &str) -> (GraphId, CommitId) {
    let g = graph(store).await;
    let x = "<urn:s> <urn:p> \"X\" .";
    let c1 = change(store, &g, "main", None, &[x, B], &[]).await;
    branch(store, &g, name, BranchPolicy::default()).await;
    change(
        store,
        &g,
        "main",
        Some(c1.clone()),
        &["<urn:s> <urn:p> \"Y\" ."],
        &[x],
    )
    .await;
    change(
        store,
        &g,
        name,
        Some(c1.clone()),
        &["<urn:s> <urn:p> \"Z\" ."],
        &[x, B],
    )
    .await;
    (g, c1)
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn every_strategy_persists_applies_and_reconstructs_exactly() {
    let store = store().await;
    for (strategy, slot) in [
        (MergeStrategy::TakeTarget, vec!["<urn:s> <urn:p> \"Y\" ."]),
        (MergeStrategy::TakeSource, vec!["<urn:s> <urn:p> \"Z\" ."]),
        (
            MergeStrategy::Union,
            vec!["<urn:s> <urn:p> \"Y\" .", "<urn:s> <urn:p> \"Z\" ."],
        ),
    ] {
        let (g, _) = conflicting(&store, "agent/st").await;
        let sp = spec("agent/st", "main", strategy);
        let p = preview(&store, &g, &sp).await;
        let token = p.preview_token.clone().unwrap();
        let pr = propose(&store, &g, &sp, &token, "s-propose").await.unwrap();
        assert_eq!(
            (pr.strategy.as_str(), pr.conflict_count),
            (strategy.as_str(), 1)
        );
        apply(&store, &g, pr.proposal_id, &token, "reviewer", "s-apply")
            .await
            .unwrap();
        let (main, _) = head(&store, &g, "main").await;
        // B was deleted by the source only (main kept it): it goes in every strategy.
        let want: BTreeSet<Quad> = slot.iter().map(|s| q(s)).collect();
        assert_eq!(state(&store, &main).await, want, "{strategy:?}");
        assert_eq!(
            ledger_rdf::state_digest(&want),
            p.merged_state_digest.unwrap(),
            "{strategy:?}"
        );
        verify_clean(&store).await;
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_resolution_that_keeps_the_target_state_is_still_recorded_and_never_reapplied() {
    // Base k={a}; target k={b}; source k={c}. take-target keeps the target state: an empty
    // integration commit records the resolution (it is not NO_CHANGE: the source changed).
    let store = store().await;
    let g = graph(&store).await;
    let a = "<urn:k> <urn:p> \"a\" .";
    let c1 = change(&store, &g, "main", None, &[a], &[]).await;
    branch(&store, &g, "agent/rs", BranchPolicy::default()).await;
    let t2 = change(
        &store,
        &g,
        "main",
        Some(c1.clone()),
        &["<urn:k> <urn:p> \"b\" ."],
        &[a],
    )
    .await;
    change(
        &store,
        &g,
        "agent/rs",
        Some(c1.clone()),
        &["<urn:k> <urn:p> \"c\" ."],
        &[a],
    )
    .await;
    let sp = spec("agent/rs", "main", MergeStrategy::TakeTarget);
    let p = preview(&store, &g, &sp).await;
    assert_eq!(p.class, MergeClass::Divergent);
    let token = p.preview_token.unwrap();
    let pr = propose(&store, &g, &sp, &token, "rs-propose")
        .await
        .unwrap();
    apply(&store, &g, pr.proposal_id, &token, "reviewer", "rs-apply")
        .await
        .unwrap();
    let (main, _) = head(&store, &g, "main").await;
    assert_eq!(state(&store, &main).await, state(&store, &t2).await);
    // The target later goes back to {a}: the old source change is NOT silently reapplied —
    // the source is contained now.
    change(
        &store,
        &g,
        "main",
        Some(main.clone()),
        &[a],
        &["<urn:k> <urn:p> \"b\" ."],
    )
    .await;
    assert_eq!(
        preview(&store, &g, &spec("agent/rs", "main", MergeStrategy::Abort))
            .await
            .class,
        MergeClass::AlreadyContained
    );
    // A source with no net change from the base is NO_CHANGE and creates nothing.
    let g2 = graph(&store).await;
    let d1 = change(&store, &g2, "main", None, &[a], &[]).await;
    branch(&store, &g2, "agent/noop", BranchPolicy::default()).await;
    let n1 = change(&store, &g2, "agent/noop", Some(d1.clone()), &[B], &[]).await;
    change(&store, &g2, "agent/noop", Some(n1), &[], &[B]).await;
    change(
        &store,
        &g2,
        "main",
        Some(d1),
        &["<urn:m> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    assert_eq!(
        preview(
            &store,
            &g2,
            &spec("agent/noop", "main", MergeStrategy::Abort)
        )
        .await
        .class,
        MergeClass::NoChange
    );
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn delete_versus_modify_of_one_slot_conflicts() {
    let store = store().await;
    let g = graph(&store).await;
    let t = "<urn:t> <urn:p> \"X\" .";
    let c1 = change(&store, &g, "main", None, &[t, A], &[]).await;
    branch(&store, &g, "agent/dm", BranchPolicy::default()).await;
    change(&store, &g, "main", Some(c1.clone()), &[], &[t]).await;
    change(
        &store,
        &g,
        "agent/dm",
        Some(c1.clone()),
        &["<urn:t> <urn:p> \"W\" ."],
        &[t],
    )
    .await;
    let p = preview(&store, &g, &spec("agent/dm", "main", MergeStrategy::Abort)).await;
    assert_eq!((p.class, p.conflict_count), (MergeClass::Conflicted, 1));
    let c = &p.conflicts[0];
    assert_eq!(
        (
            c.key.subject.as_str(),
            c.key.predicate.as_str(),
            c.key.graph.as_ref()
        ),
        ("<urn:t>", "<urn:p>", None)
    );
    assert_eq!(
        (
            c.base.quads.clone(),
            c.target.quads.clone(),
            c.source.quads.clone()
        ),
        (vec![q(t)], vec![], vec![q("<urn:t> <urn:p> \"W\" .")])
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn distinct_reviewer_on_the_target_excludes_the_merge_proposer_and_the_source_authors() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    store
        .workflows()
        .create_branch(
            &CreateBranchRequest {
                scope: scope(&g, "cb-four"),
                name: "four-eyes".into(),
                source: "main".into(),
                from_commit: None,
                policy: BranchPolicy {
                    protected: false,
                    require_validation: false,
                    require_distinct_reviewer: true,
                },
            },
            limits(),
        )
        .await
        .unwrap();
    branch(&store, &g, "agent/lax", BranchPolicy::default()).await;
    // The content on the lax source is proposed (and self-accepted) by "curator".
    change(&store, &g, "agent/lax", Some(c1.clone()), &[B], &[]).await;
    let sp = spec("agent/lax", "four-eyes", MergeStrategy::Abort);
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    // The merge itself is proposed by "mechanic".
    let pr = store
        .workflows()
        .merge_propose(
            &ProposeMergeRequest {
                scope: scope_as("mechanic", &g, "fe-propose", "fe-propose"),
                spec: sp.clone(),
                preview_token: token.clone(),
                message: String::new(),
                evidence_refs: vec![],
            },
            limits(),
        )
        .await
        .unwrap();
    for who in ["mechanic", "curator"] {
        let r = apply(
            &store,
            &g,
            pr.proposal_id,
            &token,
            who,
            &format!("fe-{who}"),
        )
        .await;
        assert!(
            matches!(r, Err(LedgerError::BranchPolicyViolation(_))),
            "{who}: {r:?}"
        );
    }
    apply(
        &store,
        &g,
        pr.proposal_id,
        &token,
        "reviewer",
        "fe-reviewer",
    )
    .await
    .unwrap();
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_rejected_merge_can_be_proposed_again_and_duplicate_proposals_are_clean() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "agent/rj", BranchPolicy::default()).await;
    change(&store, &g, "agent/rj", Some(c1.clone()), &[B], &[]).await;
    let sp = spec("agent/rj", "main", MergeStrategy::Abort);
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    let first = propose(&store, &g, &sp, &token, "rj-1").await.unwrap();
    store
        .workflows()
        .reject(&ledger_store::RejectRequest {
            scope: scope_as("reviewer", &g, "rj-reject", "rj-reject"),
            branch: "main".into(),
            candidate: first.candidate.clone(),
            reason: "not now".into(),
            validation_id: None,
        })
        .await
        .unwrap();
    // The rejected proposal names its decision; the same preview can be proposed again.
    let r = apply(
        &store,
        &g,
        first.proposal_id,
        &token,
        "reviewer",
        "rj-apply-1",
    )
    .await;
    assert!(
        matches!(&r, Err(LedgerError::LineageMismatch(m)) if m.contains("terminal decision (rejected")),
        "{r:?}"
    );
    let second = propose(&store, &g, &sp, &token, "rj-2").await.unwrap();
    let third = propose(&store, &g, &sp, &token, "rj-3").await.unwrap();
    assert_ne!(second.candidate, third.candidate);
    assert_eq!(second.preview_token, third.preview_token);
    apply(
        &store,
        &g,
        second.proposal_id,
        &token,
        "reviewer",
        "rj-apply-2",
    )
    .await
    .unwrap();
    assert!(matches!(
        apply(
            &store,
            &g,
            third.proposal_id,
            &token,
            "reviewer",
            "rj-apply-3"
        )
        .await,
        Err(LedgerError::MergeStale(_))
    ));
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn refused_merges_write_nothing_and_deleted_branches_refuse() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "agent/rf", BranchPolicy::default()).await;
    branch(&store, &g, "doomed", BranchPolicy::default()).await;
    let s1 = change(&store, &g, "agent/rf", Some(c1.clone()), &[B], &[]).await;
    let sp = spec("agent/rf", "main", MergeStrategy::Abort);
    // Preview, then the source moves: propose is stale and writes nothing.
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    change(
        &store,
        &g,
        "agent/rf",
        Some(s1),
        &["<urn:q> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    let before = counts(&store, &g).await;
    assert!(matches!(
        propose(&store, &g, &sp, &token, "rf-1").await,
        Err(LedgerError::MergeStale(_))
    ));
    assert_eq!(counts(&store, &g).await, before);
    // Equal heads, same branch, unknown branch, a base where nothing is merged.
    let eq = preview(&store, &g, &spec("doomed", "main", MergeStrategy::Abort)).await;
    assert_eq!(eq.class, MergeClass::AlreadyEqual);
    assert!(matches!(
        store
            .workflows()
            .merge_preview(
                &tenant(),
                &g,
                &spec("main", "main", MergeStrategy::Abort),
                limits()
            )
            .await,
        Err(LedgerError::InvalidIdentifier { .. })
    ));
    assert!(matches!(
        store
            .workflows()
            .merge_preview(
                &tenant(),
                &g,
                &spec("nope", "main", MergeStrategy::Abort),
                limits()
            )
            .await,
        Err(LedgerError::BranchNotFound(_))
    ));
    let with_base = MergeSpec {
        base: Some(c1.clone()),
        ..spec("doomed", "main", MergeStrategy::Abort)
    };
    assert!(matches!(
        store
            .workflows()
            .merge_preview(&tenant(), &g, &with_base, limits())
            .await,
        Err(LedgerError::InvalidMergeBase(_))
    ));
    // Another tenant: unknown graph.
    assert!(matches!(
        store
            .workflows()
            .merge_preview(&TenantId::new("tenant-b").unwrap(), &g, &sp, limits())
            .await,
        Err(LedgerError::UnknownGraph(_))
    ));
    // Propose, then the TARGET is deleted: apply is refused as deleted.
    let doomed = spec("agent/rf", "doomed", MergeStrategy::Abort);
    let token = preview(&store, &g, &doomed).await.preview_token.unwrap();
    let pr = propose(&store, &g, &doomed, &token, "rf-2").await.unwrap();
    store
        .workflows()
        .delete_branch(&BranchLifecycleRequest {
            scope: scope(&g, "del-doomed"),
            name: "doomed".into(),
            reason: None,
        })
        .await
        .unwrap();
    let before = counts(&store, &g).await;
    assert!(matches!(
        apply(&store, &g, pr.proposal_id, &token, "reviewer", "rf-apply").await,
        Err(LedgerError::BranchDeleted(_))
    ));
    assert_eq!(counts(&store, &g).await, before);
    // …and a preview onto a deleted target is refused as well.
    assert!(matches!(
        store
            .workflows()
            .merge_preview(&tenant(), &g, &doomed, limits())
            .await,
        Err(LedgerError::BranchDeleted(_))
    ));
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_merge_writes_exactly_one_ordinary_outbox_row_and_branch_merges_are_not_backlog() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "agent/ob", BranchPolicy::default()).await;
    branch(&store, &g, "side", BranchPolicy::default()).await;
    change(&store, &g, "agent/ob", Some(c1.clone()), &[B], &[]).await;
    for target in ["main", "side"] {
        let sp = spec("agent/ob", target, MergeStrategy::Abort);
        let token = preview(&store, &g, &sp).await.preview_token.unwrap();
        let pr = propose(&store, &g, &sp, &token, &format!("ob-{target}"))
            .await
            .unwrap();
        let ap = apply(
            &store,
            &g,
            pr.proposal_id,
            &token,
            "reviewer",
            &format!("oba-{target}"),
        )
        .await
        .unwrap();
        let row: (String, String, i64, String) = sqlx::query_as(
            "SELECT branch, commit_id, ref_version, event_kind FROM projection_outbox WHERE outbox_id = $1",
        )
        .bind(ap.outbox_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                target.to_owned(),
                pr.candidate.to_string(),
                ap.ref_version,
                "ref_advanced".to_owned()
            )
        );
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM projection_outbox WHERE ref_event_id = $1")
                .bind(ap.ref_event_id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(rows, 1);
    }
    // The non-main merge's row is not an (unconfigured) projection backlog.
    let side_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM projection_outbox WHERE graph_id = $1 AND branch = 'side' AND delivered_at IS NULL",
    )
    .bind(g.as_str())
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(side_rows, 1);
    // Both global readings in one snapshot: other tests write outbox rows concurrently.
    let mut snapshot = store.pool().begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *snapshot)
        .await
        .unwrap();
    let unconfigured_main: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM projection_outbox WHERE delivered_at IS NULL AND branch = 'main' \
         AND NOT EXISTS (SELECT 1 FROM projection_state s WHERE s.graph_id = projection_outbox.graph_id \
         AND s.branch = projection_outbox.branch AND s.status <> 'disabled')",
    )
    .fetch_one(&mut *snapshot)
    .await
    .unwrap();
    assert_eq!(
        ledger_store::ProjectionRepository::unconfigured_pending_on(&mut snapshot)
            .await
            .unwrap(),
        unconfigured_main
    );
    snapshot.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_crash_at_any_merge_stage_leaves_nothing_and_the_retry_succeeds() {
    use ledger_store::FailPoint;
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "agent/fp", BranchPolicy::default()).await;
    change(&store, &g, "agent/fp", Some(c1.clone()), &[B], &[]).await;
    let sp = spec("agent/fp", "main", MergeStrategy::Abort);
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    let request = |key: &str| ProposeMergeRequest {
        scope: scope(&g, key),
        spec: sp.clone(),
        preview_token: token.clone(),
        message: "integrate".into(),
        evidence_refs: vec![],
    };
    for point in [FailPoint::AfterDecision, FailPoint::BeforeCommit] {
        let before = counts(&store, &g).await;
        let failing = store.workflows().clone().with_failpoint(point);
        let err = failing
            .merge_propose(&request("fp-p"), limits())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, LedgerError::Storage(m) if m == &format!("injected failure at {point:?}")),
            "{point:?}: {err:?}"
        );
        assert_eq!(counts(&store, &g).await, before, "{point:?}");
    }
    let pr = store
        .workflows()
        .merge_propose(&request("fp-p"), limits())
        .await
        .unwrap();
    let apply_request = || ApplyMergeRequest {
        scope: scope_as("reviewer", &g, "fp-a", "fp-a"),
        proposal_id: pr.proposal_id,
        preview_token: token.clone(),
        reason: None,
        validation: ValidationPolicy::NoValidation,
    };
    for point in [
        FailPoint::AfterRefUpdate,
        FailPoint::AfterRefEvent,
        FailPoint::AfterDecision,
        FailPoint::AfterOutbox,
        FailPoint::BeforeCommit,
    ] {
        let before = counts(&store, &g).await;
        let failing = store.workflows().clone().with_failpoint(point);
        let err = failing.merge_apply(&apply_request()).await.unwrap_err();
        assert!(
            matches!(&err, LedgerError::Storage(m) if m == &format!("injected failure at {point:?}")),
            "{point:?}: {err:?}"
        );
        assert_eq!(counts(&store, &g).await, before, "{point:?}");
        assert_eq!(head(&store, &g, "main").await, (c1.clone(), 1), "{point:?}");
    }
    let ap = store
        .workflows()
        .merge_apply(&apply_request())
        .await
        .unwrap();
    assert_eq!(ap.head, pr.candidate);
    verify_clean(&store).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn the_database_refuses_raw_merge_writes() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "agent/db", BranchPolicy::default()).await;
    let s1 = change(&store, &g, "agent/db", Some(c1.clone()), &[B], &[]).await;
    let sp = spec("agent/db", "main", MergeStrategy::Abort);
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    let pr = propose(&store, &g, &sp, &token, "db-propose")
        .await
        .unwrap();
    let pool = store.pool();
    let refused = |sql: String| async move {
        match sqlx::query(&sql).execute(pool).await {
            Err(sqlx::Error::Database(d)) => {
                format!("{} {}", d.code().unwrap_or_default(), d.message())
            }
            other => panic!("{sql} must be refused: {other:?}"),
        }
    };
    // Write-once merge rows.
    assert!(
        refused(format!(
            "UPDATE merge_proposals SET strategy = 'union' WHERE proposal_id = {}",
            pr.proposal_id
        ))
        .await
        .starts_with("23000")
    );
    assert!(
        refused(format!(
            "DELETE FROM merge_proposals WHERE proposal_id = {}",
            pr.proposal_id
        ))
        .await
        .starts_with("23000")
    );
    // A merge row for an ordinary (single-parent) candidate is not an integration commit.
    let ordinary = store
        .workflows()
        .prepare(&PrepareRequest {
            scope: scope(&g, "db-ord"),
            branch: "main".into(),
            expected_head: Some(c1.clone()),
            requested: Patch::new([Operation {
                kind: OperationKind::Add,
                quad: q("<urn:o> <urn:p> \"1\" ."),
            }])
            .unwrap(),
            activity: "cognitive-correction".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: "ordinary".into(),
        })
        .await
        .unwrap();
    let prop_id: i64 =
        sqlx::query_scalar("SELECT proposal_id FROM proposals WHERE candidate_commit = $1")
            .bind(ordinary.candidate.to_string())
            .fetch_one(pool)
            .await
            .unwrap();
    let m = refused(format!(
        "INSERT INTO merge_proposals (proposal_id, graph_id, target_branch, candidate_commit, target_head, source_branch, \
         source_head, merge_base, base_explicit, classification, strategy, merge_algorithm, conflict_count, \
         merged_state_digest, preview_token, source_parties) VALUES ({prop_id}, '{g}', 'main', '{}', '{c1}', 'agent/db', \
         '{s1}', '{c1}', false, 'fast_forward', 'abort', 'structural-slot/v1', 0, 'sha256:{z}', 'sha256:{z}', '{{}}')",
        ordinary.candidate,
        z = "0".repeat(64)
    ))
    .await;
    assert!(m.contains("is not the integration commit"), "{m}");
    // An accepted decision on a merge candidate must reference a merge event.
    let advance_event: i64 = sqlx::query_scalar(
        "SELECT event_id FROM ref_events WHERE graph_id = $1 AND branch = 'main' AND new_version = 1",
    )
    .bind(g.as_str())
    .fetch_one(pool)
    .await
    .unwrap();
    let m = refused(format!(
        "INSERT INTO decisions (proposal_id, graph_id, branch, candidate_commit, decision, tenant_id, principal_id, \
         principal_type, validation_ids, ref_event_id) VALUES ({}, '{g}', 'main', '{}', 'accepted', 'tenant-a', \
         'urn:it:raw', 'agent', '{{}}', {advance_event})",
        pr.proposal_id, pr.candidate
    ))
    .await;
    assert!(m.contains("must be accepted through a merge event"), "{m}");
    // The idempotency shape binds merge operations to their result kinds.
    let m = refused(format!(
        "INSERT INTO idempotency (tenant_id, principal_id, principal_type, graph_id, operation, idempotency_key, \
         request_digest, result_kind, result_commit) VALUES ('tenant-a', 'urn:it:raw', 'agent', '{g}', \
         'merge_propose', 'raw-k', 'sha256:{z}', 'accepted', '{c1}')",
        z = "0".repeat(64)
    ))
    .await;
    assert!(m.starts_with("23514"), "{m}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn an_explicit_base_resolves_a_criss_cross_and_is_recorded() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    for b in ["x", "y"] {
        branch(&store, &g, b, BranchPolicy::default()).await;
    }
    let x1 = change(
        &store,
        &g,
        "x",
        Some(c1.clone()),
        &["<urn:x> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    change(
        &store,
        &g,
        "y",
        Some(c1.clone()),
        &["<urn:y> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    let run = |src: &'static str, tgt: &'static str, key: &'static str| {
        let (store, g) = (&store, &g);
        async move {
            let sp = spec(src, tgt, MergeStrategy::Abort);
            let t = preview(store, g, &sp).await.preview_token.unwrap();
            let pr = propose(store, g, &sp, &t, key).await.unwrap();
            apply(
                store,
                g,
                pr.proposal_id,
                &t,
                "reviewer",
                &format!("{key}-a"),
            )
            .await
            .unwrap();
        }
    };
    run("y", "x", "cx-1").await;
    store
        .workflows()
        .create_branch(
            &CreateBranchRequest {
                scope: scope(&g, "cb-w2"),
                name: "w".into(),
                source: "x".into(),
                from_commit: Some(x1.clone()),
                policy: BranchPolicy::default(),
            },
            limits(),
        )
        .await
        .unwrap();
    run("w", "y", "cx-2").await;
    // Make the two sides differ again so the merge is not NO_CHANGE.
    let (yh, _) = head(&store, &g, "y").await;
    change(
        &store,
        &g,
        "y",
        Some(yh),
        &["<urn:y2> <urn:p> \"2\" ."],
        &[],
    )
    .await;
    assert!(matches!(
        preview(&store, &g, &spec("y", "x", MergeStrategy::Abort))
            .await
            .class,
        MergeClass::AmbiguousMergeBase(_)
    ));
    let explicit = MergeSpec {
        base: Some(x1.clone()),
        ..spec("y", "x", MergeStrategy::Abort)
    };
    let p = preview(&store, &g, &explicit).await;
    assert_eq!(
        (p.class.clone(), p.base_explicit),
        (MergeClass::Divergent, true)
    );
    let t = p.preview_token.unwrap();
    let pr = propose(&store, &g, &explicit, &t, "cx-3").await.unwrap();
    apply(&store, &g, pr.proposal_id, &t, "reviewer", "cx-3a")
        .await
        .unwrap();
    let recorded: (String, bool) = sqlx::query_as(
        "SELECT merge_base, base_explicit FROM merge_proposals WHERE proposal_id = $1",
    )
    .bind(pr.proposal_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(recorded, (x1.to_string(), true));
    verify_clean(&store).await;
}

// ---- forced races (ADR-0024 concurrency list) -------------------------------------------------

/// An owner transaction holding the branch rows of `branches` exclusively.
async fn hold_branch_rows(
    pool: &sqlx::PgPool,
    g: &GraphId,
    branches: &[&str],
) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
    let mut tx = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    for b in branches {
        sqlx::query("SELECT 1 FROM branches WHERE graph_id = $1 AND branch = $2 FOR UPDATE")
            .bind(g.as_str())
            .bind(*b)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    (tx, pid)
}

/// A proposed merge `source -> target` on a fresh graph (target = main at C1, source one
/// commit ahead): (graph, c1, source head, proposal id, token).
async fn proposed_merge(
    store: &PostgresLedgerStore,
    source: &str,
) -> (GraphId, CommitId, CommitId, i64, String) {
    let g = graph(store).await;
    let c1 = change(store, &g, "main", None, &[A], &[]).await;
    branch(store, &g, source, BranchPolicy::default()).await;
    let s1 = change(store, &g, source, Some(c1.clone()), &[B], &[]).await;
    let sp = spec(source, "main", MergeStrategy::Abort);
    let token = preview(store, &g, &sp).await.preview_token.unwrap();
    let pr = propose(store, &g, &sp, &token, "race-propose")
        .await
        .unwrap();
    (g, c1, s1, pr.proposal_id, token)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn apply_racing_a_source_acceptance_integrates_the_head_it_locked() {
    let store = Arc::new(store().await);
    let pool = store.pool().clone();
    for apply_first in [true, false] {
        let (g, _c1, s1, proposal, token) = proposed_merge(&store, "src").await;
        let next = store
            .workflows()
            .prepare(&PrepareRequest {
                scope: scope(&g, "sa-prep"),
                branch: "src".into(),
                expected_head: Some(s1.clone()),
                requested: Patch::new([Operation {
                    kind: OperationKind::Add,
                    quad: q("<urn:sa> <urn:p> \"1\" ."),
                }])
                .unwrap(),
                activity: "cognitive-correction".into(),
                event_time: None,
                evidence_refs: vec![],
                source_system: None,
                message: "source moves".into(),
            })
            .await
            .unwrap()
            .candidate;
        let (hold, pid) = hold_refs(&pool, &g, &["src"]).await;
        let merge = {
            let (s, g, t) = (store.clone(), g.clone(), token.clone());
            move || {
                tokio::spawn(
                    async move { apply(&s, &g, proposal, &t, "reviewer", "sa-apply").await },
                )
            }
        };
        let accept = {
            let (s, g, c, h) = (store.clone(), g.clone(), next.clone(), s1.clone());
            move || {
                tokio::spawn(async move {
                    s.workflows()
                        .accept(&AcceptRequest {
                            scope: scope(&g, "sa-accept"),
                            branch: "src".into(),
                            expected_head: Some(h),
                            candidate: c,
                            reason: None,
                            validation: ValidationPolicy::NoValidation,
                        })
                        .await
                })
            }
        };
        let (m, a) = if apply_first {
            let m = merge();
            until_waiting(&pool, pid, 1).await;
            let a = accept();
            until_waiting(&pool, pid, 2).await;
            (m, a)
        } else {
            let a = accept();
            until_waiting(&pool, pid, 1).await;
            let m = merge();
            until_waiting(&pool, pid, 2).await;
            (m, a)
        };
        hold.commit().await.unwrap();
        let (m, a) = (m.await.unwrap(), a.await.unwrap());
        // The source acceptance always lands (a merge only reads its source).
        a.unwrap();
        if apply_first {
            let applied = m.unwrap();
            let parents: Vec<String> = sqlx::query_scalar(
                "SELECT parent_id FROM commit_parents WHERE commit_id = $1 ORDER BY position",
            )
            .bind(applied.head.to_string())
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(parents[1], s1.to_string(), "integrated the head it locked");
        } else {
            assert!(matches!(m, Err(LedgerError::MergeStale(_))), "{m:?}");
        }
        verify_clean(&store).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn two_applies_of_one_proposal_land_once() {
    let store = Arc::new(store().await);
    let pool = store.pool().clone();
    let (g, c1, _s1, proposal, token) = proposed_merge(&store, "twice").await;
    let (hold, pid) = hold_refs(&pool, &g, &["main"]).await;
    let spawn = |key: &'static str| {
        let (s, g, t) = (store.clone(), g.clone(), token.clone());
        tokio::spawn(async move { apply(&s, &g, proposal, &t, "reviewer", key).await })
    };
    let a = spawn("tw-1");
    until_waiting(&pool, pid, 1).await;
    let b = spawn("tw-2");
    until_waiting(&pool, pid, 2).await;
    hold.commit().await.unwrap();
    let results = [a.await.unwrap(), b.await.unwrap()];
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(
        results.iter().any(
            |r| matches!(r, Err(LedgerError::LineageMismatch(m)) if m.contains("terminal decision"))
        ),
        "{results:?}"
    );
    assert_eq!(head(&store, &g, "main").await.1, 2);
    let _ = c1;
    verify_clean(&store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn apply_racing_the_deletion_of_its_target_never_lands_on_a_tombstone() {
    let store = Arc::new(store().await);
    let pool = store.pool().clone();
    for apply_first in [true, false] {
        let g = graph(&store).await;
        let c1 = change(&store, &g, "main", None, &[A], &[]).await;
        branch(&store, &g, "tgt", BranchPolicy::default()).await;
        branch(&store, &g, "src", BranchPolicy::default()).await;
        change(&store, &g, "src", Some(c1.clone()), &[B], &[]).await;
        let sp = spec("src", "tgt", MergeStrategy::Abort);
        let token = preview(&store, &g, &sp).await.preview_token.unwrap();
        let pr = propose(&store, &g, &sp, &token, "td-propose")
            .await
            .unwrap();
        let (hold, pid) = hold_branch_rows(&pool, &g, &["tgt"]).await;
        let merge = {
            let (s, g, t) = (store.clone(), g.clone(), token.clone());
            move || {
                tokio::spawn(async move {
                    apply(&s, &g, pr.proposal_id, &t, "reviewer", "td-apply").await
                })
            }
        };
        let delete = {
            let (s, g) = (store.clone(), g.clone());
            move || {
                tokio::spawn(async move {
                    s.workflows()
                        .delete_branch(&BranchLifecycleRequest {
                            scope: scope(&g, "td-delete"),
                            name: "tgt".into(),
                            reason: None,
                        })
                        .await
                })
            }
        };
        let (m, d) = if apply_first {
            let m = merge();
            until_waiting(&pool, pid, 1).await;
            let d = delete();
            until_waiting(&pool, pid, 2).await;
            (m, d)
        } else {
            let d = delete();
            until_waiting(&pool, pid, 1).await;
            let m = merge();
            until_waiting(&pool, pid, 2).await;
            (m, d)
        };
        hold.commit().await.unwrap();
        let (m, d) = (m.await.unwrap(), d.await.unwrap().unwrap());
        let (tgt_head, version) = head(&store, &g, "tgt").await;
        if apply_first {
            assert_eq!(m.unwrap().head, tgt_head);
            assert_eq!(
                (d.event.head, version),
                (tgt_head, 2),
                "tombstone at the merge head"
            );
        } else {
            assert!(matches!(m, Err(LedgerError::BranchDeleted(_))), "{m:?}");
            assert_eq!((tgt_head, version), (c1.clone(), 1));
        }
        verify_clean(&store).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_three_branch_merge_ring_serializes_without_deadlock_or_criss_cross() {
    let store = Arc::new(store().await);
    let pool = store.pool().clone();
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    for b in ["ra", "rb", "rc"] {
        branch(&store, &g, b, BranchPolicy::default()).await;
        change(
            &store,
            &g,
            b,
            Some(c1.clone()),
            &[&format!("<urn:{b}> <urn:p> \"1\" .")],
            &[],
        )
        .await;
    }
    let mut proposals = Vec::new();
    for (src, tgt) in [("rb", "ra"), ("rc", "rb"), ("ra", "rc")] {
        let sp = spec(src, tgt, MergeStrategy::Abort);
        let t = preview(&store, &g, &sp).await.preview_token.unwrap();
        let pr = propose(&store, &g, &sp, &t, &format!("ring-{src}-{tgt}"))
            .await
            .unwrap();
        proposals.push((pr.proposal_id, t));
    }
    let (hold, pid) = hold_refs(&pool, &g, &["ra", "rb", "rc"]).await;
    let mut tasks = Vec::new();
    for (n, (p, t)) in proposals.into_iter().enumerate() {
        let (s, g) = (store.clone(), g.clone());
        tasks.push(tokio::spawn(async move {
            apply(&s, &g, p, &t, "reviewer", &format!("ring-a{n}")).await
        }));
        until_waiting(&pool, pid, n as i64 + 1).await;
    }
    hold.commit().await.unwrap();
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(_) => ok += 1,
            Err(LedgerError::MergeStale(_)) => {}
            Err(e) => panic!("unexpected ring outcome {e}"),
        }
    }
    assert!((1..=2).contains(&ok), "{ok}");
    for (a, b) in [("ra", "rb"), ("rb", "rc"), ("rc", "ra")] {
        let class = preview(&store, &g, &spec(a, b, MergeStrategy::Abort))
            .await
            .class;
        assert!(
            !matches!(class, MergeClass::AmbiguousMergeBase(_)),
            "{a}/{b}: {class:?}"
        );
    }
    verify_clean(&store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_propose_paused_while_the_target_moves_is_stale() {
    let store = Arc::new(store().await);
    let pool = store.pool().clone();
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    branch(&store, &g, "agent/pz", BranchPolicy::default()).await;
    change(&store, &g, "agent/pz", Some(c1.clone()), &[B], &[]).await;
    let sp = spec("agent/pz", "main", MergeStrategy::Abort);
    let token = preview(&store, &g, &sp).await.preview_token.unwrap();
    // Hold the propose request's own idempotency lock: it computes its preview lock-free and
    // then waits at the start of its transaction.
    let mut hold = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *hold)
        .await
        .unwrap();
    let key = format!(
        "tenant-a\u{1f}urn:it:curator\u{1f}agent\u{1f}\u{1f}{}\u{1f}merge_propose\u{1f}pz-propose",
        g.as_str()
    );
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ledger_store::lock_key(&format!("idempotency:{key}")))
        .execute(&mut *hold)
        .await
        .unwrap();
    let task = {
        let (s, g, sp, t) = (store.clone(), g.clone(), sp.clone(), token.clone());
        tokio::spawn(async move { propose(&s, &g, &sp, &t, "pz-propose").await })
    };
    until_waiting(&pool, pid, 1).await;
    change(
        &store,
        &g,
        "main",
        Some(c1.clone()),
        &["<urn:pz> <urn:p> \"1\" ."],
        &[],
    )
    .await;
    hold.commit().await.unwrap();
    assert!(matches!(
        task.await.unwrap(),
        Err(LedgerError::MergeStale(_))
    ));
    let merges: i64 =
        sqlx::query_scalar("SELECT count(*) FROM merge_proposals WHERE graph_id = $1")
            .bind(g.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(merges, 0);
}

/// The stored propose results of `key` in this graph: (request digest, proposal).
async fn stored_proposes(pool: &sqlx::PgPool, g: &GraphId, key: &str) -> Vec<(String, i64)> {
    sqlx::query_as(
        "SELECT request_digest, result_proposal_id FROM idempotency \
         WHERE graph_id = $1 AND operation = 'merge_propose' AND idempotency_key = $2",
    )
    .bind(g.as_str())
    .bind(key)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Forced ordering (feature `test-hooks`; no sleeps, no held locks): a retry R of a propose
/// pauses right after its first stored-result lookup found nothing; the original O then
/// proposes and the merge is applied; R resumes and its recomputation now sees the source
/// contained (and an explicit base no longer applicable). Completed durable replay must win
/// over the mutable recomputed state: R returns O's result, never `MERGE_NOTHING_TO_DO`,
/// `MERGE_STALE` or `INVALID_MERGE_BASE`; and R with the same key but another canonical
/// request gets `IDEMPOTENCY_CONFLICT` without disturbing O's stored result.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_propose_retry_paused_before_recomputation_replays_the_applied_original() {
    use ledger_store::test_hooks::{HookPoint, PauseHook};
    let store = store().await;
    let pool = store.pool().clone();
    // (explicit base, retry with the same canonical request)
    for (explicit_base, same_request) in [(false, true), (true, true), (false, false)] {
        let g = graph(&store).await;
        let c1 = change(&store, &g, "main", None, &[A], &[]).await;
        branch(&store, &g, "agent/rr", BranchPolicy::default()).await;
        change(&store, &g, "agent/rr", Some(c1.clone()), &[B], &[]).await;
        let mut sp = spec("agent/rr", "main", MergeStrategy::Abort);
        if explicit_base {
            // A fast-forward's only valid base is the target head; after the apply the
            // source is contained and naming a base is refused by a recomputation.
            sp.base = Some(c1.clone());
        }
        let p = preview(&store, &g, &sp).await;
        assert_eq!(p.class, MergeClass::FastForward);
        let token = p.preview_token.unwrap();
        let key = "rr-propose";
        let r_scope = if same_request {
            scope(&g, key)
        } else {
            scope_as("curator", &g, key, "another canonical request")
        };

        let hook = PauseHook::new(HookPoint::ProposeAfterReplayCheck);
        let paused = store.workflows().clone().with_pause_hook(hook.clone());
        let retry = {
            let request = ProposeMergeRequest {
                scope: r_scope,
                spec: sp.clone(),
                preview_token: token.clone(),
                message: "integrate".into(),
                evidence_refs: vec![],
            };
            tokio::spawn(async move { paused.merge_propose(&request, limits()).await })
        };
        // R has looked up the stored result (none) and is paused before recomputing.
        hook.reached().await;
        assert!(stored_proposes(&pool, &g, key).await.is_empty());

        // O completes, and its proposal is applied.
        let original = propose(&store, &g, &sp, &token, key).await.unwrap();
        assert!(!original.replayed);
        apply(
            &store,
            &g,
            original.proposal_id,
            &token,
            "reviewer",
            "rr-apply",
        )
        .await
        .unwrap();
        // The recomputation R is about to run would now refuse.
        let now = preview(&store, &g, &spec("agent/rr", "main", MergeStrategy::Abort)).await;
        assert_eq!(now.class, MergeClass::AlreadyContained);

        hook.resume();
        let outcome = retry.await.unwrap();
        if same_request {
            let replay = outcome.unwrap_or_else(|e| {
                panic!("explicit base {explicit_base}: the retry must replay, got {e:?}")
            });
            assert!(replay.replayed);
            assert_eq!(
                (
                    replay.proposal_id,
                    &replay.candidate,
                    &replay.preview_token,
                    &replay.merged_state_digest
                ),
                (
                    original.proposal_id,
                    &original.candidate,
                    &original.preview_token,
                    &original.merged_state_digest
                )
            );
        } else {
            assert!(
                matches!(outcome, Err(LedgerError::IdempotencyConflict)),
                "{outcome:?}"
            );
            // O's result still replays for O, unchanged.
            let again = propose(&store, &g, &sp, &token, key).await.unwrap();
            assert!(again.replayed && again.proposal_id == original.proposal_id);
        }
        // Exactly one durable result for the scope/key: O's.
        let stored = stored_proposes(&pool, &g, key).await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].1, original.proposal_id);
        assert_eq!(
            stored[0].0,
            ContentId::for_bytes(key.as_bytes()).to_string()
        );
        let merges: i64 =
            sqlx::query_scalar("SELECT count(*) FROM merge_proposals WHERE graph_id = $1")
                .bind(g.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(merges, 1);
    }
    verify_clean(&store).await;
}

/// Backends waiting for the advisory lock `key` (as `pg_advisory_xact_lock(bigint)` splits it).
async fn advisory_waiters(pool: &sqlx::PgPool, key: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
         AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Forced ordering: the original propose O is paused just before its COMMIT (everything
/// written, idempotency lock held); the target then moves, so a duplicate R of the same
/// key recomputes another token. R's refusal-path lookup must wait on O's idempotency lock
/// and then replay O's committed result (or report `IDEMPOTENCY_CONFLICT` for another
/// request), never answer `MERGE_STALE` while O is about to commit.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_duplicate_propose_racing_the_uncommitted_original_waits_and_replays() {
    use ledger_store::test_hooks::{HookPoint, PauseHook};
    let store = store().await;
    let pool = store.pool().clone();
    for same_request in [true, false] {
        let g = graph(&store).await;
        let c1 = change(&store, &g, "main", None, &[A], &[]).await;
        branch(&store, &g, "agent/ic", BranchPolicy::default()).await;
        change(&store, &g, "agent/ic", Some(c1.clone()), &[B], &[]).await;
        let sp = spec("agent/ic", "main", MergeStrategy::Abort);
        let token = preview(&store, &g, &sp).await.preview_token.unwrap();
        let key = "ic-propose";
        let lock = ledger_store::lock_key(&format!(
            "idempotency:tenant-a\u{1f}urn:it:curator\u{1f}agent\u{1f}\u{1f}{}\u{1f}merge_propose\u{1f}{key}",
            g.as_str()
        ));

        let hook = PauseHook::new(HookPoint::ProposeBeforeCommit);
        let original = {
            let paused = store.workflows().clone().with_pause_hook(hook.clone());
            let request = ProposeMergeRequest {
                scope: scope(&g, key),
                spec: sp.clone(),
                preview_token: token.clone(),
                message: "integrate".into(),
                evidence_refs: vec![],
            };
            tokio::spawn(async move { paused.merge_propose(&request, limits()).await })
        };
        hook.reached().await;
        // O holds its idempotency lock, uncommitted. The target moves (an ordinary accept
        // is compatible with O's share lock on the target branch row).
        change(
            &store,
            &g,
            "main",
            Some(c1.clone()),
            &["<urn:ic> <urn:p> \"1\" ."],
            &[],
        )
        .await;
        let duplicate = {
            let (store, request) = (
                store.workflows().clone(),
                ProposeMergeRequest {
                    scope: if same_request {
                        scope(&g, key)
                    } else {
                        scope_as("curator", &g, key, "another canonical request")
                    },
                    spec: sp.clone(),
                    preview_token: token.clone(),
                    message: "integrate".into(),
                    evidence_refs: vec![],
                },
            );
            tokio::spawn(async move { store.merge_propose(&request, limits()).await })
        };
        // R recomputes (stale token) and must now be waiting on O's idempotency lock; it
        // must not have answered while O is uncommitted.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while advisory_waiters(&pool, lock).await == 0 {
            assert!(
                !duplicate.is_finished(),
                "the duplicate answered while the original was uncommitted: {:?}",
                duplicate.await
            );
            assert!(
                std::time::Instant::now() < deadline,
                "duplicate never waited"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        hook.resume();
        let original = original.await.unwrap().unwrap();
        assert!(!original.replayed);
        let outcome = duplicate.await.unwrap();
        if same_request {
            let replay = outcome.unwrap();
            assert!(replay.replayed);
            assert_eq!(
                (replay.proposal_id, &replay.candidate),
                (original.proposal_id, &original.candidate)
            );
        } else {
            assert!(
                matches!(outcome, Err(LedgerError::IdempotencyConflict)),
                "{outcome:?}"
            );
        }
        assert_eq!(stored_proposes(&pool, &g, key).await.len(), 1);
    }
    verify_clean(&store).await;
}

/// Prepare + accept one added quad on `branch` as `principal`, optionally acting for
/// `delegator`; the new head.
async fn change_as(
    store: &PostgresLedgerStore,
    g: &GraphId,
    branch: &str,
    head: CommitId,
    quad: &str,
    principal: &str,
    delegator: Option<&str>,
) -> CommitId {
    let key = unique("ca");
    let mut s = scope_as(principal, g, &format!("p-{key}"), &format!("p-{key}"));
    s.principal.on_behalf_of = delegator.map(|d| PrincipalId::new(format!("urn:it:{d}")).unwrap());
    let prepared = store
        .workflows()
        .prepare(&PrepareRequest {
            scope: s.clone(),
            branch: branch.into(),
            expected_head: Some(head.clone()),
            requested: Patch::new([Operation {
                kind: OperationKind::Add,
                quad: q(quad),
            }])
            .unwrap(),
            activity: "cognitive-correction".into(),
            event_time: None,
            evidence_refs: vec![],
            source_system: None,
            message: key.clone(),
        })
        .await
        .unwrap();
    s.idempotency_key = format!("a-{key}");
    s.request_digest = ContentId::for_bytes(s.idempotency_key.as_bytes());
    store
        .workflows()
        .accept(&AcceptRequest {
            scope: s,
            branch: branch.into(),
            expected_head: Some(head),
            candidate: prepared.candidate.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await
        .unwrap();
    prepared.candidate
}

/// Four-eyes cannot be laundered through delegation or a nested merge: the source parties
/// of a merge include the delegator of a source commit's proposer, and the authors and
/// proposer of a merge already integrated into the source (its whole source-only ancestry).
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn four_eyes_counts_delegators_and_the_authors_of_nested_merges() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A], &[]).await;
    let strict = BranchPolicy {
        protected: false,
        require_validation: false,
        require_distinct_reviewer: true,
    };
    store
        .workflows()
        .create_branch(
            &CreateBranchRequest {
                scope: scope(&g, "cb-strict"),
                name: "strict".into(),
                source: "main".into(),
                from_commit: None,
                policy: strict,
            },
            limits(),
        )
        .await
        .unwrap();
    branch(&store, &g, "agent/outer", BranchPolicy::default()).await;
    branch(&store, &g, "agent/inner", BranchPolicy::default()).await;
    // "helper" proposes on behalf of "owner" on the outer source.
    let s1 = change_as(
        &store,
        &g,
        "agent/outer",
        c1.clone(),
        "<urn:outer> <urn:p> \"1\" .",
        "helper",
        Some("owner"),
    )
    .await;
    // "inner-author" works on another branch, which "merger" merges into the outer source
    // (self-applied: the outer source has the default, lax policy).
    change_as(
        &store,
        &g,
        "agent/inner",
        c1.clone(),
        "<urn:inner> <urn:p> \"1\" .",
        "inner-author",
        None,
    )
    .await;
    let nested = spec("agent/inner", "agent/outer", MergeStrategy::Abort);
    let token = preview(&store, &g, &nested).await.preview_token.unwrap();
    let inner = store
        .workflows()
        .merge_propose(
            &ProposeMergeRequest {
                scope: scope_as("merger", &g, "nested-p", "nested-p"),
                spec: nested,
                preview_token: token.clone(),
                message: String::new(),
                evidence_refs: vec![],
            },
            limits(),
        )
        .await
        .unwrap();
    apply(&store, &g, inner.proposal_id, &token, "merger", "nested-a")
        .await
        .unwrap();
    assert_eq!(head(&store, &g, "agent/outer").await.0, inner.candidate);
    assert_ne!(inner.candidate, s1);

    // The outer merge onto the four-eyes target, proposed by "mechanic".
    let outer = spec("agent/outer", "strict", MergeStrategy::Abort);
    let token = preview(&store, &g, &outer).await.preview_token.unwrap();
    let pr = store
        .workflows()
        .merge_propose(
            &ProposeMergeRequest {
                scope: scope_as("mechanic", &g, "outer-p", "outer-p"),
                spec: outer,
                preview_token: token.clone(),
                message: String::new(),
                evidence_refs: vec![],
            },
            limits(),
        )
        .await
        .unwrap();
    let parties: Vec<String> =
        sqlx::query_scalar("SELECT source_parties FROM merge_proposals WHERE proposal_id = $1")
            .bind(pr.proposal_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    for party in ["helper", "owner", "inner-author", "merger"] {
        assert!(
            parties.contains(&format!("urn:it:{party}")),
            "{party} missing from {parties:?}"
        );
    }
    for who in ["owner", "helper", "inner-author", "merger", "mechanic"] {
        let r = apply(
            &store,
            &g,
            pr.proposal_id,
            &token,
            who,
            &format!("outer-{who}"),
        )
        .await;
        assert!(
            matches!(r, Err(LedgerError::BranchPolicyViolation(_))),
            "{who}: {r:?}"
        );
    }
    // Acting for a source author is the same party too.
    let mut delegated = scope_as("someone", &g, "outer-del", "outer-del");
    delegated.principal.on_behalf_of = Some(PrincipalId::new("urn:it:inner-author").unwrap());
    let r = store
        .workflows()
        .merge_apply(&ApplyMergeRequest {
            scope: delegated,
            proposal_id: pr.proposal_id,
            preview_token: token.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await;
    assert!(
        matches!(r, Err(LedgerError::BranchPolicyViolation(_))),
        "{r:?}"
    );
    apply(
        &store,
        &g,
        pr.proposal_id,
        &token,
        "reviewer",
        "outer-reviewer",
    )
    .await
    .unwrap();
    verify_clean(&store).await;
}
