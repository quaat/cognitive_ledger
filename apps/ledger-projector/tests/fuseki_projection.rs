//! Projection against a **real Fuseki** (pinned image, `deploy/fuseki/ledger-projection.ttl`)
//! and real PostgreSQL (Plan 0007 scenarios): genesis, advance, duplicate / stale /
//! equal-version writes, target outage and catch-up, a write that committed behind a lost
//! response, every crash window at genesis and over a predecessor, lost / corrupt / ahead /
//! foreign markers, a stale replacement racing a newer projection, reconciliation of a target
//! that lost its data, the target's literal canonicalization, concurrent workers, many
//! graphs, unsupported state and the transactional probe.
//!
//! Assertions read the target with **raw SPARQL** (reqwest + the ledger's own quad parser),
//! never through the adapter under test. The tests share one TDB2 dataset whose single writer
//! commits in ~0.5–1 s, so they run one at a time (a process-wide lock, [`serial`]).
//!
//! Requires `LEDGER_TEST_DATABASE_URL` (owner), `LEDGER_TEST_FUSEKI_URL` (dataset base URL,
//! e.g. `http://127.0.0.1:53030/ledger`) and `LEDGER_TEST_FUSEKI_PASSWORD` (the `admin`
//! password). Every test is `#[ignore]`.

use ledger_core::{
    AuthenticatedPrincipal, CommitId, ContentId, GraphId, PrincipalId, PrincipalType, TenantId,
};
use ledger_projection::{
    CognitiveGraph, ErrorClass, LP_NAMESPACE, MARKER_GRAPH, MarkerTerm, Observation,
    ProjectedState, ProjectionClient, ProjectionError, ProjectionErrorCode, ProjectionMarker,
    WriteMode,
};
use ledger_projection_fuseki::{FusekiClient, FusekiConfig, TargetCredentials};
use ledger_projector::{FailPoint, Projector, ProjectorConfig, StepOutcome, metrics::Metrics};
use ledger_rdf::{Operation, OperationKind, Patch, Quad};
use ledger_store::{
    AcceptRequest, GraphStatus, NewGraph, PostgresLedgerStore, PrepareRequest,
    ProjectionRepository, ReconstructionLimits, RequestScope, StreamKey, V1Binding,
    ValidationPolicy,
};
use sqlx::Connection;
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// One test at a time against the shared dataset (see the module doc).
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
}

fn quads(lines: &[&str]) -> BTreeSet<Quad> {
    lines.iter().map(|q| q.parse().unwrap()).collect()
}

/// The projector identity used by every test in this process (created once, ADR-0021).
async fn projector_url() -> String {
    static URL: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    URL.get_or_init(|| async {
        let url = env("LEDGER_TEST_DATABASE_URL");
        let role = format!("it_projector_{}", std::process::id());
        let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
        ledger_store::schema::migrate_all_on(&mut conn)
            .await
            .unwrap();
        sqlx::query(&format!(
            "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN \
             CREATE ROLE {role} LOGIN PASSWORD 'it-projector-secret'; END IF; END $$"
        ))
        .execute(&mut conn)
        .await
        .unwrap();
        ledger_store::schema::grant_projector_role(&mut conn, &role)
            .await
            .unwrap();
        let (scheme, rest) = url.split_once("://").unwrap();
        let (_, host) = rest.rsplit_once('@').unwrap();
        format!("{scheme}://{role}:it-projector-secret@{host}")
    })
    .await
    .clone()
}

fn fuseki(base: &str) -> Arc<FusekiClient> {
    fuseki_as(base, &env("LEDGER_TEST_FUSEKI_PASSWORD"))
}

fn fuseki_as(base: &str, password: &str) -> Arc<FusekiClient> {
    Arc::new(
        FusekiClient::new(FusekiConfig {
            query_endpoint: format!("{base}/query"),
            update_endpoint: format!("{base}/update"),
            credentials: TargetCredentials::Basic {
                username: "admin".into(),
                password: password.into(),
            },
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(10),
            max_update_bytes: 16 * 1024 * 1024,
            max_response_bytes: 16 * 1024 * 1024,
            allow_insecure_loopback: true,
        })
        .unwrap(),
    )
}

fn target() -> Arc<FusekiClient> {
    fuseki(&env("LEDGER_TEST_FUSEKI_URL"))
}

/// A target nothing listens on (outage).
fn dead_target() -> Arc<FusekiClient> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    fuseki(&format!("http://127.0.0.1:{port}/ledger"))
}

/// The real Fuseki client with scripted misbehaviour around `write`.
struct Scripted {
    inner: Arc<FusekiClient>,
    /// Hold the next write until notified (a stalled worker).
    hold: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Hold the next observation until notified (a worker stalled between selecting its
    /// work and looking at the target, e.g. a delayed claim reply).
    hold_observe: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Forward the next write, then report a timeout (the target committed; the response
    /// was lost).
    lose_response: AtomicBool,
    /// Report success for writes without forwarding them (a target that acknowledges and
    /// loses the request).
    swallow: AtomicBool,
    /// Answer every containment check with `false`.
    deny_containment: AtomicBool,
    /// Fail the transactional probe.
    fail_probe: AtomicBool,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: target(),
            hold: Mutex::new(None),
            hold_observe: Mutex::new(None),
            lose_response: AtomicBool::new(false),
            swallow: AtomicBool::new(false),
            deny_containment: AtomicBool::new(false),
            fail_probe: AtomicBool::new(false),
        })
    }
}

#[async_trait::async_trait]
impl ProjectionClient for Scripted {
    async fn observe(&self, graph: &CognitiveGraph) -> Result<Observation, ProjectionError> {
        let gate = self.hold_observe.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.inner.observe(graph).await
    }

    async fn write(
        &self,
        graph: &CognitiveGraph,
        state: &ProjectedState,
        marker: &ProjectionMarker,
        mode: WriteMode,
        expected: &[(String, MarkerTerm)],
    ) -> Result<(), ProjectionError> {
        let gate = self.hold.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        if self.swallow.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.inner
            .write(graph, state, marker, mode, expected)
            .await?;
        if self.lose_response.swap(false, Ordering::SeqCst) {
            return Err(ProjectionError::retryable(
                ProjectionErrorCode::TargetTimeout,
                "response lost after the target committed",
            ));
        }
        Ok(())
    }

    async fn fence(
        &self,
        graph: &CognitiveGraph,
        expected: &[(String, MarkerTerm)],
        write_id: &str,
    ) -> Result<(), ProjectionError> {
        self.inner.fence(graph, expected, write_id).await
    }

