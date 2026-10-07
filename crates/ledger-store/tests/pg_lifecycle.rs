//! Plan 0013 M1 — request lifecycle versus transaction lifecycle, measured on real PostgreSQL
//! **before** any production timeout, cancellation or admission behaviour changes.
//!
//! Every test is classified in its doc comment (Plan 0013 §M1 evidence):
//! - `PRESERVATION`: passes on the pre-M2 code and must keep passing (ADR-0013 semantics);
//! - `CHARACTERIZATION`: pins today's undesirable but bounded behaviour with its measured
//!   envelope, so M2/M3 change it deliberately;
//! - `FUTURE ACCEPTANCE` (name prefix `future_`): asserts the ADR-0026 contract and fails today;
//!   excluded from the suite run with `--skip future_` and run once explicitly for the red
//!   evidence.
//!
//! Hooks are `test-hooks` only (`ledger_store::test_hooks`): a pause before `COMMIT`, a pause
//! after `COMMIT`, and an injected `pg_sleep` statement on the transaction's own connection.
//! Every test is `#[ignore]` and runs with `LEDGER_TEST_DATABASE_URL` (owner identity); each
//! store under observation gets its own `application_name`, so `pg_stat_activity` and
//! `pg_locks` assertions never see another test's sessions. Timing is observed through the
//! catalog, never through sleeps, except where a wall-clock wait *is* the scenario.
#![cfg(feature = "postgres")]

use ledger_core::{
    AuthenticatedPrincipal, CommitId, ContentId, GraphId, LedgerError, PrincipalId, PrincipalType,
    TenantId,
};
use ledger_rdf::{Operation, OperationKind, Patch, Quad};
use ledger_store::{
    AcceptRequest, ApplyMergeRequest, BranchLifecycleRequest, BranchPolicy, CreateBranchRequest,
    DbSessionLimits, GraphStatus, MergeSpec, MergeStrategy, NewGraph, PostgresLedgerStore,
    PrepareRequest, ProposeMergeRequest, RejectRequest, RequestScope, TraversalLimits, V1Binding,
    ValidateRequest, ValidationBegin, ValidationPolicy, ValidatorOutcome,
    test_hooks::{HookPoint, PauseHook},
};
use ledger_validation_protocol::{
    BaseKb, Ontology, RequestedContext, SemanticExecutionContext, ShapeSet, ValidationOutcome,
    ValidatorIdentity,
};
use sqlx::{Connection, Row};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const IGNORE: &str =
    "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL";

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

/// The test URL with a distinct `application_name`, so this store's sessions are
/// identifiable in `pg_stat_activity` and `pg_locks`.
fn named_url(name: &str) -> String {
    let base = database_url();
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}application_name={name}")
}

/// Owner store over the migrated shared database (default pool, unnamed): fixtures and
/// reference runs.
async fn store() -> PostgresLedgerStore {
    PostgresLedgerStore::connect_and_migrate(&database_url(), V1Binding::Reject)
        .await
        .unwrap()
}

/// A store over its own small pool built exactly as the server builds it
/// (`DbSessionLimits::pool_options`: session `SET`s, the PostgreSQL 17 backstop, the acquire
/// timeout) and carrying the same lifecycle limits (transaction bound) — here under the owner
/// identity, so no runtime-role setup is needed — named for the catalog views. The shared
/// database must already be migrated (`store()`).
async fn store_with_limits(name: &str, limits: DbSessionLimits) -> PostgresLedgerStore {
    let pool = limits
        .pool_options()
        .connect(&named_url(name))
        .await
        .unwrap();
    PostgresLedgerStore::from_pool_migrated(pool, V1Binding::Reject).with_session_limits(limits)
}

/// The M1 shape: `max_connections`, `statement_timeout`, `lock_timeout`; everything else at
/// the production defaults (transaction bound 20 s, acquire 5 s, idle 30 s).
async fn store_with(
    name: &str,
    max_connections: u32,
    statement_timeout: Duration,
    lock_timeout: Duration,
) -> PostgresLedgerStore {
    store_with_limits(
        name,
        DbSessionLimits {
            max_connections,
            statement_timeout,
            lock_timeout,
            ..DbSessionLimits::default()
        },
    )
    .await
}

/// A named store with the default session limits.
async fn named_store(name: &str, max_connections: u32) -> PostgresLedgerStore {
    store_with_limits(
        name,
        DbSessionLimits {
            max_connections,
            ..DbSessionLimits::default()
        },
    )
    .await
}

async fn graph(store: &PostgresLedgerStore) -> GraphId {
    let id = GraphId::new(unique("lc")).unwrap();
    store
        .graphs()
        .create(&NewGraph {
            graph_id: id.clone(),
            tenant_id: tenant(),
            knowledge_base_id: Some(unique("kb")),
            purpose: None,
            status: GraphStatus::Active,
        })
        .await
        .unwrap();
    id
}

fn tenant() -> TenantId {
    TenantId::new("tenant-lc").unwrap()
}

fn actor(principal: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal {
        principal_id: PrincipalId::new(format!("urn:lc:{principal}")).unwrap(),
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

fn patch(adds: &[&str]) -> Patch {
    Patch::new(adds.iter().map(|a| Operation {
        kind: OperationKind::Add,
        quad: q(a),
    }))
    .unwrap()
}

fn prepare_req(
    g: &GraphId,
    branch: &str,
    head: Option<CommitId>,
    key: &str,
    adds: &[&str],
) -> PrepareRequest {
    PrepareRequest {
        scope: scope(g, key),
        branch: branch.into(),
        expected_head: head,
        requested: patch(adds),
        activity: "cognitive-correction".into(),
        event_time: None,
        evidence_refs: vec![],
        source_system: None,
        message: key.to_owned(),
    }
}

/// The digest is the candidate (as the API layer's canonical accept identity would make
/// it), so the same key with another candidate is a different request.
fn accept_req(
    g: &GraphId,
    branch: &str,
    head: Option<CommitId>,
    candidate: &CommitId,
    key: &str,
) -> AcceptRequest {
    AcceptRequest {
        scope: scope_as("curator", g, key, &candidate.to_string()),
        branch: branch.into(),
        expected_head: head,
        candidate: candidate.clone(),
        reason: None,
        validation: ValidationPolicy::NoValidation,
    }
}

/// Prepare + accept one patch on `branch`; the new head.
async fn change(
    store: &PostgresLedgerStore,
    g: &GraphId,
    branch: &str,
    head: Option<CommitId>,
    adds: &[&str],
) -> CommitId {
    let key = unique("c");
    let p = store
        .workflows()
        .prepare(&prepare_req(
            g,
            branch,
            head.clone(),
            &format!("p-{key}"),
            adds,
        ))
        .await
        .unwrap();
    store
        .workflows()
        .accept(&accept_req(
            g,
            branch,
            head,
            &p.candidate,
            &format!("a-{key}"),
        ))
        .await
        .unwrap();
    p.candidate
}

async fn branch(store: &PostgresLedgerStore, g: &GraphId, name: &str) {
    store
        .workflows()
        .create_branch(
            &CreateBranchRequest {
                scope: scope(g, &format!("cb-{name}")),
                name: name.into(),
                source: "main".into(),
                from_commit: None,
                policy: BranchPolicy::default(),
            },
            limits(),
        )
        .await
        .unwrap();
}

/// Every graph-scoped table a workflow write can touch (`immutable_objects` is content-
/// addressed and shared, so it is not counted per graph).
const TABLES: [&str; 13] = [
    "semantic_execution_contexts",
    "validation_records",
    "proposals",
    "merge_proposals",
    "decisions",
    "ref_events",
    "projection_outbox",
    "idempotency",
    "commit_index",
    "commit_parents",
    "refs",
    "branches",
    "branch_events",
];

/// Row counts of everything a workflow write can produce, for this graph only.
async fn counts(store: &PostgresLedgerStore, g: &GraphId) -> BTreeMap<&'static str, i64> {
    let mut out = BTreeMap::new();
    for t in TABLES {
        // `commit_parents` is keyed by commit; its graph is the indexed commit's.
        let sql = if t == "commit_parents" {
            "SELECT count(*) FROM commit_parents cp JOIN commit_index ci ON ci.id = cp.commit_id \
             WHERE ci.graph_id = $1"
                .to_owned()
        } else {
            format!("SELECT count(*) FROM {t} WHERE graph_id = $1")
        };
        let n: i64 = sqlx::query_scalar(&sql)
            .bind(g.as_str())
            .fetch_one(store.pool())
            .await
            .unwrap();
        out.insert(t, n);
    }
    out
}

fn delta(
    before: &BTreeMap<&'static str, i64>,
    after: &BTreeMap<&'static str, i64>,
) -> BTreeMap<&'static str, i64> {
    after
        .iter()
        .filter_map(|(t, n)| {
            let d = n - before[t];
            (d != 0).then_some((*t, d))
        })
        .collect()
}

async fn idempotency_rows(
    store: &PostgresLedgerStore,
    g: &GraphId,
    operation: &str,
    key: &str,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM idempotency WHERE graph_id = $1 AND operation = $2 AND idempotency_key = $3",
    )
    .bind(g.as_str())
    .bind(operation)
    .bind(key)
    .fetch_one(store.pool())
    .await
    .unwrap()
}

/// One session of a named store as `pg_stat_activity` reports it.
#[derive(Debug, Clone)]
struct Session {
    pid: i32,
    state: String,
    wait_event_type: String,
    query: String,
}

/// Every session of the named store (its `application_name`).
async fn sessions(owner: &PostgresLedgerStore, app: &str) -> Vec<Session> {
    sqlx::query(
        "SELECT pid, coalesce(state, '') AS state, coalesce(wait_event_type, '') AS wet, \
         coalesce(query, '') AS query \
         FROM pg_stat_activity WHERE application_name = $1 ORDER BY pid",
    )
    .bind(app)
    .fetch_all(owner.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|r| Session {
        pid: r.get("pid"),
        state: r.get("state"),
        wait_event_type: r.get("wet"),
        query: r.get("query"),
    })
    .collect()
}

/// Granted locks (relation, tuple, advisory, transaction) held by the named store's sessions:
/// zero once every transaction of that store has ended.
async fn locks_held(owner: &PostgresLedgerStore, app: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid \
         WHERE a.application_name = $1 AND l.granted AND l.locktype <> 'virtualxid'",
    )
    .bind(app)
    .fetch_one(owner.pool())
    .await
    .unwrap()
}

fn active_in_sleep(s: &Session) -> bool {
    s.state == "active" && s.query.contains("pg_sleep")
}

