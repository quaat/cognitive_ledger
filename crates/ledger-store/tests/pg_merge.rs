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
    // Applying the same proposal again under a new key: already decided.
    assert!(matches!(
        apply(
            &store,
            &g,
            proposed.proposal_id,
            &token,
            "reviewer",
            "ff-apply-2"
        )
        .await,
        Err(LedgerError::MergeStale(_) | LedgerError::LineageMismatch(_))
    ));
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
        let mut want: BTreeSet<Quad> = slot.iter().map(|s| q(s)).collect();
        want.extend(std::iter::empty());
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
    // Both branches have the same state, so with an explicit base it is no change; without
    // one it is reported ambiguous.
    assert_eq!(p.class, MergeClass::AmbiguousMergeBase(want));
    let explicit = MergeSpec {
        base: Some(x1.clone()),
        ..spec("y", "x", MergeStrategy::Abort)
    };
    assert_eq!(
        preview(&store, &g, &explicit).await.class,
        MergeClass::NoChange
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