    async fn read_graph(&self, graph: &CognitiveGraph) -> Result<BTreeSet<Quad>, ProjectionError> {
        self.inner.read_graph(graph).await
    }

    async fn contains_all(
        &self,
        graph: &CognitiveGraph,
        state: &ProjectedState,
    ) -> Result<bool, ProjectionError> {
        if self.deny_containment.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.inner.contains_all(graph, state).await
    }

    async fn bind_target(&self, target_id: &str) -> Result<(), ProjectionError> {
        self.inner.bind_target(target_id).await
    }

    async fn probe_transactional(&self) -> Result<(), ProjectionError> {
        if self.fail_probe.load(Ordering::SeqCst) {
            return Err(ProjectionError::permanent(
                ProjectionErrorCode::TargetNotTransactional,
                "scripted: the failed update left its insert behind",
            ));
        }
        self.inner.probe_transactional().await
    }

    fn describe(&self) -> String {
        "scripted".into()
    }
}

// ---- independent target reads (raw SPARQL, not the adapter under test) -----------------

async fn raw_query(query: &str, accept: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{}/query", env("LEDGER_TEST_FUSEKI_URL")))
        .header("content-type", "application/sparql-query")
        .header("accept", accept)
        .body(query.to_owned())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    response.text().await.unwrap()
}

async fn raw_update(sparql: &str) {
    let response = reqwest::Client::new()
        .post(format!("{}/update", env("LEDGER_TEST_FUSEKI_URL")))
        .basic_auth("admin", Some(env("LEDGER_TEST_FUSEKI_PASSWORD")))
        .header("content-type", "application/sparql-update")
        .body(sparql.to_owned())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
}

/// The cognitive graph's triples as the target returns them.
async fn raw_graph(graph: &CognitiveGraph) -> BTreeSet<Quad> {
    raw_query(
        &format!("CONSTRUCT {{ ?s ?p ?o }} WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}"),
        "application/n-triples",
    )
    .await
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(|l| l.parse().unwrap())
    .collect()
}