async fn wait_until(mut condition: impl AsyncFnMut() -> bool, what: &str) -> Duration {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(30);
    while !condition().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    started.elapsed()
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
const C: &str = "<urn:c> <urn:p> \"1\" .";

// ---- every write path, uniformly --------------------------------------------------------

/// The nine idempotent workflow write paths, the validation record (begin + record; M2 closed
/// the M1 gap) included. Not here: `mark_superseded` (no key, F8) and the raw bootstrap CAS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Prepare,
    Accept,
    Reject,
    Validate,
    BranchCreate,
    BranchDelete,
    BranchRestore,
    MergePropose,
    MergeApply,
}

const OPS: [Op; 9] = [
    Op::Prepare,
    Op::Accept,
    Op::Reject,
    Op::Validate,
    Op::BranchCreate,
    Op::BranchDelete,
    Op::BranchRestore,
    Op::MergePropose,
    Op::MergeApply,
];

impl Op {
    fn operation(self) -> &'static str {
        match self {
            Op::Prepare => "prepare",
            Op::Accept => "accept",
            Op::Reject => "reject",
            Op::Validate => "validate",
            Op::BranchCreate => "branch_create",
            Op::BranchDelete => "branch_delete",
            Op::BranchRestore => "branch_restore",
            Op::MergePropose => "merge_propose",
            Op::MergeApply => "merge_apply",
        }
    }
}

/// Everything one operation needs, on a fresh graph, built with the plain store.
struct Fixture {
    g: GraphId,
    /// The genesis commit of `main` (a historical point once `head` moved on).
    genesis: CommitId,
    head: Option<CommitId>,
    candidate: Option<CommitId>,
    preview_token: String,
    proposal_id: i64,
}

async fn fixture(store: &PostgresLedgerStore, op: Op) -> Fixture {
    let g = graph(store).await;
    let c1 = change(store, &g, "main", None, &[A]).await;
    let mut f = Fixture {
        g: g.clone(),
        genesis: c1.clone(),
        head: Some(c1.clone()),
        candidate: None,
        preview_token: String::new(),
        proposal_id: 0,
    };
    match op {
        Op::Prepare | Op::BranchCreate => {}
        Op::Accept | Op::Reject | Op::Validate => {
            let p = store
                .workflows()
                .prepare(&prepare_req(
                    &g,
                    "main",
                    Some(c1.clone()),
                    "fx-prepare",
                    &[B],
                ))
                .await
                .unwrap();
            f.candidate = Some(p.candidate);
        }
        Op::BranchDelete => branch(store, &g, "agent/lc").await,
        Op::BranchRestore => {
            branch(store, &g, "agent/lc").await;
            store
                .workflows()
                .delete_branch(&BranchLifecycleRequest {
                    scope: scope(&g, "fx-delete"),
                    name: "agent/lc".into(),
                    reason: None,
                })
                .await
                .unwrap();
        }
        Op::MergePropose | Op::MergeApply => {
            branch(store, &g, "agent/lc").await;
            change(store, &g, "agent/lc", Some(c1.clone()), &[B]).await;
            let c2 = change(store, &g, "main", Some(c1.clone()), &[C]).await;
            f.head = Some(c2);
            let preview = store
                .workflows()
                .merge_preview(&tenant(), &g, &merge_spec(), limits())
                .await
                .unwrap();
            f.preview_token = preview.preview_token.unwrap();
            if op == Op::MergeApply {
                let proposed = store
                    .workflows()
                    .merge_propose(&propose_req(&g, &f.preview_token, "fx-propose"), limits())
                    .await
                    .unwrap();
                f.proposal_id = proposed.proposal_id;
            }
        }
    }
    f
}

fn merge_spec() -> MergeSpec {
    MergeSpec {
        source: "agent/lc".into(),
        target: "main".into(),
        strategy: MergeStrategy::Abort,
        base: None,
    }
}

fn propose_req(g: &GraphId, token: &str, key: &str) -> ProposeMergeRequest {
    ProposeMergeRequest {
        scope: scope(g, key),
        spec: merge_spec(),
        preview_token: token.into(),
        message: "integrate".into(),
        evidence_refs: vec![],
    }
}

/// A uniform view of any write's result: whether it replayed, and the identities a replay
/// must reproduce.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    replayed: bool,
    ids: String,
}

type OpFuture = Pin<Box<dyn Future<Output = Result<Outcome, LedgerError>> + Send>>;

/// Run `op` with `key` through `store` (whose repositories may carry a hook) against `f`.
fn run(store: PostgresLedgerStore, f: &Fixture, op: Op, key: &str) -> OpFuture {
    let repo = store.workflows().clone();
    let validations = store.validations().clone();
    let g = f.g.clone();
    let head = f.head.clone();
    let candidate = f.candidate.clone();
    let token = f.preview_token.clone();
    let proposal_id = f.proposal_id;
    let key = key.to_owned();
    Box::pin(async move {
        Ok(match op {
            Op::Prepare => {
                let r = repo
                    .prepare(&prepare_req(&g, "main", head, &key, &[B]))
                    .await?;
                Outcome {
                    replayed: r.replayed,
                    ids: format!("proposal:{} candidate:{}", r.proposal_id, r.candidate),
                }
            }
            Op::Accept => {
                let r = repo
                    .accept(&accept_req(
                        &g,
                        "main",
                        head,
                        candidate.as_ref().unwrap(),
                        &key,
                    ))
                    .await?;
                Outcome {
                    replayed: r.replayed,
                    ids: format!(
                        "decision:{} event:{} outbox:{} version:{} head:{}",
                        r.decision_id, r.ref_event_id, r.outbox_id, r.ref_version, r.head
                    ),
                }
            }
            Op::Validate => {
                let request = validate_req(&g, candidate.as_ref().unwrap(), &key);
                let ticket = match validations.begin(&request).await? {
                    ValidationBegin::Replayed(recorded) => {
                        return Ok(Outcome {
                            replayed: true,
                            ids: format!("validation:{}", recorded.validation_id),
                        });
                    }
                    ValidationBegin::Fresh(ticket) => ticket,
                };
                let r = validations
                    .record(
                        &request,
                        &ticket,
                        conforming(context_for(
                            &g,
                            candidate.as_ref().unwrap(),
                            ticket.state_digest(),
                        )),
                    )
                    .await?;
                Outcome {
                    replayed: r.replayed,
                    ids: format!("validation:{}", r.validation_id),
                }
            }
            Op::Reject => {
                let r = repo
                    .reject(&RejectRequest {
                        scope: scope_as("reviewer", &g, &key, &key),
                        branch: "main".into(),
                        candidate: candidate.unwrap(),
                        reason: "no".into(),
                        validation_id: None,
                    })
                    .await?;
                Outcome {
                    replayed: r.replayed,
                    ids: format!("decision:{}", r.decision_id),
                }
            }
            Op::BranchCreate => {
                let r = repo
                    .create_branch(
                        &CreateBranchRequest {
                            scope: scope(&g, &key),
                            name: "agent/lc".into(),
                            source: "main".into(),
                            from_commit: None,
                            policy: BranchPolicy::default(),
                        },
                        limits(),
                    )
                    .await?;
                Outcome {
                    replayed: r.replayed,
                    ids: format!(
                        "event:{} lifecycle:{}",
                        r.event.event_id, r.event.lifecycle_version
                    ),
                }
            }
            Op::BranchDelete | Op::BranchRestore => {
                let request = BranchLifecycleRequest {
                    scope: scope(&g, &key),
                    name: "agent/lc".into(),
                    reason: None,
                };
                let r = if op == Op::BranchDelete {
                    repo.delete_branch(&request).await?
                } else {
                    repo.restore_branch(&request).await?
                };
                Outcome {
                    replayed: r.replayed,
                    ids: format!(
                        "event:{} lifecycle:{} status:{}",
                        r.event.event_id, r.event.lifecycle_version, r.event.status_after
                    ),
                }
            }
            Op::MergePropose => {
                let r = repo
                    .merge_propose(&propose_req(&g, &token, &key), limits())
                    .await?;
                Outcome {
                    replayed: r.replayed,
                    ids: format!("proposal:{} candidate:{}", r.proposal_id, r.candidate),
                }
            }
            Op::MergeApply => {
                let r = repo
                    .merge_apply(&ApplyMergeRequest {
                        scope: scope_as("reviewer", &g, &key, &key),
                        proposal_id,
                        preview_token: token,
                        reason: None,
                        validation: ValidationPolicy::NoValidation,
                    })
                    .await?;
                Outcome {
                    replayed: r.replayed,
                    ids: format!(
                        "decision:{} event:{} outbox:{} version:{} head:{}",
                        r.decision_id, r.ref_event_id, r.outbox_id, r.ref_version, r.head
                    ),
                }
            }
        })
    })
}

/// The identity the *database* holds for the newest result of `op` on `f.g` (for the
/// validation: the result the idempotency row of `key` names), as the `ids` fragment a replay
/// must contain.
async fn durable_id_fragment(
    store: &PostgresLedgerStore,
    f: &Fixture,
    op: Op,
    key: &str,
) -> String {
    if op == Op::Validate {
        let id: String = sqlx::query_scalar(
            "SELECT result_validation_id FROM idempotency WHERE graph_id = $1 AND operation = 'validate' \
             AND idempotency_key = $2",
        )
        .bind(f.g.as_str())
        .bind(key)
        .fetch_one(store.pool())
        .await
        .unwrap();
        return format!("validation:{id}");
    }
    let (sql, label) = match op {
        Op::Prepare | Op::MergePropose => (
            "SELECT max(proposal_id) FROM proposals WHERE graph_id = $1",
            "proposal",
        ),
        Op::Accept | Op::Reject | Op::MergeApply => (
            "SELECT max(decision_id) FROM decisions WHERE graph_id = $1",
            "decision",
        ),
        Op::BranchCreate | Op::BranchDelete | Op::BranchRestore => (
            "SELECT max(event_id) FROM branch_events WHERE graph_id = $1",
            "event",
        ),
        Op::Validate => unreachable!(),
    };
    let id: i64 = sqlx::query_scalar(sql)
        .bind(f.g.as_str())
        .fetch_one(store.pool())
        .await
        .unwrap();
    format!("{label}:{id}")
}

/// Validation fixtures (the shape `pg_validation` uses): a request whose digest covers the
/// candidate and hints, a context naming the ticket's state digest, a conforming outcome.
fn validate_req(g: &GraphId, candidate: &CommitId, key: &str) -> ValidateRequest {
    let hints = RequestedContext::default();
    let mut digest = format!("validate:{candidate}:").into_bytes();
    hints.encode_into(&mut digest).unwrap();
    ValidateRequest {
        scope: RequestScope {
            principal: actor("validator-caller"),
            graph: g.clone(),
            idempotency_key: key.to_owned(),
            request_digest: ContentId::for_bytes(&digest),
            correlation_id: None,
        },
        candidate: candidate.clone(),
        requested: hints,
    }
}

