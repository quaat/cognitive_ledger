//! Projection against a **real Fuseki** (pinned image, `deploy/fuseki/ledger-projection.ttl`)
//! and real PostgreSQL (Plan 0007 scenarios): genesis, advance, duplicate and stale writes,
//! target outage and catch-up, every crash window, lost / corrupt / ahead markers and
//! rebuild, concurrent workers, multiple graphs, unsupported state, the transactional probe.
//!
//! Requires `LEDGER_TEST_DATABASE_URL` (owner), `LEDGER_TEST_FUSEKI_URL` (dataset base URL,
//! e.g. `http://127.0.0.1:53030/ledger`) and `LEDGER_TEST_FUSEKI_PASSWORD` (the `admin`
//! password). Every test is `#[ignore]`.

use ledger_core::{
    AuthenticatedPrincipal, CommitId, ContentId, GraphId, PrincipalId, PrincipalType, TenantId,
};
use ledger_projection::{
    CognitiveGraph, ErrorClass, MARKER_GRAPH, MarkerRead, ProjectedState, ProjectionClient,
    ProjectionErrorCode, ProjectionMarker, WriteMode,
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
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
    Arc::new(
        FusekiClient::new(FusekiConfig {
            query_endpoint: format!("{base}/query"),
            update_endpoint: format!("{base}/update"),
            credentials: TargetCredentials::Basic {
                username: "admin".into(),
                password: env("LEDGER_TEST_FUSEKI_PASSWORD"),
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

struct World {
    store: PostgresLedgerStore,
    target_id: String,
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
        }
    }

    async fn projector(
        &self,
        client: Arc<dyn ProjectionClient>,
        owner: &str,
        ttl: Duration,
        failpoint: Option<FailPoint>,
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
                lease_ttl: ttl,
                reconstruction: ReconstructionLimits::DEVELOPMENT,
                backoff_base: Duration::from_millis(10),
                backoff_max: Duration::from_millis(50),
                poll_interval: Duration::from_millis(20),
                concurrency: 2,
            },
            Arc::new(Metrics::default()),
        );
        match failpoint {
            Some(point) => projector.with_failpoint(point),
            None => projector,
        }
    }

    async fn healthy(&self) -> Projector<dyn ProjectionClient> {
        self.projector(target(), &unique("w"), Duration::from_secs(30), None)
            .await
    }

    /// A graph with its own KB (hence its own cognitive graph), projection enabled.
    async fn graph(&self, tenant: &str) -> (GraphId, CognitiveGraph) {
        let id = GraphId::new(unique("pg")).unwrap();
        let kb = format!("urn:it:kb:{}", unique("kb"));
        self.store
            .graphs()
            .create(&NewGraph {
                graph_id: id.clone(),
                tenant_id: TenantId::new(tenant).unwrap(),
                knowledge_base_id: Some(kb.clone()),
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
        (id, CognitiveGraph::for_knowledge_base(&kb).unwrap())
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
        let scope = |key: String| RequestScope {
            principal: AuthenticatedPrincipal {
                principal_id: PrincipalId::new("urn:it:agent").unwrap(),
                principal_type: PrincipalType::Agent,
                tenant_id: self.tenant_of(graph),
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

    fn tenant_of(&self, _graph: &GraphId) -> TenantId {
        TenantId::new("tenant-it").unwrap()
    }

    /// The ledger's authoritative accepted state at `commit` (owner-side reconstruction).
    async fn state(&self, graph: &GraphId, commit: &CommitId) -> BTreeSet<Quad> {
        ProjectionRepository::new(self.store.pool().clone())
            .state_at(graph, commit, &ReconstructionLimits::DEVELOPMENT)
            .await
            .unwrap()
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

    /// Expire the stream's lease as if its holder crashed long ago (owner-side, test only;
    /// deterministic instead of sleeping past the TTL).
    async fn expire_lease(&self, graph: &GraphId) {
        sqlx::query(
            "UPDATE projection_state SET lease_until = now() - interval '1 second' \
             WHERE graph_id = $1 AND lease_until IS NOT NULL",
        )
        .bind(graph.as_str())
        .execute(self.store.pool())
        .await
        .unwrap();
    }

    /// Skip backoff so a test can retry immediately (owner-side, test only).
    async fn due_now(&self, graph: &GraphId) {
        sqlx::query("UPDATE projection_state SET next_attempt_at = now() WHERE graph_id = $1")
            .bind(graph.as_str())
            .execute(self.store.pool())
            .await
            .unwrap();
    }
}

/// The target holds exactly `state` and a marker for `(commit, version)`.
async fn assert_projected(
    graph: &CognitiveGraph,
    ledger_graph: &GraphId,
    state: &BTreeSet<Quad>,
    commit: &CommitId,
    version: i64,
) {
    let client = target();
    assert_eq!(
        &client.read_graph(graph).await.unwrap(),
        state,
        "graph content"
    );
    let observed = client.observe(graph).await.unwrap();
    match observed.marker {
        MarkerRead::Present(m) => {
            assert_eq!(m.graph_id, *ledger_graph);
            assert_eq!(&m.commit, commit);
            assert_eq!(m.ref_version, version);
            assert_eq!(m.triple_count, state.len() as u64);
            assert_eq!(m.state_digest, ledger_rdf::state_digest(state));
        }
        other => panic!("expected a marker, got {other:?}"),
    }
    assert_eq!(observed.triple_count, state.len() as u64);
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

const Q1: &str = "<urn:m:a> <urn:label> \"A\" .";
const Q2: &str = "<urn:m:b> <urn:label> \"B\"@en .";
const Q3: &str = "<urn:m:c> <urn:weight> \"3\"^^<http://www.w3.org/2001/XMLSchema#integer> .";

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn the_target_is_transactional() {
    target()
        .probe_transactional()
        .await
        .expect("TDB2 rolls a failed update back");
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn genesis_advance_and_duplicate_or_stale_writes() {
    let w = World::new().await;
    let (g, cg) = w.graph("tenant-it").await;
    let p = w.healthy().await;
    // Genesis.
    let c1 = w.accept(&g, None, &[Q1]).await;
    assert_eq!(
        p.step().await.unwrap(),
        StepOutcome::Projected {
            version: 1,
            rebuilt: false
        }
    );
    let s1 = w.state(&g, &c1).await;
    // Independent oracle: exactly the accepted quads.
    assert_eq!(
        s1,
        [Q1].iter()
            .map(|q| q.parse().unwrap())
            .collect::<BTreeSet<Quad>>()
    );
    assert_projected(&cg, &g, &s1, &c1, 1).await;
    assert_eq!(p.step().await.unwrap(), StepOutcome::Idle, "nothing left");
    // Advance.
    let c2 = w.accept(&g, Some(&c1), &[Q2, Q3]).await;
    assert_eq!(
        p.step().await.unwrap(),
        StepOutcome::Projected {
            version: 2,
            rebuilt: false
        }
    );
    let s2 = w.state(&g, &c2).await;
    assert_eq!(
        s2,
        [Q1, Q2, Q3]
            .iter()
            .map(|q| q.parse().unwrap())
            .collect::<BTreeSet<Quad>>()
    );
    assert_projected(&cg, &g, &s2, &c2, 2).await;
    let status = w.status(&g).await;
    assert_eq!(
        (
            status.projected_ref_version,
            status.lag_versions(),
            status.pending_events
        ),
        (Some(2), 0, 0)
    );
    // A duplicate v2 write and a stale v1 write are no-ops on the target.
    let client = target();
    let marker = |commit: &CommitId, version: i64, state: &BTreeSet<Quad>| ProjectionMarker {
        graph_id: g.clone(),
        branch: "main".into(),
        commit: commit.clone(),
        ref_version: version,
        state_digest: ledger_rdf::state_digest(state),
        triple_count: state.len() as u64,
    };
    client
        .write(
            &cg,
            &ProjectedState::from_state(&s2).unwrap(),
            &marker(&c2, 2, &s2),
            WriteMode::Conditional,
        )
        .await
        .unwrap();
    let s1 = w.state(&g, &c1).await;
    client
        .write(
            &cg,
            &ProjectedState::from_state(&s1).unwrap(),
            &marker(&c1, 1, &s1),
            WriteMode::Conditional,
        )
        .await
        .unwrap();
    assert_projected(&cg, &g, &s2, &c2, 2).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_target_outage_never_blocks_acceptance_and_the_projector_catches_up() {
    let w = World::new().await;
    let (g, cg) = w.graph("tenant-it").await;
    let down = w
        .projector(dead_target(), "down", Duration::from_secs(30), None)
        .await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    match down.step().await.unwrap() {
        StepOutcome::Failed { code, class } => {
            assert_eq!(
                (code, class),
                (
                    ProjectionErrorCode::TargetUnavailable,
                    ErrorClass::Retryable
                )
            );
        }
        other => panic!("{other:?}"),
    }
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
    assert_eq!(status.consecutive_failures, 1);
    // Recovery: one projection straight to the accepted head (state-based, ADR-0020).
    w.due_now(&g).await;
    let up = w.healthy().await;
    assert_eq!(
        up.step().await.unwrap(),
        StepOutcome::Projected {
            version: 3,
            rebuilt: false
        }
    );
    assert_projected(&cg, &g, &w.state(&g, &c3).await, &c3, 3).await;
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
async fn every_crash_window_recovers_to_exactly_the_accepted_state() {
    let w = World::new().await;
    for point in [
        FailPoint::AfterClaim,
        FailPoint::BeforeTargetRequest,
        FailPoint::AfterTargetSuccess,
        FailPoint::BeforeMarkerVerification,
        FailPoint::AfterMarkerVerification,
        FailPoint::BeforeAcknowledge,
    ] {
        let (g, cg) = w.graph("tenant-it").await;
        let c1 = w.accept(&g, None, &[Q1, Q2]).await;
        let crashing = w
            .projector(target(), "crashing", Duration::from_secs(300), Some(point))
            .await;
        assert_eq!(
            crashing.step().await.unwrap(),
            StepOutcome::Crashed(point),
            "{point:?}"
        );
        let status = w.status(&g).await;
        assert_eq!(
            status.projected_ref_version, None,
            "{point:?}: nothing acknowledged"
        );
        assert!(
            status.leased,
            "{point:?}: the crashed worker's lease is still held"
        );
        // While the lease is live, nobody else takes the stream.
        let other = w.healthy().await;
        assert_eq!(other.step().await.unwrap(), StepOutcome::Idle, "{point:?}");
        w.expire_lease(&g).await;
        let wrote_before_crash = matches!(
            point,
            FailPoint::AfterTargetSuccess
                | FailPoint::BeforeMarkerVerification
                | FailPoint::AfterMarkerVerification
                | FailPoint::BeforeAcknowledge
        );
        let expected = if wrote_before_crash {
            // The marker proves the target already holds the event: acknowledge, no rewrite.
            StepOutcome::Acknowledged {
                version: 1,
                wrote: false,
            }
        } else {
            StepOutcome::Projected {
                version: 1,
                rebuilt: false,
            }
        };
        assert_eq!(other.step().await.unwrap(), expected, "{point:?}");
        assert_projected(&cg, &g, &w.state(&g, &c1).await, &c1, 1).await;
        let status = w.status(&g).await;
        assert_eq!(
            (
                status.projected_ref_version,
                status.pending_events,
                status.leased
            ),
            (Some(1), 0, false),
            "{point:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn lost_or_corrupt_markers_are_detected_and_rebuilt_from_the_ledger() {
    let w = World::new().await;
    let (g, cg) = w.graph("tenant-it").await;
    let p = w.healthy().await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    p.step().await.unwrap();
    // Marker lost while the graph stays populated.
    raw_update(&format!(
        "DELETE WHERE {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> ?p ?o }} }}"
    ))
    .await;
    let c2 = w.accept(&g, Some(&c1), &[Q2]).await;
    assert_eq!(
        p.step().await.unwrap(),
        StepOutcome::Projected {
            version: 2,
            rebuilt: true
        }
    );
    assert_projected(&cg, &g, &w.state(&g, &c2).await, &c2, 2).await;
    // Marker corrupted (a second refVersion value).
    raw_update(&format!(
        "INSERT DATA {{ GRAPH <{MARKER_GRAPH}> {{ <{cg}> <urn:sculpin:ledger-projection:v1#refVersion> \
         \"7\"^^<http://www.w3.org/2001/XMLSchema#integer> }} }}"
    ))
    .await;
    let c3 = w.accept(&g, Some(&c2), &[Q3]).await;
    assert_eq!(
        p.step().await.unwrap(),
        StepOutcome::Projected {
            version: 3,
            rebuilt: true
        }
    );
    assert_projected(&cg, &g, &w.state(&g, &c3).await, &c3, 3).await;
    // Out-of-band edit with nothing pending: verify detects it, a rebuild repairs it.
    raw_update(&format!(
        "INSERT DATA {{ GRAPH <{cg}> {{ <urn:x> <urn:y> \"stray\" }} }}"
    ))
    .await;
    let report = p.verify(&w.key(&g)).await.unwrap();
    assert!(!report.consistent, "{}", report.detail);
    assert_eq!(
        p.rebuild(&w.key(&g)).await.unwrap(),
        Some(StepOutcome::Projected {
            version: 3,
            rebuilt: true
        })
    );
    assert!(p.verify(&w.key(&g)).await.unwrap().consistent);
    assert_eq!(w.status(&g).await.rebuilds, 3);
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn a_marker_ahead_of_the_ledger_is_never_regressed_automatically() {
    let w = World::new().await;
    let (g, cg) = w.graph("tenant-it").await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    // The target claims version 99 (e.g. a ledger restored from an older backup).
    let s1 = w.state(&g, &c1).await;
    target()
        .write(
            &cg,
            &ProjectedState::from_state(&s1).unwrap(),
            &ProjectionMarker {
                graph_id: g.clone(),
                branch: "main".into(),
                commit: CommitId(ContentId::for_bytes(b"from the future")),
                ref_version: 99,
                state_digest: ContentId::for_bytes(b"x"),
                triple_count: 1,
            },
            WriteMode::Replace,
        )
        .await
        .unwrap();
    let p = w.healthy().await;
    match p.step().await.unwrap() {
        StepOutcome::Failed { code, class } => {
            assert_eq!(
                (code, class),
                (ProjectionErrorCode::MarkerAhead, ErrorClass::Permanent)
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(w.status(&g).await.status, "rebuild_required");
    w.due_now(&g).await;
    assert_eq!(
        p.step().await.unwrap(),
        StepOutcome::Idle,
        "not claimed while recovery is pending"
    );
    // An operator rebuild decides: the ledger is authoritative.
    assert_eq!(
        p.rebuild(&w.key(&g)).await.unwrap(),
        Some(StepOutcome::Projected {
            version: 1,
            rebuilt: true
        })
    );
    assert_projected(&cg, &g, &s1, &c1, 1).await;
    assert_eq!(w.status(&g).await.status, "active");
    // Rebuild is idempotent.
    assert_eq!(
        p.rebuild(&w.key(&g)).await.unwrap(),
        Some(StepOutcome::Projected {
            version: 1,
            rebuilt: true
        })
    );
    assert_projected(&cg, &g, &s1, &c1, 1).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn concurrent_workers_and_many_graphs_never_mix_or_regress() {
    let w = World::new().await;
    let a = w.healthy().await;
    let b = w.healthy().await;
    // Two workers racing for one stream: one projects, the other finds nothing claimable.
    let (g, cg) = w.graph("tenant-it").await;
    let c1 = w.accept(&g, None, &[Q1]).await;
    let (ra, rb) = tokio::join!(a.step(), b.step());
    let mut outcomes = [ra.unwrap(), rb.unwrap()];
    outcomes.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(
        outcomes,
        [
            StepOutcome::Idle,
            StepOutcome::Projected {
                version: 1,
                rebuilt: false
            }
        ]
    );
    assert_projected(&cg, &g, &w.state(&g, &c1).await, &c1, 1).await;
    // Many graphs, two projector processes running their loops concurrently.
    let mut graphs = Vec::new();
    for i in 0..6 {
        let (gi, cgi) = w.graph("tenant-it").await;
        let mut head = None;
        for j in 0..=i % 3 {
            let quad = format!("<urn:g{i}:s{j}> <urn:p> \"{i}-{j}\" .");
            head = Some(w.accept(&gi, head.as_ref(), &[quad.as_str()]).await);
        }
        graphs.push((gi, cgi, head.unwrap(), i % 3 + 1));
    }
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let a = Arc::new(a);
    let b = Arc::new(b);
    let ta = tokio::spawn(a.clone().run(stop_rx.clone()));
    let tb = tokio::spawn(b.clone().run(stop_rx));
    for _ in 0..200 {
        let mut done = true;
        for (gi, ..) in &graphs {
            done &= w.status(gi).await.lag_versions() == 0;
        }
        if done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = stop_tx.send(true);
    let _ = tokio::join!(ta, tb);
    for (gi, cgi, head, version) in &graphs {
        let state = w.state(gi, head).await;
        assert_projected(cgi, gi, &state, head, *version as i64).await;
        // No other graph's statements leaked in.
        for q in target().read_graph(cgi).await.unwrap() {
            assert!(state.contains(&q), "{q} leaked into {cgi}");
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Fuseki: LEDGER_TEST_DATABASE_URL, LEDGER_TEST_FUSEKI_URL"]
async fn named_graph_state_blocks_the_stream_visibly() {
    let w = World::new().await;
    let (g, cg) = w.graph("tenant-it").await;
    w.accept(&g, None, &["<urn:s> <urn:p> <urn:o> <urn:named> ."])
        .await;
    let p = w.healthy().await;
    match p.step().await.unwrap() {
        StepOutcome::Failed { code, class } => assert_eq!(
            (code, class),
            (
                ProjectionErrorCode::NamedGraphUnsupported,
                ErrorClass::Permanent
            )
        ),
        other => panic!("{other:?}"),
    }
    let status = w.status(&g).await;
    assert_eq!(status.status, "blocked");
    assert_eq!(
        status.last_error_code.as_deref(),
        Some("NAMED_GRAPH_UNSUPPORTED")
    );
    assert!(
        target().read_graph(&cg).await.unwrap().is_empty(),
        "nothing written"
    );
}