/// The marker as the target returns it: predicate local name → object values.
async fn raw_marker(graph: &CognitiveGraph) -> HashMap<String, Vec<String>> {
    let body = raw_query(
        &format!("SELECT ?p ?o WHERE {{ GRAPH <{MARKER_GRAPH}> {{ <{graph}> ?p ?o }} }}"),
        "application/sparql-results+json",
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for b in json["results"]["bindings"].as_array().unwrap() {
        let p = b["p"]["value"].as_str().unwrap();
        let local = p.strip_prefix(LP_NAMESPACE).unwrap_or(p).to_owned();
        out.entry(local)
            .or_default()
            .push(b["o"]["value"].as_str().unwrap().to_owned());
    }
    out
}

/// The target holds exactly `expected` (canonical test literals) and one well-formed marker
/// naming `(ledger_graph, main, commit, version)` with the digest of `expected` and the
/// target's own triple count.
async fn assert_projected(
    graph: &CognitiveGraph,
    ledger_graph: &GraphId,
    expected: &BTreeSet<Quad>,
    commit: &CommitId,
    version: i64,
) {
    assert_eq!(
        &raw_graph(graph).await,
        expected,
        "graph content of {graph}"
    );
    let marker = raw_marker(graph).await;
    let one = |k: &str| {
        let v = marker
            .get(k)
            .unwrap_or_else(|| panic!("marker lacks {k}: {marker:?}"));
        assert_eq!(v.len(), 1, "{k}: {v:?}");
        v[0].clone()
    };
    assert_eq!(one("graphId"), ledger_graph.as_str());
    assert_eq!(one("branch"), "main");
    assert_eq!(one("commitId"), commit.to_string());
    assert_eq!(one("refVersion"), version.to_string());
    assert_eq!(one("tripleCount"), expected.len().to_string());
    assert_eq!(
        one("stateDigest"),
        ledger_rdf::state_digest(expected).to_string()
    );
    assert!(!one("writeId").is_empty());
    assert_eq!(
        marker.len(),
        8,
        "exactly the protocol predicates: {marker:?}"
    );
}

fn marker(
    g: &GraphId,
    commit: &CommitId,
    version: i64,
    state: &BTreeSet<Quad>,
) -> ProjectionMarker {
    ProjectionMarker {
        graph_id: g.clone(),
        branch: "main".into(),
        commit: commit.clone(),
        ref_version: version,
        state_digest: ledger_rdf::state_digest(state),
        triple_count: 0, // target-computed
        write_id: unique("test-write"),
    }
}

struct World {
    store: PostgresLedgerStore,
    target_id: String,
    /// The tenant of every graph this world created.
    tenants: Mutex<HashMap<GraphId, TenantId>>,
}

impl World {
    async fn new() -> Self {
        let _ = projector_url().await;
        Self {
            store: PostgresLedgerStore::connect_and_migrate(
                &env("LEDGER_TEST_DATABASE_URL"),
                V1Binding::Reject,
            )
            .await
            .unwrap(),
            target_id: unique("fuseki"),
            tenants: Mutex::new(HashMap::new()),
        }
    }

    fn owner(&self) -> ProjectionRepository {
        ProjectionRepository::new(self.store.pool().clone())
    }

    async fn projector(
        &self,
        client: Arc<dyn ProjectionClient>,
        owner: &str,
        failpoint: Option<FailPoint>,
    ) -> Projector<dyn ProjectionClient> {
        self.projector_with(client, owner, failpoint, Arc::new(Metrics::default()))
            .await
    }

    async fn projector_with(
        &self,
        client: Arc<dyn ProjectionClient>,
        owner: &str,
        failpoint: Option<FailPoint>,
        metrics: Arc<Metrics>,
    ) -> Projector<dyn ProjectionClient> {
        let repo = ProjectionRepository::connect(
            &projector_url().await,
            ledger_store::DbSessionLimits::default(),
        )
        .await
        .expect("the projector identity verifies");
        let projector = Projector::new(
            repo,
            client,
            ProjectorConfig {
                target_id: self.target_id.clone(),
                owner: owner.into(),
                lease_ttl: Duration::from_secs(300),
                reconstruction: ReconstructionLimits::DEVELOPMENT,
                backoff_base: Duration::from_millis(10),
                backoff_max: Duration::from_millis(50),
                poll_interval: Duration::from_millis(20),
                concurrency: 2,
                reconcile_interval: Duration::from_secs(3600),
                probe_interval: Duration::from_secs(3600),
            },
            metrics,
        );
        match failpoint {
            Some(point) => projector.with_failpoint(point),
            None => projector,
        }
    }

    async fn healthy(&self) -> Projector<dyn ProjectionClient> {
        self.projector(target(), &unique("w"), None).await
    }

    /// A graph of knowledge base `kb`, projection enabled for this world's target.
    async fn graph_with_kb(&self, kb: &str) -> (GraphId, CognitiveGraph) {
        self.graph_of("tenant-it", kb).await
    }

    async fn graph_of(&self, tenant: &str, kb: &str) -> (GraphId, CognitiveGraph) {
        let id = GraphId::new(unique("pg")).unwrap();
        let tenant = TenantId::new(tenant).unwrap();
        self.tenants
            .lock()
            .unwrap()
            .insert(id.clone(), tenant.clone());
        self.store
            .graphs()
            .create(&NewGraph {
                graph_id: id.clone(),
                tenant_id: tenant,
                knowledge_base_id: Some(kb.to_owned()),
                purpose: None,
                status: GraphStatus::Active,
            })
            .await
            .unwrap();
        ProjectionRepository::new(self.store.pool().clone())
            .enable(&self.key(&id), |kb| {
                CognitiveGraph::for_knowledge_base(kb)
                    .map(|g| g.as_iri().to_owned())
                    .map_err(|e| e.to_string())
            })
            .await
            .unwrap();
        (id, CognitiveGraph::for_knowledge_base(kb).unwrap())
    }

    /// A graph with its own KB (hence its own cognitive graph).
    async fn graph(&self) -> (GraphId, CognitiveGraph) {
        self.graph_with_kb(&format!("urn:it:kb:{}", unique("kb")))
            .await
    }

    fn key(&self, graph: &GraphId) -> StreamKey {
        StreamKey {
            graph_id: graph.clone(),
            branch: "main".into(),
            target_id: self.target_id.clone(),
        }
    }

    /// Prepare + accept quads on `main`; acceptance never depends on projection.
    async fn accept(&self, graph: &GraphId, head: Option<&CommitId>, quads: &[&str]) -> CommitId {
        let tenant = self.tenants.lock().unwrap()[graph].clone();
        let scope = |key: String| RequestScope {
            principal: AuthenticatedPrincipal {
                principal_id: PrincipalId::new("urn:it:agent").unwrap(),
                principal_type: PrincipalType::Agent,
                tenant_id: tenant.clone(),
                on_behalf_of: None,
            },
            graph: graph.clone(),
            request_digest: ContentId::for_bytes(key.as_bytes()),
            idempotency_key: key,
            correlation_id: None,
        };
        let tag = unique("k");
        let prepared = self
            .store
            .workflows()
            .prepare(&PrepareRequest {
                scope: scope(format!("p-{tag}")),
                branch: "main".into(),
                expected_head: head.cloned(),
                requested: Patch::new(quads.iter().map(|q| Operation {
                    kind: OperationKind::Add,
                    quad: q.parse().unwrap(),
                }))
                .unwrap(),
                activity: "cognitive-correction".into(),
                event_time: None,
                evidence_refs: vec![],
                source_system: None,
                message: "it".into(),
            })
            .await
            .unwrap();
        self.store
            .workflows()
            .accept(&AcceptRequest {
                scope: scope(format!("a-{tag}")),
                branch: "main".into(),
                expected_head: head.cloned(),
                candidate: prepared.candidate.clone(),
                reason: None,
                validation: ValidationPolicy::NoValidation,
            })
            .await
            .unwrap();
        prepared.candidate
    }

    async fn status(&self, graph: &GraphId) -> ledger_store::StreamStatus {
        ProjectionRepository::new(self.store.pool().clone())
            .status(Some(&self.target_id))
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.key.graph_id == *graph)
            .unwrap()
    }

    async fn owner_sql(&self, sql: &str, graph: &GraphId) {
        sqlx::query(sql)
            .bind(graph.as_str())
            .bind(&self.target_id)
            .execute(self.store.pool())
            .await
            .unwrap();
    }

    /// Expire the stream's lease as if its holder crashed long ago (owner-side, test only;
    /// deterministic instead of sleeping past the TTL).
    async fn expire_lease(&self, graph: &GraphId) {
        self.owner_sql(
            "UPDATE projection_state SET lease_until = now() - interval '1 second' \
             WHERE graph_id = $1 AND target_id = $2 AND lease_until IS NOT NULL",
            graph,
        )
        .await;
    }

    /// Skip backoff so a test can retry immediately (owner-side, test only).
    async fn due_now(&self, graph: &GraphId) {
        self.owner_sql(
            "UPDATE projection_state SET next_attempt_at = now() \
             WHERE graph_id = $1 AND target_id = $2",
            graph,
        )
        .await;
    }

    /// Make an idle stream due for reconciliation (owner-side, test only).
    async fn reconcile_due(&self, graph: &GraphId) {
        self.owner_sql(
            "UPDATE projection_state SET last_success_at = now() - interval '2 hours' \
             WHERE graph_id = $1 AND target_id = $2",
            graph,
        )
        .await;
    }
}

const Q1: &str = "<urn:m:a> <urn:label> \"A\" .";
const Q2: &str = "<urn:m:b> <urn:label> \"B\"@en .";
const Q3: &str = "<urn:m:c> <urn:weight> \"3\"^^<http://www.w3.org/2001/XMLSchema#integer> .";

fn projected(version: i64, rebuilt: bool) -> StepOutcome {
    StepOutcome::Projected { version, rebuilt }
}

fn acknowledged(version: i64) -> StepOutcome {
    StepOutcome::Acknowledged {
        version,
        wrote: false,
    }
}