fn context_for(g: &GraphId, candidate: &CommitId, state: &ContentId) -> SemanticExecutionContext {
    SemanticExecutionContext {
        graph_id: g.clone(),
        candidate_commit: candidate.clone(),
        candidate_state_digest: state.clone(),
        base_kb: BaseKb {
            kb_id: "urn:exodus:kb:lc".into(),
            revision: "kbrev-1".into(),
        },
        ontology: Some(Ontology {
            id: "urn:sculpin:ontology:core".into(),
            version: "O1".into(),
        }),
        shapes: ShapeSet {
            id: "urn:sculpin:shapes:core".into(),
            version: "S1".into(),
        },
        reasoning: None,
        sources_revision: None,
        virtual_contexts: vec![],
        validator: ValidatorIdentity {
            service_id: "urn:sculpin:validator".into(),
            service_version: "1.0".into(),
            configuration_version: "c1".into(),
        },
    }
}

fn conforming(context: SemanticExecutionContext) -> ValidatorOutcome {
    ValidatorOutcome {
        context,
        outcome: ValidationOutcome::conforms(),
        report_digest: ContentId::for_bytes(b"report-ok"),
        report_reference: None,
    }
}

/// Wait until the operation reaches its hook — or fail at once if it ended first (a bound that
/// fired early, a refused acquisition), instead of hanging on a hook that is never reached.
async fn reached_or_failed<T: std::fmt::Debug>(
    hook: &PauseHook,
    handle: &mut tokio::task::JoinHandle<Result<T, LedgerError>>,
) {
    tokio::select! {
        () = hook.reached() => {}
        r = handle => panic!("the operation ended before reaching the hook: {r:?}"),
    }
}

/// Start `op` on a store whose repositories carry `hook`, wait until it reaches the hook, and
/// abandon it (the task is aborted: the future, its transaction and its pooled connection are
/// dropped exactly as the pre-M2 edge dropped a handler future).
async fn abandon_at(store: &PostgresLedgerStore, f: &Fixture, op: Op, key: &str, hook: PauseHook) {
    let hooked = store.clone().with_workflow_pause_hook(hook.clone());
    let handle = tokio::spawn(run(hooked, f, op, key));
    hook.reached().await;
    handle.abort();
    let joined = handle.await;
    assert!(
        joined.is_err_and(|e| e.is_cancelled()),
        "{op:?}: the paused task must be cancelled"
    );
}

/// PRESERVATION. A write abandoned between its last statement and `COMMIT` (the client gave
/// up while the transaction was still open) rolls back: no durable idempotency result, no
/// partial row in any graph-scoped table, every lock its session held is released (observed in
/// `pg_locks`, timed), the session is idle, and the same-key retry runs fresh — never a
/// replay — producing exactly the rows a clean run produces (ADR-0013: results, not
/// reservations; a rolled-back attempt leaves the key unused). The nine workflow write paths,
/// the validation record included (M2 closed that M1 gap).
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_write_dropped_before_commit_rolls_back_leaves_the_key_unused_and_the_retry_runs_fresh() {
    let _ = IGNORE;
    let store = store().await;
    let app = unique("lc-drop");
    let named = named_store(&app, 4).await;
    let mut report = Vec::new();
    for op in OPS {
        // Reference: the rows one clean execution produces.
        let clean = fixture(&store, op).await;
        let before = counts(&store, &clean.g).await;
        let reference = run(store.clone(), &clean, op, "clean").await.unwrap();
        assert!(!reference.replayed, "{op:?}");
        let expected = delta(&before, &counts(&store, &clean.g).await);
        assert!(
            expected.contains_key("idempotency"),
            "{op:?}: every write records its result: {expected:?}"
        );

        // Abandon one at the pause before COMMIT (locks held, every row written).
        let f = fixture(&store, op).await;
        let before = counts(&store, &f.g).await;
        abandon_at(
            &named,
            &f,
            op,
            "dropped",
            PauseHook::new(HookPoint::BeforeCommit),
        )
        .await;
        let dropped_at = Instant::now();
        // Locks: observed released in pg_locks (the ROLLBACK the dropped transaction queued
        // has been processed), then the session is idle.
        let locks_released = wait_until(
            async || locks_held(&store, &app).await == 0,
            "every lock of the dropped transaction released",
        )
        .await;
        wait_until(
            async || {
                let s = sessions(&store, &app).await;
                !s.is_empty() && s.iter().all(|s| s.state == "idle")
            },
            "the dropped attempt's session idle",
        )
        .await;
        assert!(
            locks_released < Duration::from_secs(1),
            "{op:?}: rollback released the locks only after {locks_released:?}"
        );
        assert_eq!(
            delta(&before, &counts(&store, &f.g).await),
            BTreeMap::new(),
            "{op:?}: nothing durable"
        );
        // The retry runs afresh and writes exactly one clean execution's rows.
        let retry_started = Instant::now();
        let retried = run(store.clone(), &f, op, "dropped").await.unwrap();
        let retry_took = retry_started.elapsed();
        assert!(
            !retried.replayed,
            "{op:?}: a rolled-back attempt must not be replayable"
        );
        let after = counts(&store, &f.g).await;
        assert_eq!(
            delta(&before, &after),
            expected,
            "{op:?}: the retry writes exactly one clean execution's rows"
        );
        assert_eq!(
            idempotency_rows(&store, &f.g, op.operation(), "dropped").await,
            1,
            "{op:?}"
        );
        // A second retry replays the retry's result, not anything of the dropped attempt.
        let replay = run(store.clone(), &f, op, "dropped").await.unwrap();
        assert_eq!(
            replay,
            Outcome {
                replayed: true,
                ids: retried.ids.clone()
            },
            "{op:?}"
        );
        assert_eq!(
            counts(&store, &f.g).await,
            after,
            "{op:?}: the replay writes nothing"
        );
        report.push(format!(
            "{op:?}: locks released {locks_released:?} after the drop; fresh retry took {retry_took:?} ({:?} after the drop)",
            dropped_at.elapsed()
        ));
    }
    verify_clean(&store).await;
    println!("{}", report.join("\n"));
}

/// PRESERVATION. A write abandoned after `COMMIT` returned (the response was lost) has a
/// durable result: every row is there, the session is idle (`pg_stat_activity`), the
/// invariants hold, and the same-key retry replays exactly the identities the database holds
/// without writing anything. COMMIT is the durable-outcome boundary; the HTTP acknowledgement
/// that may be lost is downstream of it. The nine workflow write paths, the validation record
/// included.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_write_dropped_after_commit_is_durable_and_the_retry_replays_exactly() {
    let store = store().await;
    let app = unique("lc-lost");
    let named = named_store(&app, 4).await;
    for op in OPS {
        let clean = fixture(&store, op).await;
        let before = counts(&store, &clean.g).await;
        run(store.clone(), &clean, op, "clean").await.unwrap();
        let expected = delta(&before, &counts(&store, &clean.g).await);

        let f = fixture(&store, op).await;
        let before = counts(&store, &f.g).await;
        abandon_at(
            &named,
            &f,
            op,
            "lost",
            PauseHook::new(HookPoint::AfterCommit),
        )
        .await;
        wait_until(
            async || {
                let s = sessions(&store, &app).await;
                !s.is_empty() && s.iter().all(|s| s.state == "idle")
            },
            "the session idle after COMMIT",
        )
        .await;
        assert_eq!(locks_held(&store, &app).await, 0, "{op:?}");
        let after_drop = counts(&store, &f.g).await;
        assert_eq!(
            delta(&before, &after_drop),
            expected,
            "{op:?}: committed rows are durable although the response was lost"
        );
        assert_eq!(
            idempotency_rows(&store, &f.g, op.operation(), "lost").await,
            1,
            "{op:?}"
        );
        let durable = durable_id_fragment(&store, &f, op, "lost").await;
        let replay = run(store.clone(), &f, op, "lost").await.unwrap();
        assert!(
            replay.replayed,
            "{op:?}: the retry must replay the committed result"
        );
        assert!(
            replay.ids.contains(&durable),
            "{op:?}: replay {replay:?} must name the committed row {durable}"
        );
        assert_eq!(
            counts(&store, &f.g).await,
            after_drop,
            "{op:?}: a replay writes nothing"
        );
        let again = run(store.clone(), &f, op, "lost").await.unwrap();
        assert_eq!(again, replay, "{op:?}: replays are stable");
    }
    verify_clean(&store).await;
}

/// PRESERVATION (premise of the Plan 0013 admission model). Every heavy store path completes
/// on a pool of exactly one connection — the eight write paths, branch creation from a
/// historical commit (reachability walk, then the transaction), the merge preview (three
/// reconstructions), the public state read (membership lookup, then reconstruction) and the
/// first-parent history walk: no path ever needs two connections at once, so one admitted
/// operation occupies at most one connection (the DB-work permit of ADR-0026 bounds
/// connections one-to-one). Plan 0012 proved prepare / preview / propose / apply this way;
/// M2 added the validation begin/record pair (`Op::Validate`).
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn every_heavy_store_path_completes_on_a_one_connection_pool() {
    let owner = store().await;
    let one = named_store(&unique("lc-one"), 1).await;
    for op in OPS {
        let f = fixture(&owner, op).await;
        let r = run(one.clone(), &f, op, "one").await.unwrap();
        assert!(!r.replayed, "{op:?}");
    }
    let f = fixture(&owner, Op::MergePropose).await;
    let head = f.head.clone().unwrap();
    one.workflows()
        .merge_preview(&tenant(), &f.g, &merge_spec(), limits())
        .await
        .unwrap();
    // Branch from a historical commit: the reachability walk runs on a pooled connection
    // before the transaction begins.
    let created = one
        .workflows()
        .create_branch(
            &CreateBranchRequest {
                scope: scope(&f.g, "one-hist"),
                name: "agent/hist".into(),
                source: "main".into(),
                from_commit: Some(f.genesis.clone()),
                policy: BranchPolicy::default(),
            },
            limits(),
        )
        .await
        .unwrap();
    assert_eq!(created.event.head, f.genesis);
    // The public state read: membership, then reconstruction.
    assert_eq!(one.commit_graph(&head).await.unwrap(), Some(f.g.clone()));
    let state = one
        .workflows()
        .reconstruct(&head, &ledger_store::ReconstructionLimits::DEVELOPMENT)
        .await
        .unwrap();
    assert_eq!(state.len(), 2);
    let history = one
        .workflows()
        .first_parent_history(&tenant(), &f.g, &head, 100, limits())
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
    verify_clean(&owner).await;
}

// ---- abandoned active statements ---------------------------------------------------------

/// CHARACTERIZATION (F1). A request abandoned while its SQL statement is running leaves the
/// statement running server-side: the session stays `active`, its pooled connection is not
/// available to anyone (sqlx returns it only once PostgreSQL has finished the statement),
/// its locks are held meanwhile, and only then is the transaction rolled back. The envelope
/// is measured from the moment the statement was sent (the hook signals just before): with
/// the statement shorter than `statement_timeout` it ends by itself; with `statement_timeout`
/// shorter, PostgreSQL cancels it (57014) and the connection is released at ≈
/// `statement_timeout` plus the client-observed return to the pool. Either way nothing
/// durable remains and a retry runs fresh. After M2 (`statement_timeout < request_timeout`
/// validated) this same test is the acceptance test of the timeout floor.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn an_abandoned_active_statement_pins_its_connection_until_postgres_ends_it() {
    let owner = store().await;
    let mut report = Vec::new();
    for (statement_timeout, sleep, label) in [
        (
            Duration::from_secs(10),
            Duration::from_millis(1500),
            "statement ends by itself",
        ),
        (
            Duration::from_millis(700),
            Duration::from_secs(10),
            "statement_timeout cancels it",
        ),
    ] {
        let app = unique("lc-pin");
        let one = store_with(&app, 1, statement_timeout, Duration::from_secs(10)).await;
        let f = fixture(&owner, Op::Accept).await;
        let before = counts(&owner, &f.g).await;
        let hook = PauseHook::slow_statement(HookPoint::BeforeCommit, sleep, 1);
        let hooked = one.clone().with_workflow_pause_hook(hook.clone());
        let handle = tokio::spawn(run(hooked, &f, Op::Accept, "pinned"));
        hook.reached().await;
        let statement_sent = Instant::now();
        // The statement is observably in flight server-side; then abandon the request exactly
        // as an edge timeout drops a handler future.
        let seen = wait_until(
            async || sessions(&owner, &app).await.iter().any(active_in_sleep),
            "the statement visible as active",
        )
        .await;
        let abandoned = Instant::now();
        handle.abort();
        assert!(handle.await.is_err_and(|e| e.is_cancelled()));
        assert!(
            sessions(&owner, &app).await.iter().any(active_in_sleep),
            "{label}: the abandoned statement keeps running server-side"
        );
        assert!(
            locks_held(&owner, &app).await > 0,
            "{label}: its transaction still holds locks"
        );
        // The only connection is pinned: nobody can acquire it.
        let acquire = tokio::time::timeout(Duration::from_millis(300), one.pool().acquire()).await;
        assert!(
            acquire.is_err(),
            "{label}: the pinned connection was handed out while its statement ran"
        );
        // It becomes available only once PostgreSQL ended the statement.
        wait_until(
            async || {
                tokio::time::timeout(Duration::from_millis(100), one.pool().acquire())
                    .await
                    .is_ok_and(|c| c.is_ok())
            },
            "the connection to return to the pool",
        )
        .await;
        let held_from_send = statement_sent.elapsed();
        let held_after_drop = abandoned.elapsed();
        let expected = sleep.min(statement_timeout);
        assert!(
            held_from_send >= expected && held_from_send < expected + Duration::from_secs(2),
            "{label}: connection held {held_from_send:?} from the statement, expected ≈ {expected:?}"
        );
        // Rolled back: nothing durable, no locks, the session idle, the retry fresh.
        assert_eq!(
            delta(&before, &counts(&owner, &f.g).await),
            BTreeMap::new(),
            "{label}"
        );
        assert_eq!(
            idempotency_rows(&owner, &f.g, "accept", "pinned").await,
            0,
            "{label}"
        );
        wait_until(
            async || {
                let s = sessions(&owner, &app).await;
                !s.is_empty()
                    && s.iter().all(|s| s.state == "idle")
                    && locks_held(&owner, &app).await == 0
            },
            "the session idle without locks",
        )
        .await;
        let retried = run(one.clone(), &f, Op::Accept, "pinned").await.unwrap();
        assert!(!retried.replayed, "{label}");
        report.push(format!(
            "{label}: statement_timeout {statement_timeout:?}, sleep {sleep:?}: active seen after {seen:?}; connection released {held_from_send:?} after the statement was sent ({held_after_drop:?} after the drop) — client-observed, one sample"
        ));
    }
    verify_clean(&owner).await;
    println!("{}", report.join("\n"));
}

/// CHARACTERIZATION (hazard proof). A PostgreSQL backend pid names the *session*, not the
/// request that last used it. After an abandoned request's statement ends, its pooled
/// connection — the same backend — is lent to the next request; a cancellation addressed to
/// the pid that arrives late then cancels that later request's statement (57014). This is
/// exactly the race the `pg_cancel_backend(pid)` drop guard of the M0 model has, and why
/// ADR-0026 issues no active cancellation. The cancel here is issued by the test through a
/// raw pool: the test pins the hazard; it is not the gate of a future fenced design (that
/// design brings its own gate test through the production cancellation path, ADR-0026 §3).
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_late_cancel_addressed_by_pid_hits_the_next_borrower_of_the_pooled_session() {
    let owner = store().await;
    let app = unique("lc-reuse");
    let one = named_store(&app, 1).await;
    // Request A: owns the only connection, starts a statement, is abandoned mid-flight.
    let mut conn_a = one.pool().acquire().await.unwrap();
    let pid_a: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *conn_a)
        .await
        .unwrap();
    let a = tokio::spawn(async move {
        sqlx::query("SELECT pg_sleep(0.5)")
            .execute(&mut *conn_a)
            .await
    });
    wait_until(
        async || {
            sessions(&owner, &app)
                .await
                .iter()
                .any(|s| s.pid == pid_a && active_in_sleep(s))
        },
        "A active in its statement",
    )
    .await;
    a.abort();
    assert!(a.await.is_err_and(|e| e.is_cancelled()));
    // A's "delayed cancellation" is deliberately held back while A's statement finishes and
    // the connection returns to the pool.
    let mut conn_b = wait_until_acquired(&one).await;
    let pid_b: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *conn_b)
        .await
        .unwrap();
    assert_eq!(
        pid_b, pid_a,
        "the pool lends the same backend to the next request"
    );
    // Request B: an unrelated long statement on the reused session.
    let b = tokio::spawn(async move {
        let r = sqlx::query("SELECT pg_sleep(5)")
            .execute(&mut *conn_b)
            .await;
        drop(conn_b);
        r
    });
    wait_until(
        async || {
            sessions(&owner, &app)
                .await
                .iter()
                .any(|s| s.pid == pid_a && active_in_sleep(s))
        },
        "B active on the reused backend",
    )
    .await;
    // Now A's late cancellation fires, addressed by pid.
    let cancelled: bool = sqlx::query_scalar("SELECT pg_cancel_backend($1)")
        .bind(pid_a)
        .fetch_one(owner.pool())
        .await
        .unwrap();
    assert!(cancelled);
    let outcome = b.await.unwrap();
    let code = match &outcome {
        Err(sqlx::Error::Database(e)) => e.code().map(|c| c.to_string()),
        other => panic!("B must have been cancelled by A's late cancel, got {other:?}"),
    };
    assert_eq!(
        code.as_deref(),
        Some("57014"),
        "B's statement was cancelled by a cancel meant for A"
    );
    // The session itself survives (a cancel is not a terminate) and serves again.
    let mut conn = wait_until_acquired(&one).await;
    let one_: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(one_, 1);
}

async fn wait_until_acquired(
    store: &PostgresLedgerStore,
) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(Ok(c)) =
            tokio::time::timeout(Duration::from_millis(100), store.pool().acquire()).await
        {
            return c;
        }
        assert!(
            Instant::now() < deadline,
            "connection never returned to the pool"
        );
    }
}

/// ACCEPTED (M2; M1 characterization: the error then classified `Storage`). PostgreSQL 17's
/// `transaction_timeout` **terminates the session** (FATAL, SQLSTATE 25P04) rather than
/// cancelling the statement: the pooled connection is gone and the pool reconnects; since M2
/// the error is retryable (`DependencyTimeout`, ADR-0026 §2). On PostgreSQL 15 the setting does
/// not exist (42704). Both facts are asserted, so the test is meaningful on either server.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn pg17_transaction_timeout_terminates_the_session_and_pg15_lacks_it() {
    let owner = store().await;
    let app = unique("lc-txto");
    let one = named_store(&app, 1).await;
    let version: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(one.pool())
        .await
        .unwrap();
    let version: i64 = version.parse().unwrap();
    let mut conn = one.pool().acquire().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let set = sqlx::query("SET transaction_timeout = '300ms'")
        .execute(&mut *conn)
        .await;
    if version < 170000 {
        let code = match &set {
            Err(sqlx::Error::Database(e)) => e.code().map(|c| c.to_string()),
            other => {
                panic!("PostgreSQL {version}: transaction_timeout must not exist, got {other:?}")
            }
        };
        assert_eq!(
            code.as_deref(),
            Some("42704"),
            "unrecognized configuration parameter"
        );
        println!("server {version}: no transaction_timeout (42704)");
        return;
    }
    set.unwrap();
    sqlx::query("BEGIN").execute(&mut *conn).await.unwrap();
    let started = Instant::now();
    let err = sqlx::query("SELECT pg_sleep(2)")
        .execute(&mut *conn)
        .await
        .unwrap_err();
    let fired = started.elapsed();
    let code = match &err {
        sqlx::Error::Database(e) => e.code().map(|c| c.to_string()),
        _ => None,
    };
    let classified = ledger_store::classify_db_error(err);
    assert!(
        matches!(classified, LedgerError::DependencyTimeout(_)),
        "25P04 is retryable since M2 (ADR-0026 §2): {classified:?}"
    );
    // The session is terminated: the next statement on this connection fails, and the pool
    // reconnects with a new backend.
    let next = sqlx::query("SELECT 1").execute(&mut *conn).await;
    assert!(next.is_err(), "the session must be gone, got {next:?}");
    drop(conn);
    let mut fresh = wait_until_acquired(&one).await;
    let pid2: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *fresh)
        .await
        .unwrap();
    assert_ne!(pid2, pid, "a new backend replaced the terminated one");
    assert!(
        sessions(&owner, &app).await.iter().all(|s| s.pid != pid),
        "the terminated backend is gone"
    );
    println!(
        "server {version}: transaction_timeout 300ms fired after {fired:?} with SQLSTATE {code:?}; classified today as {classified:?}; session replaced (pid {pid} → {pid2})"
    );
}

// ---- SQLSTATE mapping through real write transactions --------------------------------------