fn assert_failed(outcome: StepOutcome, code: ProjectionErrorCode, class: ErrorClass) {
    match outcome {
        StepOutcome::Failed { code: c, class: k } => assert_eq!((c, k), (code, class)),
        other => panic!("expected {code:?}/{class:?}, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn the_target_is_transactional() {
    let _serial = serial().await;
    target()
        .probe_transactional()
        .await
        .expect("TDB2 rolls a failed update back");
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn genesis_advance_and_duplicate_stale_or_equal_version_writes() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let p = w.healthy().await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    assert_eq!(p.step().await.unwrap(), projected(1, false));
    let s1 = quads(&[Q1]);
    assert_projected(&cg, &g, &s1, &c1, 1).await;
    assert_eq!(p.step().await.unwrap(), StepOutcome::Idle, "nothing left");
    let c2 = w.accept(&g, Some(&c1), &[Q2, Q3]).await;
    assert_eq!(p.step().await.unwrap(), projected(2, false));
    let s2 = quads(&[Q1, Q2, Q3]);
    assert_projected(&cg, &g, &s2, &c2, 2).await;
    let status = w.status(&g).await;
    assert_eq!(
        (
            status.projected_ref_version,
            status.lag_versions(),
            status.pending_events,
            status.rebuilds
        ),
        (Some(2), 0, 0, 0)
    );
    // A duplicate v2, a stale v1, and a v2 write carrying other content, each planned from
    // the current observation: all no-ops.
    let client = target();
    for (state, m) in [
        (&s2, marker(&g, &c2, 2, &s2)),
        (&s1, marker(&g, &c1, 1, &s1)),
        (&s1, marker(&g, &c2, 2, &s1)),
    ] {
        let observed = client.observe(&cg).await.unwrap();
        client
            .write(
                &cg,
                &ProjectedState::from_state(state).unwrap(),
                &m,
                WriteMode::Conditional,
                &observed.terms,
            )
            .await
            .unwrap();
        assert_projected(&cg, &g, &s2, &c2, 2).await;
    }
    // A replacement planned from an observation that is no longer current: a no-op.
    let stale = client.observe(&cg).await.unwrap();
    raw_update(&format!(
        "INSERT DATA {{ GRAPH <{cg}> {{ <urn:x> <urn:y> \"stray\" }} }}"
    ))
    .await;
    raw_update(&format!(
        "DELETE WHERE {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> <{LP_NAMESPACE}tripleCount> ?n }} }} ; \
         INSERT DATA {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> <{LP_NAMESPACE}tripleCount> 4 }} }}"
    ))
    .await;
    client
        .write(
            &cg,
            &ProjectedState::from_state(&s1).unwrap(),
            &marker(&g, &c1, 1, &s1),
            WriteMode::Replace,
            &stale.terms,
        )
        .await
        .unwrap();
    assert_eq!(
        raw_graph(&cg).await.len(),
        4,
        "the stale replacement wrote nothing"
    );
    // A duplicate write never adds a second count value.
    let current = client.observe(&cg).await.unwrap();
    client
        .write(
            &cg,
            &ProjectedState::from_state(&s2).unwrap(),
            &marker(&g, &c2, 2, &s2),
            WriteMode::Conditional,
            &current.terms,
        )
        .await
        .unwrap();
    assert_eq!(raw_marker(&cg).await["tripleCount"], ["4"]);
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_target_outage_never_blocks_acceptance_and_the_projector_catches_up() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let down = w.projector(dead_target(), "down", None).await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    assert_failed(
        down.step().await.unwrap(),
        ProjectionErrorCode::TargetUnavailable,
        ErrorClass::Retryable,
    );
    // The ledger keeps accepting while the target is down; the backlog is observable.
    let c2 = w.accept(&g, Some(&c1), &[Q2]).await;
    let c3 = w.accept(&g, Some(&c2), &[Q3]).await;
    let status = w.status(&g).await;
    assert_eq!(status.head_version, Some(3));
    assert_eq!((status.lag_versions(), status.pending_events), (3, 3));
    assert_eq!(
        status.last_error_code.as_deref(),
        Some("TARGET_UNAVAILABLE")
    );
    assert_eq!(
        (status.consecutive_failures, status.status.as_str()),
        (1, "active")
    );
    // Recovery: one projection straight to the accepted head (state-based, ADR-0020).
    w.due_now(&g).await;
    let up = w.healthy().await;
    assert_eq!(up.step().await.unwrap(), projected(3, false));
    assert_projected(&cg, &g, &quads(&[Q1, Q2, Q3]), &c3, 3).await;
    let status = w.status(&g).await;
    assert_eq!(
        (
            status.lag_versions(),
            status.pending_events,
            status.consecutive_failures
        ),
        (0, 0, 0)
    );
    assert_eq!(
        status.last_error_code.as_deref(),
        Some("TARGET_UNAVAILABLE"),
        "last error kept for audit"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_write_committed_behind_a_lost_response_is_acknowledged_not_repeated() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let scripted = Scripted::new();
    scripted.lose_response.store(true, Ordering::SeqCst);
    let p = w.projector(scripted.clone(), "lossy", None).await;
    let c1 = w.accept(&g, None, &[Q1, Q2]).await;
    assert_failed(
        p.step().await.unwrap(),
        ProjectionErrorCode::TargetTimeout,
        ErrorClass::Retryable,
    );
    assert_eq!(
        w.status(&g).await.projected_ref_version,
        None,
        "ambiguous: nothing recorded"
    );
    w.due_now(&g).await;
    // The marker proves the write landed: acknowledged without writing again.
    assert_eq!(p.step().await.unwrap(), acknowledged(1));
    assert_projected(&cg, &g, &quads(&[Q1, Q2]), &c1, 1).await;
    let status = w.status(&g).await;
    assert_eq!(
        (
            status.projected_ref_version,
            status.pending_events,
            status.rebuilds
        ),
        (Some(1), 0, 0)
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn every_crash_window_recovers_at_genesis_and_over_a_predecessor() {
    let _serial = serial().await;
    let w = World::new().await;
    for point in [
        FailPoint::AfterClaim,
        FailPoint::BeforeTargetRequest,
        FailPoint::AfterTargetSuccess,
        FailPoint::BeforeMarkerVerification,
        FailPoint::AfterMarkerVerification,
        FailPoint::BeforeAcknowledge,
    ] {
        let wrote_before_crash = matches!(
            point,
            FailPoint::AfterTargetSuccess
                | FailPoint::BeforeMarkerVerification
                | FailPoint::AfterMarkerVerification
                | FailPoint::BeforeAcknowledge
        );
        let (g, cg) = w.graph().await;
        let crashing = w.projector(target(), "crashing", Some(point)).await;
        let other = w.healthy().await;
        let mut head: Option<CommitId> = None;
        let mut expected: Vec<&str> = Vec::new();
        for (version, quad) in [(1, Q1), (2, Q2)] {
            let commit = w.accept(&g, head.as_ref(), &[quad]).await;
            expected.push(quad);
            let ctx = format!("{point:?} at v{version}");
            assert_eq!(
                crashing.step().await.unwrap(),
                StepOutcome::Crashed(point),
                "{ctx}"
            );
            let status = w.status(&g).await;
            assert_eq!(
                status.projected_ref_version,
                (version > 1).then_some(version - 1),
                "{ctx}: nothing new acknowledged"
            );
            assert!(status.leased, "{ctx}: the crashed worker's lease is held");
            // While the lease is live, nobody else takes the stream.
            assert_eq!(other.step().await.unwrap(), StepOutcome::Idle, "{ctx}");
            w.expire_lease(&g).await;
            let recovered = if wrote_before_crash {
                // The marker proves the target already holds the version: no rewrite.
                acknowledged(version)
            } else {
                projected(version, false)
            };
            assert_eq!(other.step().await.unwrap(), recovered, "{ctx}");
            assert_projected(&cg, &g, &quads(&expected), &commit, version).await;
            let status = w.status(&g).await;
            assert_eq!(
                (
                    status.projected_ref_version,
                    status.pending_events,
                    status.leased,
                    status.rebuilds
                ),
                (Some(version), 0, false, 0),
                "{ctx}"
            );
            head = Some(commit);
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn lost_or_corrupt_markers_are_rebuilt_and_out_of_band_edits_are_reconciled() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let p = w.healthy().await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    p.step().await.unwrap();
    // Marker lost while the graph stays populated.
    raw_update(&format!(
        "DELETE WHERE {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> ?p ?o }} }}"
    ))
    .await;
    let c2 = w.accept(&g, Some(&c1), &[Q2]).await;
    assert_eq!(p.step().await.unwrap(), projected(2, true));
    assert_projected(&cg, &g, &quads(&[Q1, Q2]), &c2, 2).await;
    // Marker corrupted with a second refVersion value outside i64 (a double): malformed, not
    // provably ahead; the replacement names the observed terms exactly, so recovery applies.
    raw_update(&format!(
        "INSERT DATA {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> <{LP_NAMESPACE}refVersion> \
         \"1e3\"^^<http://www.w3.org/2001/XMLSchema#double> }} }}"
    ))
    .await;
    let c3 = w.accept(&g, Some(&c2), &[Q3]).await;
    assert_eq!(p.step().await.unwrap(), projected(3, true));
    assert_projected(&cg, &g, &quads(&[Q1, Q2, Q3]), &c3, 3).await;
    // Out-of-band edit with nothing pending: verify detects it, reconciliation repairs it.
    raw_update(&format!(
        "INSERT DATA {{ GRAPH <{cg}> {{ <urn:x> <urn:y> \"stray\" }} }}"
    ))
    .await;
    let report = p.verify(&w.key(&g)).await.unwrap();
    assert!(!report.consistent, "{}", report.detail);
    assert_eq!(
        p.step().await.unwrap(),
        StepOutcome::Idle,
        "nothing pending"
    );
    w.reconcile_due(&g).await;
    assert_eq!(p.reconcile_step().await.unwrap(), projected(3, true));
    assert!(p.verify(&w.key(&g)).await.unwrap().consistent);
    assert_projected(&cg, &g, &quads(&[Q1, Q2, Q3]), &c3, 3).await;
    assert_eq!(w.status(&g).await.rebuilds, 3);
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn reconciliation_repairs_a_target_that_lost_its_data_without_a_new_event() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let p = w.healthy().await;
    let c1 = w.accept(&g, None, &[Q1, Q3]).await;
    p.step().await.unwrap();
    // A healthy idle stream: reconciliation re-observes and acknowledges without writing.
    w.reconcile_due(&g).await;
    assert_eq!(p.reconcile_step().await.unwrap(), acknowledged(1));
    assert_eq!(
        p.reconcile_step().await.unwrap(),
        StepOutcome::Idle,
        "checked recently"
    );
    // The target loses everything (e.g. restarted onto an empty store); nothing is pending.
    raw_update(&format!(
        "DROP SILENT GRAPH <{cg}> ; DELETE WHERE {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> ?p ?o }} }}"
    ))
    .await;
    assert_eq!(
        p.step().await.unwrap(),
        StepOutcome::Idle,
        "no pending event"
    );
    w.reconcile_due(&g).await;
    assert_eq!(p.reconcile_step().await.unwrap(), projected(1, true));
    assert_projected(&cg, &g, &quads(&[Q1, Q3]), &c1, 1).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_stale_replacement_never_overwrites_a_newer_projection() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let healthy = w.healthy().await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    healthy.step().await.unwrap();
    // Make the next evaluation a rebuild (malformed marker: an unknown protocol predicate).
    raw_update(&format!(
        "INSERT DATA {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> <{LP_NAMESPACE}unknown> \"x\" }} }}"
    ))
    .await;
    let c2 = w.accept(&g, Some(&c1), &[Q2]).await;
    // Worker A plans a v2 replacement and stalls inside its write.
    let scripted = Scripted::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    *scripted.hold.lock().unwrap() = Some(gate.clone());
    let stale = Arc::new(w.projector(scripted.clone(), "stale", None).await);
    let stale_step = tokio::spawn({
        let stale = stale.clone();
        async move { stale.step().await.unwrap() }
    });
    let mut reached = false;
    for _ in 0..500 {
        if scripted.hold.lock().unwrap().is_none() {
            reached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(reached, "worker A reached its target write");
    // A's lease expires; v3 is accepted; worker B rebuilds to v3 and acknowledges.
    w.expire_lease(&g).await;
    let c3 = w.accept(&g, Some(&c2), &[Q3]).await;
    assert_eq!(healthy.step().await.unwrap(), projected(3, true));
    assert_projected(&cg, &g, &quads(&[Q1, Q2, Q3]), &c3, 3).await;
    // A's v2 replacement lands late: guarded by its ceiling, it is a no-op.
    gate.notify_one();
    assert_eq!(stale_step.await.unwrap(), StepOutcome::Superseded);
    assert_projected(&cg, &g, &quads(&[Q1, Q2, Q3]), &c3, 3).await;
    let status = w.status(&g).await;
    assert_eq!(
        (status.projected_ref_version, status.status.as_str()),
        (Some(3), "active")
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn markers_ahead_or_of_another_stream_are_never_overwritten_automatically() {
    let _serial = serial().await;
    let w = World::new().await;
    let kb = format!("urn:it:kb:{}", unique("shared"));
    let (g, cg) = w.graph_with_kb(&kb).await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    let s1 = quads(&[Q1]);
    // The target claims version 99 (e.g. the ledger was restored from an older backup).
    let future = CommitId(ContentId::for_bytes(b"from the future"));
    let empty = target().observe(&cg).await.unwrap();
    target()
        .write(
            &cg,
            &ProjectedState::from_state(&s1).unwrap(),
            &marker(&g, &future, 99, &s1),
            WriteMode::Replace,
            &empty.terms,
        )
        .await
        .unwrap();
    assert_eq!(raw_marker(&cg).await["refVersion"], ["99"]);
    let p = w.healthy().await;
    assert_failed(
        p.step().await.unwrap(),
        ProjectionErrorCode::MarkerAhead,
        ErrorClass::Permanent,
    );
    assert_eq!(w.status(&g).await.status, "rebuild_required");
    assert_eq!(
        raw_marker(&cg).await["refVersion"],
        ["99"],
        "left untouched"
    );
    w.due_now(&g).await;
    assert_eq!(
        p.step().await.unwrap(),
        StepOutcome::Idle,
        "not claimed while recovery is pending"
    );
    // An operator rebuild decides: the ledger is authoritative. Rebuild is idempotent.
    for _ in 0..2 {
        assert_eq!(
            p.rebuild(&w.key(&g)).await.unwrap(),
            Some(projected(1, true))
        );
        assert_projected(&cg, &g, &s1, &c1, 1).await;
        assert_eq!(w.status(&g).await.status, "active");
    }
    // Another deployment (another target id on the same dataset) projects a different
    // ledger graph of the same KB into the same cognitive graph: refused, nothing written.
    let other = World::new().await;
    let (g2, cg2) = other.graph_with_kb(&kb).await;
    assert_eq!(cg2, cg);
    other.accept(&g2, None, &[Q2]).await;
    assert_failed(
        other.healthy().await.step().await.unwrap(),
        ProjectionErrorCode::TargetConflict,
        ErrorClass::Permanent,
    );
    assert_eq!(other.status(&g2).await.status, "rebuild_required");
    assert_projected(&cg, &g, &s1, &c1, 1).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn the_targets_literal_canonicalization_never_loops_or_blocks() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let p = w.healthy().await;
    // Two lexical forms of one integer value and a non-canonical decimal: three distinct RDF
    // terms in the ledger, which TDB2 stores canonically as two triples.
    let c1 = w
        .accept(
            &g,
            None,
            &[
                "<urn:n> <urn:v> \"01\"^^<http://www.w3.org/2001/XMLSchema#integer> .",
                "<urn:n> <urn:v> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer> .",
                "<urn:n> <urn:d> \"1.50\"^^<http://www.w3.org/2001/XMLSchema#decimal> .",
            ],
        )
        .await;
    assert_eq!(p.step().await.unwrap(), projected(1, false));
    assert_eq!(
        raw_graph(&cg).await.len(),
        2,
        "the target merged equal values"
    );
    let marker = raw_marker(&cg).await;
    assert_eq!(marker["tripleCount"], ["2"], "the target's own count");
    assert_eq!(marker["commitId"], [c1.to_string()]);
    assert!(p.verify(&w.key(&g)).await.unwrap().consistent);
    w.reconcile_due(&g).await;
    assert_eq!(
        p.reconcile_step().await.unwrap(),
        acknowledged(1),
        "no rebuild loop"
    );
    assert_eq!(
        p.rebuild(&w.key(&g)).await.unwrap(),
        Some(projected(1, true)),
        "a rebuild verifies by the target's term equality"
    );
    assert_eq!(w.status(&g).await.status, "active");
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn concurrent_workers_and_many_graphs_never_mix_or_repeat() {
    let _serial = serial().await;
    let w = World::new().await;
    let (ma, mb) = (Arc::new(Metrics::default()), Arc::new(Metrics::default()));
    let a = w
        .projector_with(target(), "worker-a", None, ma.clone())
        .await;
    let b = w
        .projector_with(target(), "worker-b", None, mb.clone())
        .await;
    // Two workers racing for one stream: one projects, the other finds nothing claimable.
    let (g, cg) = w.graph().await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    let (ra, rb) = tokio::join!(a.step(), b.step());
    let mut outcomes = [ra.unwrap(), rb.unwrap()];
    outcomes.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(outcomes, [StepOutcome::Idle, projected(1, false)]);
    assert_projected(&cg, &g, &quads(&[Q1]), &c1, 1).await;
    // Many graphs, two projector loops running concurrently.
    let mut graphs = Vec::new();
    for i in 0..6 {
        // Graphs of two tenants, interleaved.
        let tenant = if i % 2 == 0 {
            "tenant-it"
        } else {
            "tenant-it2"
        };
        let (gi, cgi) = w
            .graph_of(tenant, &format!("urn:it:kb:{}", unique("kb")))
            .await;
        let mut head = None;
        let mut expected = Vec::new();
        for j in 0..=i % 3 {
            expected.push(format!("<urn:g{i}:s{j}> <urn:p> \"{i}-{j}\" ."));
            let quad = expected.last().unwrap().clone();
            head = Some(w.accept(&gi, head.as_ref(), &[quad.as_str()]).await);
        }
        graphs.push((gi, cgi, head.unwrap(), i64::from(i % 3 + 1), expected));
    }
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (a, b) = (Arc::new(a), Arc::new(b));
    let ta = tokio::spawn(a.clone().run(stop_rx.clone()));
    let tb = tokio::spawn(b.clone().run(stop_rx));
    let mut converged = false;
    for _ in 0..300 {
        let mut done = true;
        for (gi, ..) in &graphs {
            done &= w.status(gi).await.lag_versions() == 0;
        }
        if done {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = stop_tx.send(true);
    let _ = tokio::join!(ta, tb);
    assert!(converged, "the two projectors converged within 15 s");
    for (gi, cgi, head, version, expected) in &graphs {
        let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
        assert_projected(cgi, gi, &quads(&expected), head, *version).await;
        let status = w.status(gi).await;
        assert_eq!(
            (status.rebuilds, status.consecutive_failures, status.leased),
            (0, 0, false),
            "{gi}"
        );
    }
    // Exclusive leases: no attempt of either worker lost its lease or found its write
    // superseded, and nothing was rebuilt (a correct retry after a transient error is fine).
    for m in [&ma, &mb] {
        let text = m.render(&[], 0);
        for counter in [
            "projection_lease_lost_total",
            "projection_superseded_total",
            "projection_rebuilds_total",
        ] {
            let line = text
                .lines()
                .find(|l| l.starts_with(&format!("{counter} ")))
                .unwrap_or_else(|| panic!("{counter} missing"));
            assert_eq!(line, format!("{counter} 0"), "{text}");
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn named_graph_state_blocks_the_stream_visibly() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    w.accept(&g, None, &["<urn:s> <urn:p> <urn:o> <urn:named> ."])
        .await;
    let p = w.healthy().await;
    assert_failed(
        p.step().await.unwrap(),
        ProjectionErrorCode::NamedGraphUnsupported,
        ErrorClass::Permanent,
    );
    let status = w.status(&g).await;
    assert_eq!(status.status, "blocked");
    assert_eq!(
        status.last_error_code.as_deref(),
        Some("NAMED_GRAPH_UNSUPPORTED")
    );
    assert!(raw_graph(&cg).await.is_empty(), "nothing written");
    assert!(raw_marker(&cg).await.is_empty(), "no marker");
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_write_in_flight_across_a_feed_switch_never_lands_in_the_new_feeds_graph() {
    let _serial = serial().await;
    let w = World::new().await;
    let kb = format!("urn:it:kb:{}", unique("switch"));
    // Feed 1: tenant A's graph projects v1 into the KB's cognitive graph.
    let (g1, cg) = w.graph_of("tenant-it", &kb).await;
    let healthy = w.healthy().await;
    let c1 = w.accept(&g1, None, &[Q1]).await;
    assert_eq!(healthy.step().await.unwrap(), projected(1, false));
    // A worker observes g1's marker, plans v2 and stalls inside its write.
    w.accept(&g1, Some(&c1), &[Q2]).await;
    let scripted = Scripted::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    *scripted.hold.lock().unwrap() = Some(gate.clone());
    let stale = Arc::new(w.projector(scripted.clone(), "stale", None).await);
    let stale_step = tokio::spawn({
        let stale = stale.clone();
        async move { stale.step().await.unwrap() }
    });
    let mut reached = false;
    for _ in 0..500 {
        if scripted.hold.lock().unwrap().is_none() {
            reached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(reached, "the stale worker reached its target write");
    // The operator switches the KB's feed to another tenant's graph: disable (the graph
    // stays taken until a projector fenced it), enable, and (after the conflict is
    // reported) rebuild.
    assert!(w.owner().disable(&w.key(&g1)).await.unwrap());
    assert_eq!(w.status(&g1).await.status, "disabling");
    assert_eq!(
        healthy.step().await.unwrap(),
        StepOutcome::Idle,
        "the stalled worker holds g1"
    );
    w.expire_lease(&g1).await;
    assert_eq!(healthy.step().await.unwrap(), StepOutcome::Fenced);
    assert_eq!(w.status(&g1).await.status, "disabled");
    let (g2, cg2) = w.graph_of("tenant-it2", &kb).await;
    assert_eq!(cg2, cg);
    let d1 = w.accept(&g2, None, &[Q3]).await;
    assert_failed(
        healthy.step().await.unwrap(),
        ProjectionErrorCode::TargetConflict,
        ErrorClass::Permanent,
    );
    assert_eq!(
        healthy.rebuild(&w.key(&g2)).await.unwrap(),
        Some(projected(1, true))
    );
    assert_projected(&cg, &g2, &quads(&[Q3]), &d1, 1).await;
    // The stale write lands now: its precondition (g1's v1 marker) no longer holds.
    gate.notify_one();
    let outcome = stale_step.await.unwrap();
    assert!(
        matches!(outcome, StepOutcome::LeaseLost),
        "the disabled stream's attempt is fenced: {outcome:?}"
    );
    assert_projected(&cg, &g2, &quads(&[Q3]), &d1, 1).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_marker_naming_another_commit_at_its_version_is_rebuilt_from_the_ledger() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let p = w.healthy().await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    p.step().await.unwrap();
    // A well-formed marker of this stream at v1, but naming a commit the ledger does not
    // have at v1 (another ledger's history).
    let foreign = CommitId(ContentId::for_bytes(b"another history"));
    raw_update(&format!(
        "DELETE WHERE {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> <{LP_NAMESPACE}commitId> ?c }} }} ; \
         INSERT DATA {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> <{LP_NAMESPACE}commitId> \"{foreign}\" }} }}"
    ))
    .await;
    let c2 = w.accept(&g, Some(&c1), &[Q2]).await;
    assert_eq!(p.step().await.unwrap(), projected(2, true));
    assert_projected(&cg, &g, &quads(&[Q1, Q2]), &c2, 2).await;
    assert_eq!(w.status(&g).await.rebuilds, 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_malformed_marker_ahead_of_the_ledger_waits_for_an_operator() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let p = w.healthy().await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    p.step().await.unwrap();
    // A second refVersion value beyond the head (e.g. a newer protocol's marker).
    raw_update(&format!(
        "INSERT DATA {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> <{LP_NAMESPACE}refVersion> 99 }} }}"
    ))
    .await;
    let c2 = w.accept(&g, Some(&c1), &[Q2]).await;
    assert_failed(
        p.step().await.unwrap(),
        ProjectionErrorCode::MarkerAhead,
        ErrorClass::Permanent,
    );
    assert_eq!(w.status(&g).await.status, "rebuild_required");
    assert_eq!(
        raw_marker(&cg).await["refVersion"].len(),
        2,
        "left untouched"
    );
    assert_eq!(
        p.rebuild(&w.key(&g)).await.unwrap(),
        Some(projected(2, true))
    );
    assert_projected(&cg, &g, &quads(&[Q1, Q2]), &c2, 2).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn verification_failures_are_never_acknowledged() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let scripted = Scripted::new();
    let p = w.projector(scripted.clone(), "verifying", None).await;
    let c1 = w.accept(&g, None, &[Q1, Q2]).await;
    // The target answers "done" but never applied the write: the read-back refuses it.
    scripted.swallow.store(true, Ordering::SeqCst);
    assert_failed(
        p.step().await.unwrap(),
        ProjectionErrorCode::VerificationFailed,
        ErrorClass::Retryable,
    );
    let status = w.status(&g).await;
    assert_eq!(
        (status.projected_ref_version, status.pending_events),
        (None, 1)
    );
    assert!(raw_marker(&cg).await.is_empty());
    scripted.swallow.store(false, Ordering::SeqCst);
    w.due_now(&g).await;
    assert_eq!(p.step().await.unwrap(), projected(1, false));
    // A rebuild whose containment check fails is permanent: the stream blocks.
    scripted.deny_containment.store(true, Ordering::SeqCst);
    assert_eq!(
        p.rebuild(&w.key(&g)).await.unwrap(),
        Some(StepOutcome::Failed {
            code: ProjectionErrorCode::VerificationFailed,
            class: ErrorClass::Permanent
        })
    );
    let status = w.status(&g).await;
    assert_eq!(
        (status.status.as_str(), status.last_error_code.as_deref()),
        ("blocked", Some("VERIFICATION_FAILED"))
    );
    assert_eq!(status.projected_ref_version, Some(1));
    assert_projected(&cg, &g, &quads(&[Q1, Q2]), &c1, 1).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_failing_transactional_probe_pauses_claiming() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, _cg) = w.graph().await;
    let scripted = Scripted::new();
    let p = w.projector(scripted.clone(), "probing", None).await;
    w.accept(&g, None, &[Q1]).await;
    scripted.fail_probe.store(true, Ordering::SeqCst);
    assert_eq!(
        p.probe().await.unwrap_err().code(),
        ProjectionErrorCode::TargetNotTransactional
    );
    assert!(p.is_paused());
    assert_eq!(
        p.step().await.unwrap(),
        StepOutcome::Idle,
        "paused with pending work"
    );
    assert_eq!(w.status(&g).await.pending_events, 1);
    scripted.fail_probe.store(false, Ordering::SeqCst);
    p.probe().await.unwrap();
    assert!(!p.is_paused());
    assert_eq!(p.step().await.unwrap(), projected(1, false));
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_refused_target_credential_blocks_the_stream() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let p = w
        .projector(
            fuseki_as(&env("LEDGER_TEST_FUSEKI_URL"), "not-the-password"),
            "wrong-password",
            None,
        )
        .await;
    w.accept(&g, None, &[Q1]).await;
    // The target refuses the credential (401): permanent, never retried in a loop.
    assert_failed(
        p.step().await.unwrap(),
        ProjectionErrorCode::TargetAuth,
        ErrorClass::Permanent,
    );
    let status = w.status(&g).await;
    assert_eq!(
        (status.status.as_str(), status.last_error_code.as_deref()),
        ("blocked", Some("TARGET_AUTH"))
    );
    assert!(raw_graph(&cg).await.is_empty());
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_stalled_rebuild_of_a_disabled_feed_never_lands_after_the_old_feed_returns() {
    let _serial = serial().await;
    let w = World::new().await;
    let kb = format!("urn:it:kb:{}", unique("aba"));
    let healthy = w.healthy().await;
    // Feed A projects v1, then is disabled (fenced) in favour of feed B.
    let (ga, cg) = w.graph_of("tenant-it", &kb).await;
    let ca = w.accept(&ga, None, &[Q1]).await;
    assert_eq!(healthy.step().await.unwrap(), projected(1, false));
    assert!(w.owner().disable(&w.key(&ga)).await.unwrap());
    assert_eq!(healthy.step().await.unwrap(), StepOutcome::Fenced);
    let (gb, _) = w.graph_of("tenant-it2", &kb).await;
    w.accept(&gb, None, &[Q3]).await;
    assert_failed(
        healthy.step().await.unwrap(),
        ProjectionErrorCode::TargetConflict,
        ErrorClass::Permanent,
    );
    // An operator rebuild of B observes A's marker and stalls inside its write.
    let scripted = Scripted::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    *scripted.hold.lock().unwrap() = Some(gate.clone());
    let stale = Arc::new(w.projector(scripted.clone(), "stale-rebuild", None).await);
    let stale_rebuild = tokio::spawn({
        let (stale, key) = (stale.clone(), w.key(&gb));
        async move { stale.rebuild(&key).await.unwrap() }
    });
    let mut reached = false;
    for _ in 0..500 {
        if scripted.hold.lock().unwrap().is_none() {
            reached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(reached, "the rebuild reached its target write");
    // The operator changes their mind: B is disabled (fenced once its lease is gone) and A
    // re-enabled. A's marker content is back exactly as B's rebuild observed it — except the
    // write id the fence rotated.
    assert!(w.owner().disable(&w.key(&gb)).await.unwrap());
    w.expire_lease(&gb).await;
    assert_eq!(healthy.step().await.unwrap(), StepOutcome::Fenced);
    w.owner()
        .enable(&w.key(&ga), |kb| {
            CognitiveGraph::for_knowledge_base(kb)
                .map(|g| g.as_iri().to_owned())
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap();
    assert_eq!(w.status(&ga).await.status, "active");
    // B's stalled replacement lands now and must be a no-op.
    gate.notify_one();
    let outcome = stale_rebuild.await.unwrap();
    assert!(
        matches!(outcome, Some(StepOutcome::LeaseLost)),
        "the disabled stream's rebuild is fenced: {outcome:?}"
    );
    assert_eq!(raw_graph(&cg).await, quads(&[Q1]), "A's content, not B's");
    let marker = raw_marker(&cg).await;
    assert_eq!(marker["graphId"], [ga.as_str()]);
    assert_eq!(marker["commitId"], [ca.to_string()]);
    // A carries on normally (the fence left A's marker well-formed).
    let ca2 = w.accept(&ga, Some(&ca), &[Q2]).await;
    assert_eq!(healthy.step().await.unwrap(), projected(2, false));
    assert_projected(&cg, &ga, &quads(&[Q1, Q2]), &ca2, 2).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_delayed_operator_rebuild_never_regresses_a_newer_projection() {
    let _serial = serial().await;
    let w = World::new().await;
    let (g, cg) = w.graph().await;
    let healthy = w.healthy().await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    assert_eq!(healthy.step().await.unwrap(), projected(1, false));
    // An operator rebuild selects the head (v1) and stalls before observing the target.
    let scripted = Scripted::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    *scripted.hold_observe.lock().unwrap() = Some(gate.clone());
    let stale = Arc::new(w.projector(scripted.clone(), "slow-rebuild", None).await);
    let stale_rebuild = tokio::spawn({
        let (stale, key) = (stale.clone(), w.key(&g));
        async move { stale.rebuild(&key).await.unwrap() }
    });
    let mut reached = false;
    for _ in 0..500 {
        if scripted.hold_observe.lock().unwrap().is_none() {
            reached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(reached, "the rebuild selected its work");
    // Its lease runs out; v2 is accepted and projected by another worker.
    w.expire_lease(&g).await;
    let c2 = w.accept(&g, Some(&c1), &[Q2]).await;
    assert_eq!(healthy.step().await.unwrap(), projected(2, false));
    // The rebuild now observes v2 — genuine ledger history newer than its work — and stops.
    gate.notify_one();
    assert_eq!(stale_rebuild.await.unwrap(), Some(StepOutcome::Superseded));
    assert_projected(&cg, &g, &quads(&[Q1, Q2]), &c2, 2).await;
    assert_eq!(w.status(&g).await.projected_ref_version, Some(2));
}