/// PRESERVATION. `statement_timeout` (57014) fired inside an accept transaction (the injected
/// statement runs on the transaction's connection, so the lock it holds is real) surfaces as
/// `DependencyTimeout` naming the statement timeout, the transaction is rolled back, no key
/// is left behind, and the retry by key succeeds afresh.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_statement_timeout_inside_an_accept_is_a_dependency_timeout_and_the_retry_succeeds() {
    let owner = store().await;
    let short = store_with(
        &unique("lc-st"),
        2,
        Duration::from_millis(300),
        Duration::from_secs(10),
    )
    .await;
    let f = fixture(&owner, Op::Accept).await;
    let before = counts(&owner, &f.g).await;
    let hook = PauseHook::slow_statement(HookPoint::BeforeCommit, Duration::from_secs(3), 1);
    let started = Instant::now();
    let err = run(
        short.clone().with_workflow_pause_hook(hook),
        &f,
        Op::Accept,
        "st",
    )
    .await
    .unwrap_err();
    let took = started.elapsed();
    assert!(
        matches!(&err, LedgerError::DependencyTimeout(m) if m.contains("statement timeout")),
        "{err:?}"
    );
    assert!(
        took < Duration::from_secs(2),
        "cancelled at statement_timeout, not after the sleep: {took:?}"
    );
    assert_eq!(delta(&before, &counts(&owner, &f.g).await), BTreeMap::new());
    assert_eq!(idempotency_rows(&owner, &f.g, "accept", "st").await, 0);
    let retried = run(short.clone(), &f, Op::Accept, "st").await.unwrap();
    assert!(!retried.replayed);
    verify_clean(&owner).await;
}

/// PRESERVATION. `lock_timeout` (55P03) on the ref row `R(main) FOR UPDATE` during an accept
/// (a holder keeps the row locked) surfaces as `DependencyTimeout` after ≈ `lock_timeout`,
/// leaves no key, and the retry succeeds once the holder is gone.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_lock_timeout_on_the_ref_row_during_accept_is_a_dependency_timeout_and_the_retry_succeeds()
 {
    let owner = store().await;
    let short = store_with(
        &unique("lc-lt"),
        2,
        Duration::from_secs(30),
        Duration::from_millis(300),
    )
    .await;
    let f = fixture(&owner, Op::Accept).await;
    let before = counts(&owner, &f.g).await;
    let mut holder = owner.pool().begin().await.unwrap();
    sqlx::query("SELECT head FROM refs WHERE graph_id = $1 AND branch = 'main' FOR UPDATE")
        .bind(f.g.as_str())
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    let started = Instant::now();
    let err = run(short.clone(), &f, Op::Accept, "lt").await.unwrap_err();
    let took = started.elapsed();
    assert!(matches!(err, LedgerError::DependencyTimeout(_)), "{err:?}");
    assert!(
        took >= Duration::from_millis(250) && took < Duration::from_secs(3),
        "{took:?}"
    );
    assert_eq!(idempotency_rows(&owner, &f.g, "accept", "lt").await, 0);
    assert_eq!(delta(&before, &counts(&owner, &f.g).await), BTreeMap::new());
    holder.rollback().await.unwrap();
    let retried = run(short.clone(), &f, Op::Accept, "lt").await.unwrap();
    assert!(!retried.replayed);
    verify_clean(&owner).await;
}

/// PRESERVATION. The two SQLSTATEs the workflow never produces by its own lock order —
/// `40P01 deadlock_detected` and `40001 serialization_failure` — classified at the mapping
/// boundary from real PostgreSQL errors raised by raw test transactions (two advisory locks
/// taken in opposite order; two SERIALIZABLE transactions with a read/write dependency):
/// both are `DependencyTimeout` (retryable, 503), never a storage fault. The production lock
/// order is untouched. (The serialization case leaves one `bootstrap` graph row behind, like
/// every fixture graph of this suite.)
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn deadlock_and_serialization_failures_classify_as_dependency_timeouts_at_the_boundary() {
    store().await; // the shared database is migrated
    // 40P01: PostgreSQL's deadlock detector breaks a cycle of two raw transactions.
    let (k1, k2) = (
        ledger_store::lock_key(&unique("dl-1")),
        ledger_store::lock_key(&unique("dl-2")),
    );
    let mut t1 = sqlx::PgConnection::connect(&database_url()).await.unwrap();
    let mut t2 = sqlx::PgConnection::connect(&database_url()).await.unwrap();
    for (t, k) in [(&mut t1, k1), (&mut t2, k2)] {
        sqlx::query("BEGIN").execute(&mut *t).await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(k)
            .execute(&mut *t)
            .await
            .unwrap();
    }
    let h1 = tokio::spawn(async move {
        let r = sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(k2)
            .execute(&mut t1)
            .await;
        (r, t1)
    });
    // Either ordering of the two waits yields exactly one victim; nothing here is timing-
    // sensitive.
    let r2 = sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(k1)
        .execute(&mut t2)
        .await;
    let (r1, mut t1) = h1.await.unwrap();
    let errors: Vec<sqlx::Error> = [r1, r2].into_iter().filter_map(Result::err).collect();
    assert_eq!(errors.len(), 1, "exactly one side is the deadlock victim");
    let err = errors.into_iter().next().unwrap();
    let code = match &err {
        sqlx::Error::Database(e) => e.code().map(|c| c.to_string()),
        other => panic!("{other:?}"),
    };
    assert_eq!(code.as_deref(), Some("40P01"));
    assert!(matches!(
        ledger_store::classify_db_error(err),
        LedgerError::DependencyTimeout(_)
    ));
    for t in [&mut t1, &mut t2] {
        let _ = sqlx::query("ROLLBACK").execute(&mut *t).await;
    }
    // 40001: write skew under SERIALIZABLE on `graphs` (owner-only inserts; the loser's row
    // is rolled back, the winner's is just another unique graph).
    let tenant = unique("ser-tenant");
    let mut s1 = sqlx::PgConnection::connect(&database_url()).await.unwrap();
    let mut s2 = sqlx::PgConnection::connect(&database_url()).await.unwrap();
    for s in [&mut s1, &mut s2] {
        sqlx::query("BEGIN ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *s)
            .await
            .unwrap();
        let _: i64 = sqlx::query_scalar("SELECT count(*) FROM graphs WHERE tenant_id = $1")
            .bind(&tenant)
            .fetch_one(&mut *s)
            .await
            .unwrap();
    }
    for (i, s) in [&mut s1, &mut s2].into_iter().enumerate() {
        sqlx::query("INSERT INTO graphs (graph_id, tenant_id, knowledge_base_id, purpose, status) VALUES ($1, $2, NULL, NULL, 'bootstrap')")
            .bind(unique(&format!("ser-{i}")))
            .bind(&tenant)
            .execute(&mut *s)
            .await
            .unwrap();
    }
    let c1 = sqlx::query("COMMIT").execute(&mut s1).await;
    let c2 = sqlx::query("COMMIT").execute(&mut s2).await;
    let errors: Vec<sqlx::Error> = [c1, c2].into_iter().filter_map(Result::err).collect();
    assert_eq!(
        errors.len(),
        1,
        "exactly one serializable transaction fails to commit"
    );
    let err = errors.into_iter().next().unwrap();
    let code = match &err {
        sqlx::Error::Database(e) => e.code().map(|c| c.to_string()),
        other => panic!("{other:?}"),
    };
    assert_eq!(code.as_deref(), Some("40001"));
    assert!(matches!(
        ledger_store::classify_db_error(err),
        LedgerError::DependencyTimeout(_)
    ));
    let _ = sqlx::query("ROLLBACK").execute(&mut s2).await;
    // The direct classification of every code in the mapping.
    for code in ["57014", "55P03", "40001", "40P01"] {
        assert!(ledger_store::sqlstate_is_timeout(code), "{code}");
        assert!(!ledger_store::sqlstate_is_unavailable(code), "{code}");
    }
}

// ---- same-key concurrency (ADR-0013) ----------------------------------------------------------

/// Force a genuine overlap: the winner (its own named store) pauses just before `COMMIT`
/// holding the idempotency advisory lock; three same-key followers on another named store are
/// started and observed queued on a lock in `pg_stat_activity`; only then is the winner
/// resumed. Returns the winner's and the followers' results.
async fn overlapping_same_key<T: Send + 'static>(
    owner: &PostgresLedgerStore,
    winner_app: &str,
    follower_app: &str,
    winner: impl FnOnce(
        ledger_store::WorkflowRepository,
    ) -> Pin<Box<dyn Future<Output = Result<T, LedgerError>> + Send>>,
    follower: impl Fn(
        ledger_store::WorkflowRepository,
    ) -> Pin<Box<dyn Future<Output = Result<T, LedgerError>> + Send>>,
) -> (T, Vec<T>) {
    let winner_store = named_store(winner_app, 2).await;
    let followers_store = named_store(follower_app, 4).await;
    let hook = PauseHook::new(HookPoint::BeforeCommit);
    let paused = winner_store
        .workflows()
        .clone()
        .with_pause_hook(hook.clone());
    let w = tokio::spawn(winner(paused));
    hook.reached().await;
    let mut handles = Vec::new();
    for _ in 0..3 {
        handles.push(tokio::spawn(follower(followers_store.workflows().clone())));
    }
    wait_until(
        async || {
            sessions(owner, follower_app)
                .await
                .iter()
                .filter(|s| s.state == "active" && s.wait_event_type == "Lock")
                .count()
                >= 3
        },
        "three same-key followers queued on the idempotency lock",
    )
    .await;
    hook.resume();
    let winner_result = w.await.unwrap().unwrap();
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap().unwrap());
    }
    (winner_result, results)
}

/// PRESERVATION. Four replicas accept the same candidate onto a non-genesis head under one
/// key with a forced overlap (the first is paused before `COMMIT` while the other three are
/// observed waiting on its idempotency lock): exactly one executes, the others replay its
/// identical result, the ref moves once. The same key with a different candidate (different
/// request digest) is `IdempotencyConflict`, and a different key for a second candidate on the
/// moved head is `HeadChanged` — never a second move.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn concurrent_same_key_accepts_on_a_non_genesis_head_execute_once_and_replay() {
    let store = store().await;
    let g = graph(&store).await;
    let c1 = change(&store, &g, "main", None, &[A]).await;
    let p2 = store
        .workflows()
        .prepare(&prepare_req(&g, "main", Some(c1.clone()), "p2", &[B]))
        .await
        .unwrap();
    let p3 = store
        .workflows()
        .prepare(&prepare_req(&g, "main", Some(c1.clone()), "p3", &[C]))
        .await
        .unwrap();
    let before = counts(&store, &g).await;
    let request = accept_req(&g, "main", Some(c1.clone()), &p2.candidate, "shared-accept");
    let (winner, followers) = overlapping_same_key(
        &store,
        &unique("lc-acc-w"),
        &unique("lc-acc-f"),
        {
            let request = request.clone();
            move |repo| Box::pin(async move { repo.accept(&request).await })
        },
        {
            let request = request.clone();
            move |repo| {
                let request = request.clone();
                Box::pin(async move { repo.accept(&request).await })
            }
        },
    )
    .await;
    assert!(!winner.replayed);
    assert!(followers.iter().all(|o| o.replayed), "{followers:?}");
    let ids = (
        winner.decision_id,
        winner.ref_event_id,
        winner.outbox_id,
        winner.ref_version,
    );
    assert!(followers.iter().all(|o| {
        (o.decision_id, o.ref_event_id, o.outbox_id, o.ref_version) == ids && o.head == p2.candidate
    }));
    assert_eq!(ids.3, 2);
    let d = delta(&before, &counts(&store, &g).await);
    assert_eq!(
        d,
        BTreeMap::from([
            ("decisions", 1),
            ("ref_events", 1),
            ("projection_outbox", 1),
            ("idempotency", 1)
        ])
    );
    // Same key, different request: conflict; nothing written.
    let err = store
        .workflows()
        .accept(&accept_req(
            &g,
            "main",
            Some(c1.clone()),
            &p3.candidate,
            "shared-accept",
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, LedgerError::IdempotencyConflict), "{err:?}");
    // Another key for the second candidate: the head moved, so HEAD_CHANGED, never a move.
    let err = store
        .workflows()
        .accept(&accept_req(
            &g,
            "main",
            Some(c1.clone()),
            &p3.candidate,
            "other-key",
        ))
        .await
        .unwrap_err();
    assert!(matches!(err, LedgerError::HeadChanged { .. }), "{err:?}");
    assert_eq!(delta(&before, &counts(&store, &g).await), d);
    verify_clean(&store).await;
}

/// PRESERVATION. Four replicas apply one merge proposal under one key with a forced overlap:
/// one applies, the others replay the identical result, the target moves once; the same key
/// with another proposal is `IdempotencyConflict`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn concurrent_same_key_merge_applies_execute_once_and_replay() {
    let store = store().await;
    let f = fixture(&store, Op::MergeApply).await;
    // A second proposal of the same preview (duplicate proposals are clean, ADR-0024).
    let other = store
        .workflows()
        .merge_propose(
            &propose_req(&f.g, &f.preview_token, "fx-propose-2"),
            limits(),
        )
        .await
        .unwrap();
    let before = counts(&store, &f.g).await;
    let request = ApplyMergeRequest {
        scope: scope_as("reviewer", &f.g, "shared-apply", "shared-apply"),
        proposal_id: f.proposal_id,
        preview_token: f.preview_token.clone(),
        reason: None,
        validation: ValidationPolicy::NoValidation,
    };
    let (winner, followers) = overlapping_same_key(
        &store,
        &unique("lc-ma-w"),
        &unique("lc-ma-f"),
        {
            let request = request.clone();
            move |repo| Box::pin(async move { repo.merge_apply(&request).await })
        },
        {
            let request = request.clone();
            move |repo| {
                let request = request.clone();
                Box::pin(async move { repo.merge_apply(&request).await })
            }
        },
    )
    .await;
    assert!(!winner.replayed);
    assert!(followers.iter().all(|o| o.replayed), "{followers:?}");
    let ids = (
        winner.decision_id,
        winner.ref_event_id,
        winner.outbox_id,
        winner.ref_version,
    );
    assert!(
        followers
            .iter()
            .all(|o| (o.decision_id, o.ref_event_id, o.outbox_id, o.ref_version) == ids)
    );
    assert_eq!(ids.3, 3, "main: genesis, divergent change, merge");
    assert_eq!(
        delta(&before, &counts(&store, &f.g).await),
        BTreeMap::from([
            ("decisions", 1),
            ("ref_events", 1),
            ("projection_outbox", 1),
            ("idempotency", 1)
        ])
    );
    // Same key, another proposal (different digest): conflict.
    let err = store
        .workflows()
        .merge_apply(&ApplyMergeRequest {
            scope: scope_as(
                "reviewer",
                &f.g,
                "shared-apply",
                &format!("other-{}", other.proposal_id),
            ),
            proposal_id: other.proposal_id,
            preview_token: f.preview_token.clone(),
            reason: None,
            validation: ValidationPolicy::NoValidation,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, LedgerError::IdempotencyConflict), "{err:?}");
    verify_clean(&store).await;
}

// ---- transaction bound (ADR-0026 §2; M2) -------------------------------------------------------

/// ACCEPTED (M2; was `future_a_transaction_exceeding_its_bound_does_not_commit_late`, red before
/// M2: committed after 1.221 s / 1.214 s past a 500 ms bound). An accept whose in-transaction
/// work is three statements of 700 ms each — every one inside `statement_timeout` (2 s) —
/// exceeds a 1.5 s transaction bound: the check before *sending* `COMMIT` sees the bound
/// exceeded and rolls back with `DependencyTimeout`, key unused, no later than the injected
/// work plus a round trip (the injected statements are one uninterruptible unit, so this
/// exercises the pre-COMMIT check; the per-phase check is `…after_the_replay_check…` below, and
/// the statement tail is bounded by `statement_timeout`, `a_statement_timeout_inside_an_accept_…`).
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_transaction_exceeding_its_bound_does_not_commit_late() {
    let owner = store().await;
    let bound = Duration::from_millis(1500);
    let injected = Duration::from_millis(700) * 3;
    // Mechanism-level settings (statement_timeout > transaction_bound would be refused by the
    // server's start-up validation): the point is the pre-COMMIT check, not a valid deployment.
    let short = store_with_limits(
        &unique("lc-tx"),
        DbSessionLimits {
            max_connections: 2,
            statement_timeout: Duration::from_secs(2),
            lock_timeout: Duration::from_secs(1),
            transaction_bound: bound,
            ..DbSessionLimits::default()
        },
    )
    .await;
    let f = fixture(&owner, Op::Accept).await;
    let before = counts(&owner, &f.g).await;
    let hook = PauseHook::slow_statement(HookPoint::BeforeCommit, Duration::from_millis(700), 3);
    let mut handle = tokio::spawn(run(
        short.clone().with_workflow_pause_hook(hook.clone()),
        &f,
        Op::Accept,
        "tx-bound",
    ));
    reached_or_failed(&hook, &mut handle).await;
    // Measured from the hook (the injected statements start here), not from the request.
    let started = Instant::now();
    let result = handle.await.unwrap();
    let took = started.elapsed();
    assert!(
        matches!(&result, Err(LedgerError::DependencyTimeout(m)) if m.contains("exceeded its bound") && m.contains("before COMMIT")),
        "a transaction past its {bound:?} bound must not commit; got {result:?} after {took:?}"
    );
    assert!(
        took <= injected + Duration::from_millis(400),
        "rolled back at the pre-COMMIT check: {took:?}"
    );
    assert_eq!(delta(&before, &counts(&owner, &f.g).await), BTreeMap::new());
    assert_eq!(
        idempotency_rows(&owner, &f.g, "accept", "tx-bound").await,
        0
    );
    let retried = run(short.clone(), &f, Op::Accept, "tx-bound")
        .await
        .unwrap();
    assert!(!retried.replayed);
    verify_clean(&owner).await;
}

/// ACCEPTED (M2; was `future_a_transaction_paused_past_its_bound_rolls_back_before_commit`, red
/// before M2: committed). An accept paused just before `COMMIT` (every row written, no injected
/// statement) for longer than its 500 ms bound rolls back with `DependencyTimeout` when it
/// resumes and leaves the key unused. The wall-clock wait *is* the scenario here.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_transaction_paused_past_its_bound_rolls_back_before_commit() {
    let owner = store().await;
    let bound = Duration::from_millis(500);
    let named = store_with_limits(
        &unique("lc-txp"),
        DbSessionLimits {
            max_connections: 2,
            transaction_bound: bound,
            ..DbSessionLimits::default()
        },
    )
    .await;
    let f = fixture(&owner, Op::Accept).await;
    let before = counts(&owner, &f.g).await;
    let hook = PauseHook::new(HookPoint::BeforeCommit);
    let mut handle = tokio::spawn(run(
        named.clone().with_workflow_pause_hook(hook.clone()),
        &f,
        Op::Accept,
        "tx-paused",
    ));
    reached_or_failed(&hook, &mut handle).await;
    tokio::time::sleep(bound + Duration::from_millis(300)).await;
    hook.resume();
    let result = handle.await.unwrap();
    assert!(
        matches!(&result, Err(LedgerError::DependencyTimeout(m)) if m.contains("before COMMIT")),
        "a transaction resumed past its {bound:?} bound must not commit; got {result:?}"
    );
    assert_eq!(delta(&before, &counts(&owner, &f.g).await), BTreeMap::new());
    assert_eq!(
        idempotency_rows(&owner, &f.g, "accept", "tx-paused").await,
        0
    );
    let retried = run(named.clone(), &f, Op::Accept, "tx-paused")
        .await
        .unwrap();
    assert!(!retried.replayed);
}

/// ACCEPTED (M2). The deadline is checked before the statement phases, not only before
/// `COMMIT`: every write path paused right after its replay lookup (idempotency lock held,
/// nothing else run) for longer than its bound rolls back at the first phase check with
/// `DependencyTimeout` naming the phase, writes nothing, and the retry runs fresh.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_transaction_paused_past_its_bound_after_the_replay_check_rolls_back_at_the_first_phase()
{
    let owner = store().await;
    let bound = Duration::from_millis(300);
    let named = store_with_limits(
        &unique("lc-txr"),
        DbSessionLimits {
            max_connections: 4,
            transaction_bound: bound,
            ..DbSessionLimits::default()
        },
    )
    .await;
    for op in OPS {
        let f = fixture(&owner, op).await;
        let before = counts(&owner, &f.g).await;
        let hook = PauseHook::new(HookPoint::AfterReplayCheck);
        let mut handle = tokio::spawn(run(
            named.clone().with_workflow_pause_hook(hook.clone()),
            &f,
            op,
            "phase",
        ));
        reached_or_failed(&hook, &mut handle).await;
        tokio::time::sleep(bound + Duration::from_millis(200)).await;
        hook.resume();
        let result = handle.await.unwrap();
        // The very first phase check of each path, by its name in the error.
        let first_phase = match op {
            Op::Prepare | Op::BranchDelete | Op::BranchRestore | Op::MergePropose => {
                "before the graph and branch locks"
            }
            Op::Accept | Op::BranchCreate | Op::MergeApply => {
                "before the graph, ref and branch locks"
            }
            Op::Reject => "before the graph lock;",
            Op::Validate => "before the graph lock and the context insert",
        };
        assert!(
            matches!(&result, Err(LedgerError::DependencyTimeout(m)) if m.contains("exceeded its bound") && m.contains(first_phase)),
            "{op:?}: the first phase check ({first_phase}) must roll back; got {result:?}"
        );
        assert_eq!(
            delta(&before, &counts(&owner, &f.g).await),
            BTreeMap::new(),
            "{op:?}"
        );
        assert_eq!(
            idempotency_rows(&owner, &f.g, op.operation(), "phase").await,
            0,
            "{op:?}"
        );
        let retried = run(named.clone(), &f, op, "phase").await.unwrap();
        assert!(!retried.replayed, "{op:?}");
    }
    verify_clean(&owner).await;
}

/// ACCEPTED (M2, ADR-0026 §2). PostgreSQL 17's `transaction_timeout` backstop through the
/// production pool: `DbSessionLimits::pool_options` sets it to `transaction_bound +
/// statement_timeout` on a 17 server and not at all on 15. Two injected statements of 1.5 s
/// (each inside `statement_timeout` = 2 s) under a 300 ms bound: on 17 the backstop (2.3 s)
/// terminates the session before the pre-COMMIT check could run (3 s) — SQLSTATE 25P04,
/// classified `DependencyTimeout`, the backend replaced and the pool serving again (the
/// reconnect cost is measured); on 15 the pre-COMMIT check ends the transaction at ≈ 3 s. On
/// both: nothing durable, key unused, retry fresh. The backstop is a backstop: the ledger's
/// own check is what normally fires (the tests above).
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn the_pg17_transaction_timeout_backstop_fires_only_past_the_bound_plus_one_statement() {
    let owner = store().await;
    let app = unique("lc-backstop");
    let limits = DbSessionLimits {
        max_connections: 1,
        statement_timeout: Duration::from_secs(2),
        lock_timeout: Duration::from_millis(500),
        transaction_bound: Duration::from_millis(300),
        ..DbSessionLimits::default()
    };
    let bounded = store_with_limits(&app, limits).await;
    let version: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(bounded.pool())
        .await
        .unwrap();
    let pg17 = version.parse::<i64>().unwrap() >= 170000;
    let setting: String = sqlx::query_scalar("SHOW transaction_timeout")
        .fetch_one(bounded.pool())
        .await
        .map(|s: String| s)
        .unwrap_or_else(|_| "absent".into());
    if pg17 {
        assert_eq!(setting, "2300ms", "backstop = bound + statement_timeout");
    } else {
        assert_eq!(
            setting, "absent",
            "PostgreSQL 15 has no transaction_timeout"
        );
    }
    let pid_before: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(bounded.pool())
        .await
        .unwrap();
    let f = fixture(&owner, Op::Accept).await;
    let before = counts(&owner, &f.g).await;
    // The injected two-statement loop deliberately skips the ledger's own per-statement checks
    // (a simulated check-skipping bug): only then does the backstop have anything to end.
    // Mechanism-level settings (statement > bound) that start-up validation would refuse.
    let hook = PauseHook::slow_statement(HookPoint::BeforeCommit, Duration::from_millis(1500), 2);
    let mut handle = tokio::spawn(run(
        bounded.clone().with_workflow_pause_hook(hook.clone()),
        &f,
        Op::Accept,
        "backstop",
    ));
    reached_or_failed(&hook, &mut handle).await;
    let started = Instant::now();
    let result = handle.await.unwrap();
    let took = started.elapsed();
    let message = match &result {
        Err(LedgerError::DependencyTimeout(m)) => m.clone(),
        other => panic!("expected DependencyTimeout, got {other:?} after {took:?}"),
    };
    if pg17 {
        assert!(
            message.contains("transaction timeout") || message.contains("25P04"),
            "the backstop terminated the session: {message}"
        );
        assert!(
            took >= Duration::from_millis(2200) && took < Duration::from_millis(3300),
            "fired at ≈ 2.3 s after the statements began: {took:?}"
        );
    } else {
        assert!(message.contains("exceeded its bound"), "{message}");
        assert!(
            took >= Duration::from_millis(2900) && took < Duration::from_millis(4000),
            "the pre-COMMIT check at ≈ 3 s after the statements began: {took:?}"
        );
    }
    assert_eq!(delta(&before, &counts(&owner, &f.g).await), BTreeMap::new());
    assert_eq!(
        idempotency_rows(&owner, &f.g, "accept", "backstop").await,
        0
    );
    // The pool serves again; on 17 the terminated backend was replaced (reconnect cost).
    let reconnect_started = Instant::now();
    let pid_after: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(bounded.pool())
        .await
        .unwrap();
    let reconnect = reconnect_started.elapsed();
    if pg17 {
        assert_ne!(
            pid_after, pid_before,
            "a new backend replaced the terminated one"
        );
    }
    let retried = run(bounded.clone(), &f, Op::Accept, "backstop")
        .await
        .unwrap();
    assert!(!retried.replayed);
    println!(
        "server {version}: transaction_timeout setting {setting}; bound exceeded after {took:?} ({}); next acquire + statement {reconnect:?}",
        if pg17 {
            "25P04 backstop"
        } else {
            "pre-COMMIT check"
        }
    );
}

/// ACCEPTED (M2, ADR-0026 §3). A connection idle in the pool for longer than the transaction
/// bound is borrowed normally afterwards: none of the session timeouts is armed outside a
/// statement or an open transaction, and no transaction-local state leaks to the next
/// borrower (the backend pid is unchanged on both servers; a full accept runs on it).
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_connection_idle_in_the_pool_longer_than_the_bound_is_borrowed_normally() {
    let owner = store().await;
    // Every timer short enough that the idle wait exceeds all of them: bound 300 ms, PostgreSQL
    // 17 backstop = bound + statement = 500 ms, idle-in-transaction 400 ms.
    let one = store_with_limits(
        &unique("lc-idle"),
        DbSessionLimits {
            max_connections: 1,
            statement_timeout: Duration::from_millis(200),
            lock_timeout: Duration::from_millis(100),
            idle_in_transaction_timeout: Duration::from_millis(400),
            transaction_bound: Duration::from_millis(300),
            ..DbSessionLimits::default()
        },
    )
    .await;
    let identity = || async {
        let row = sqlx::query(
            "SELECT pg_backend_pid() AS pid, current_setting('statement_timeout') AS st, \
             current_setting('lock_timeout') AS lt, \
             current_setting('idle_in_transaction_session_timeout') AS it, \
             (SELECT backend_start::text FROM pg_stat_activity WHERE pid = pg_backend_pid()) AS started",
        )
        .fetch_one(one.pool())
        .await
        .unwrap();
        (
            row.get::<i32, _>("pid"),
            row.get::<String, _>("st"),
            row.get::<String, _>("lt"),
            row.get::<String, _>("it"),
            row.get::<String, _>("started"),
        )
    };
    let before = identity().await;
    assert_eq!(
        (&before.1[..], &before.2[..], &before.3[..]),
        ("200ms", "100ms", "400ms")
    );
    // The idle wait is the scenario: longer than the bound, the backstop and the idle timeout.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let after = identity().await;
    assert_eq!(
        after, before,
        "the same backend, started at the same time, with the same session settings"
    );
    // It serves a whole accept (several statements and a COMMIT inside a bounded transaction).
    let f = fixture(&owner, Op::Accept).await;
    let r = run(one.clone(), &f, Op::Accept, "idle").await.unwrap();
    assert!(!r.replayed);
    assert_eq!(identity().await, before);
}

/// The named store's backends as `(state, in a transaction?)`, from `pg_stat_activity`.
async fn transaction_states(owner: &PostgresLedgerStore, app: &str) -> Vec<(String, bool)> {
    sqlx::query(
        "SELECT coalesce(state, '') AS state, xact_start IS NOT NULL AS in_xact \
         FROM pg_stat_activity WHERE application_name = $1 ORDER BY pid",
    )
    .bind(app)
    .fetch_all(owner.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get::<String, _>("state"), r.get::<bool, _>("in_xact")))
    .collect()
}

/// ACCEPTED (M2 review P1, ADR-0026 §8). A request budget that ends after the connection was
/// acquired but before `BEGIN` returns the clean connection: nothing is sent on it, the backend
/// is `idle` with no open transaction, nothing is written, the key is unused, and the next
/// borrower of the one-connection pool begins and commits a normal accept on the same backend.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_request_budget_that_ends_before_begin_returns_the_connection_without_a_transaction() {
    let owner = store().await;
    let app = unique("lc-prebegin");
    let one = named_store(&app, 1).await;
    let f = fixture(&owner, Op::Accept).await;
    let before = counts(&owner, &f.g).await;
    let hook = PauseHook::new(HookPoint::BeforeBegin);
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut handle = tokio::spawn(ledger_store::lifecycle::with_request_deadline(
        deadline,
        run(
            one.clone().with_workflow_pause_hook(hook.clone()),
            &f,
            Op::Accept,
            "pre-begin",
        ),
    ));
    reached_or_failed(&hook, &mut handle).await;
    let pid_before = sessions(&owner, &app).await;
    assert_eq!(
        pid_before.len(),
        1,
        "the connection is acquired: {pid_before:?}"
    );
    // The wait is the scenario: the budget ends while the operation holds the connection
    // and has not sent BEGIN.
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(50),
    ))
    .await;
    hook.resume();
    let result = handle.await.unwrap();
    assert!(
        matches!(&result, Err(LedgerError::DependencyUnavailable(m)) if m.contains("before the transaction began")),
        "nothing begun: {result:?}"
    );
    wait_until(
        async || transaction_states(&owner, &app).await == vec![("idle".to_owned(), false)],
        "the connection to be back in the pool idle and outside any transaction",
    )
    .await;
    assert_eq!(locks_held(&owner, &app).await, 0);
    assert_eq!(delta(&before, &counts(&owner, &f.g).await), BTreeMap::new());
    assert_eq!(
        idempotency_rows(&owner, &f.g, "accept", "pre-begin").await,
        0
    );
    // The next borrower: a fresh transaction on the same backend, committed normally.
    let retried = run(one.clone(), &f, Op::Accept, "pre-begin").await.unwrap();
    assert!(!retried.replayed);
    let after = sessions(&owner, &app).await;
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].pid, pid_before[0].pid,
        "the same pooled backend served the retry"
    );
    assert_eq!(
        transaction_states(&owner, &app).await,
        vec![("idle".to_owned(), false)]
    );
    verify_clean(&owner).await;
}

/// ACCEPTED (M2 review P1, ADR-0026 §8). The caller's future is dropped while `BEGIN` is in
/// flight on the server (a slow `BEGIN` injected by the hook, the window in which sqlx 0.8.6
/// has not yet counted the transaction). The begin runs to completion in its own task and its
/// transaction is dropped there (rolled back), so the pool gets the connection back `idle`,
/// outside any transaction, holding no locks; the next borrower begins and commits a normal
/// accept on the same backend. With a begin raced against a client-side timer this connection
/// came back `idle in transaction` and the next borrower ran inside the stale transaction.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_caller_dropped_during_begin_never_returns_an_open_transaction_to_the_pool() {
    let owner = store().await;
    let app = unique("lc-begindrop");
    let one = named_store(&app, 1).await;
    let f = fixture(&owner, Op::Accept).await;
    let before = counts(&owner, &f.g).await;
    let hook = PauseHook::slow_begin(Duration::from_millis(800));
    let handle = tokio::spawn(run(
        one.clone().with_workflow_pause_hook(hook.clone()),
        &f,
        Op::Accept,
        "begin-drop",
    ));
    hook.reached().await;
    // BEGIN is in flight: the backend is active inside the injected sleep.
    wait_until(
        async || sessions(&owner, &app).await.iter().any(active_in_sleep),
        "BEGIN to be in flight on the server",
    )
    .await;
    let pid = sessions(&owner, &app).await[0].pid;
    handle.abort();
    let aborted = handle.await;
    assert!(
        aborted.is_err_and(|e| e.is_cancelled()),
        "the caller was dropped mid-BEGIN"
    );
    // Once the server finishes BEGIN the begin task drops its transaction: ROLLBACK, then the
    // connection returns to the pool. Never `idle in transaction`.
    wait_until(
        async || {
            let states = transaction_states(&owner, &app).await;
            states.iter().all(|(state, _)| state != "active") && !states.is_empty()
        },
        "the slow BEGIN to end",
    )
    .await;
    wait_until(
        async || transaction_states(&owner, &app).await == vec![("idle".to_owned(), false)],
        "the connection to be idle outside any transaction",
    )
    .await;
    assert_eq!(locks_held(&owner, &app).await, 0);
    assert_eq!(delta(&before, &counts(&owner, &f.g).await), BTreeMap::new());
    assert_eq!(
        idempotency_rows(&owner, &f.g, "accept", "begin-drop").await,
        0
    );
    // The next borrower of the same backend begins its own transaction and commits.
    let retried = run(one.clone(), &f, Op::Accept, "begin-drop")
        .await
        .unwrap();
    assert!(!retried.replayed);
    let after = sessions(&owner, &app).await;
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].pid, pid,
        "the same pooled backend served the retry"
    );
    assert_eq!(
        transaction_states(&owner, &app).await,
        vec![("idle".to_owned(), false)]
    );
    verify_clean(&owner).await;
}

/// ACCEPTED (M2 review P1, ADR-0026 §2; PostgreSQL 15 is the target: it has no backstop).
/// Six real statements, each well below `statement_timeout`, run inside a transaction whose
/// bound is 300 ms. Each goes through the production checked accessor, so the sequence stops
/// at the first statement that would start past the deadline: the overshoot is one in-flight
/// statement tail, not the six sequential tails (1.5 s) an unchecked sequence would take. On
/// PostgreSQL 17 the session-terminating backstop (bound + statement = 2.3 s) is never reached
/// — defence in depth only.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn statements_cannot_keep_starting_after_the_transaction_deadline_without_a_backstop() {
    let owner = store().await;
    let app = unique("lc-perstmt");
    let bound = Duration::from_millis(300);
    let each = Duration::from_millis(250);
    let bounded = store_with_limits(
        &app,
        DbSessionLimits {
            max_connections: 1,
            statement_timeout: Duration::from_secs(2),
            lock_timeout: Duration::from_millis(500),
            transaction_bound: bound,
            ..DbSessionLimits::default()
        },
    )
    .await;
    let version: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(bounded.pool())
        .await
        .unwrap();
    let backstop: Result<String, _> = sqlx::query_scalar("SHOW transaction_timeout")
        .fetch_one(bounded.pool())
        .await;
    let f = fixture(&owner, Op::Accept).await;
    let before = counts(&owner, &f.g).await;
    let hook = PauseHook::slow_statement_checked(HookPoint::BeforeCommit, each, 6);
    let mut handle = tokio::spawn(run(
        bounded.clone().with_workflow_pause_hook(hook.clone()),
        &f,
        Op::Accept,
        "per-stmt",
    ));
    reached_or_failed(&hook, &mut handle).await;
    let started = Instant::now();
    let result = handle.await.unwrap();
    let took = started.elapsed();
    println!(
        "server {version}: transaction_timeout {}; six {each:?} statements under a {bound:?} bound stopped after {took:?}: {result:?}",
        backstop.as_deref().unwrap_or("absent")
    );
    assert!(
        matches!(&result, Err(LedgerError::DependencyTimeout(m)) if m.contains("exceeded its bound") && m.contains("before an injected slow statement")),
        "the production deadline check refuses the next statement: {result:?}"
    );
    // At most two statements started (the second before 300 ms); the third was refused. The
    // overshoot past the deadline is one statement tail (≤ 250 ms), never 6 × 250 ms.
    assert!(
        took >= each && took < bound + each + Duration::from_millis(250),
        "one in-flight statement tail at most: {took:?}"
    );
    assert_eq!(delta(&before, &counts(&owner, &f.g).await), BTreeMap::new());
    assert_eq!(
        idempotency_rows(&owner, &f.g, "accept", "per-stmt").await,
        0
    );
    let retried = run(bounded.clone(), &f, Op::Accept, "per-stmt")
        .await
        .unwrap();
    assert!(!retried.replayed);
    verify_clean(&owner).await;
}

/// ACCEPTED (M2, ADR-0026 §4). An error from `COMMIT` itself whose class is a lost connection
/// is reported as `CommitOutcomeUnknown` (never as a rollback): the transaction, paused just
/// before `COMMIT`, has its backend terminated from another session; `COMMIT` then fails on
/// the dead connection. Here the server did roll back (the session is gone), so nothing is
/// durable, the key is unused, and the retry by key runs fresh — which is exactly what the
/// "outcome unknown; retry with the same key" guidance resolves to.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn a_commit_on_a_terminated_connection_is_outcome_unknown_and_the_retry_runs_fresh() {
    let owner = store().await;
    let app = unique("lc-commit");
    let one = named_store(&app, 1).await;
    let f = fixture(&owner, Op::Accept).await;
    let before = counts(&owner, &f.g).await;
    let hook = PauseHook::new(HookPoint::BeforeCommit);
    let mut handle = tokio::spawn(run(
        one.clone().with_workflow_pause_hook(hook.clone()),
        &f,
        Op::Accept,
        "commit-lost",
    ));
    reached_or_failed(&hook, &mut handle).await;
    let paused: Vec<Session> = sessions(&owner, &app)
        .await
        .into_iter()
        .filter(|s| s.state == "idle in transaction")
        .collect();
    assert_eq!(
        paused.len(),
        1,
        "exactly one transaction is paused before COMMIT: {paused:?}"
    );
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(paused[0].pid)
        .fetch_one(owner.pool())
        .await
        .unwrap();
    assert!(terminated);
    hook.resume();
    let result = handle.await.unwrap();
    match &result {
        Err(LedgerError::CommitOutcomeUnknown(inner)) => assert!(
            matches!(
                **inner,
                LedgerError::DependencyUnavailable(_) | LedgerError::DependencyTimeout(_)
            ),
            "{inner:?}"
        ),
        other => panic!("COMMIT on a terminated connection must be outcome unknown, got {other:?}"),
    }
    assert_eq!(delta(&before, &counts(&owner, &f.g).await), BTreeMap::new());
    assert_eq!(
        idempotency_rows(&owner, &f.g, "accept", "commit-lost").await,
        0
    );
    let retried = run(one.clone(), &f, Op::Accept, "commit-lost")
        .await
        .unwrap();
    assert!(
        !retried.replayed,
        "the key was unused: the retry executes afresh"
    );
    verify_clean(&owner).await;
}

/// CHARACTERIZATION (Plan 0013 M1 review residual, documented contract). `immutable_objects` is
/// content-addressed and shared by every graph and tenant. Two prepares of byte-identical
/// patches (here two graphs; two tenants behave identically) race on the same object row:
/// the second insert waits for the first transaction to end, and under the shortened
/// `lock_timeout` that wait can legitimately surface as `DEPENDENCY_TIMEOUT` (55P03) — a
/// retryable outcome, not a fault: once the first transaction commits the retry succeeds
/// (`ON CONFLICT DO NOTHING`) and writes its own graph's rows. The stress and fault
/// workloads never produce identical content, so the gate's `DEPENDENCY_TIMEOUT` policy stays.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via the PostgreSQL suites with LEDGER_TEST_DATABASE_URL"]
async fn an_identical_object_published_by_an_uncommitted_transaction_makes_the_second_prepare_wait()
{
    let owner = store().await;
    let short = store_with(
        &unique("lc-obj"),
        2,
        Duration::from_secs(10),
        Duration::from_millis(300),
    )
    .await;
    let a = fixture(&owner, Op::Prepare).await;
    let b = fixture(&owner, Op::Prepare).await;
    let before_b = counts(&owner, &b.g).await;
    // Content no earlier test has published (objects are global): the same quad in both
    // graphs gives byte-identical requested and effective patches, hence one object row.
    let quad = format!("<urn:shared:{}> <urn:p> \"1\" .", unique("o"));
    let req_a = prepare_req(&a.g, "main", a.head.clone(), "obj-a", &[&quad]);
    let req_b = prepare_req(&b.g, "main", b.head.clone(), "obj-b", &[&quad]);
    // A publishes the patch/candidate objects and pauses before COMMIT.
    let hook = PauseHook::new(HookPoint::BeforeCommit);
    let held = tokio::spawn({
        let hooked = owner.clone().with_workflow_pause_hook(hook.clone());
        async move { hooked.workflows().prepare(&req_a).await }
    });
    hook.reached().await;
    // B prepares the identical content in another graph.
    let started = Instant::now();
    let err = short.workflows().prepare(&req_b).await.unwrap_err();
    let took = started.elapsed();
    assert!(matches!(err, LedgerError::DependencyTimeout(_)), "{err:?}");
    assert!(
        took >= Duration::from_millis(250) && took < Duration::from_secs(3),
        "B waited for A's uncommitted object insert until lock_timeout: {took:?}"
    );
    assert_eq!(
        delta(&before_b, &counts(&owner, &b.g).await),
        BTreeMap::new()
    );
    hook.resume();
    held.await.unwrap().unwrap();
    // A committed; B's retry finds the shared object and writes its own rows.
    let retried = short.workflows().prepare(&req_b).await.unwrap();
    assert!(!retried.replayed);
    assert!(delta(&before_b, &counts(&owner, &b.g).await).contains_key("proposals"));
    verify_clean(&owner).await;
    println!(
        "shared-object wait surfaced as DEPENDENCY_TIMEOUT after {took:?}; the retry succeeded"
    );
}
