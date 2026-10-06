//! Phase 2 end-to-end over HTTP and real PostgreSQL (Plan 0006 P2.3/P2.6): every ADR-0014
//! scenario with a deterministic fake validator, revalidation, idempotency, security and
//! resource limits. The fake never interprets RDF semantically; it applies scripted rules to
//! quad text and context hints so each scenario is reproducible. Every test is `#[ignore]`
//! and runs through `scripts/test-integration.sh` with `LEDGER_TEST_DATABASE_URL`.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use ledger_api::{
    AcceptancePolicy, ApiLimits, AppState, ValidationService,
    auth::{ClaimsPolicy, DevHs256Authenticator, SharedAuthenticator},
};
use ledger_core::{ContentId, GraphId, TenantId};
use ledger_store::{
    DbSessionLimits, FailPoint, GraphStatus, NewGraph, PostgresLedgerStore, V1Binding,
    ValidationTrustPolicy,
};
use ledger_validation_protocol::{
    BaseKb, EffectiveContext, Ontology, OutcomeKind, Reasoning, ReportReference,
    SemanticEnvironment, SemanticExecutionContext, ShapeSet, VALIDATION_RESPONSE_PROTOCOL,
    ValidationClient, ValidationClientError, ValidationOutcome, ValidationRequest,
    ValidatorResponse, ValidatorVersions, ViolationSummary, VirtualContextRef,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tower::ServiceExt;

const ISSUER: &str = "https://dev-issuer.example/";
const AUDIENCE: &str = "api://sculpin-ledger-test";
const SECRET: &[u8] = b"development-only-secret-that-is-at-least-32-bytes-long";
const SERVICE_ID: &str = "urn:sculpin:service:semantic-validator";

fn database_url() -> String {
    std::env::var("LEDGER_TEST_DATABASE_URL").expect("LEDGER_TEST_DATABASE_URL must be set")
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "{prefix}-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn token(tenant: &str, subject: &str, roles: &[&str]) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &json!({
            "iss": ISSUER, "aud": AUDIENCE, "exp": now + 600, "nbf": now - 5,
            "tid": tenant, "oid": subject, "sculpin_principal_type": "service", "roles": roles,
        }),
        &jsonwebtoken::EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------------------
// Deterministic fake validator

#[derive(Clone)]
enum Mode {
    Normal,
    Unavailable,
    Refuses,
    WrongCandidate,
    /// Answers with the default catalog revision whatever the hint says.
    WrongRevision,
    Slow(Duration),
    /// Wait until the notify fires (to saturate the validation budget).
    Blocked(Arc<tokio::sync::Notify>),
}

/// One logical validation per invocation id (the Sculpin contract): the first delivery
/// computes, every other delivery with the same id waits for and shares its result.
type Inflight = Arc<tokio::sync::OnceCell<ValidatorResponse>>;

struct FakeValidator {
    mode: Mutex<Mode>,
    /// Physical deliveries.
    calls: AtomicUsize,
    /// Logical validations actually computed.
    logical: AtomicUsize,
    /// The invocation id of every physical delivery, in arrival order.
    invocations: Mutex<Vec<String>>,
    /// The validator's live base-KB revision: its environment may move between calls.
    kb_revision: Mutex<String>,
    /// `Some` = honour the invocation identity (deduplicate); `None` = every delivery computes.
    dedup: Mutex<Option<std::collections::HashMap<String, Inflight>>>,
    /// Held by the next logical validation (after it snapshotted its environment).
    gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Compute (and remember) the next result, then never answer: the ledger request that
    /// is waiting dies before it can record anything.
    crash_after_answer: std::sync::atomic::AtomicBool,
}

impl FakeValidator {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            mode: Mutex::new(Mode::Normal),
            calls: AtomicUsize::new(0),
            logical: AtomicUsize::new(0),
            invocations: Mutex::new(Vec::new()),
            kb_revision: Mutex::new("kbrev-7".into()),
            dedup: Mutex::new(None),
            gate: Mutex::new(None),
            crash_after_answer: std::sync::atomic::AtomicBool::new(false),
        })
    }
    fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn logical(&self) -> usize {
        self.logical.load(Ordering::SeqCst)
    }
    fn invocations(&self) -> Vec<String> {
        self.invocations.lock().unwrap().clone()
    }
    fn honour_invocation_identity(&self) {
        *self.dedup.lock().unwrap() = Some(std::collections::HashMap::new());
    }
    fn set_kb_revision(&self, revision: &str) {
        *self.kb_revision.lock().unwrap() = revision.into();
    }
    fn hold_next_validation(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.gate.lock().unwrap() = Some(gate.clone());
        gate
    }
    fn crash_after_next_answer(&self) {
        self.crash_after_answer.store(true, Ordering::SeqCst);
    }
    /// Wait (bounded) until `n` physical deliveries have arrived.
    async fn wait_for_calls(&self, n: usize) {
        for _ in 0..1000 {
            if self.calls() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the validator saw {} of {n} expected calls", self.calls());
    }
    /// Wait (bounded) until `n` logical validations have snapshotted their environment.
    async fn wait_for_logical(&self, n: usize) {
        for _ in 0..1000 {
            if self.logical() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "the validator started {} of {n} logical validations",
            self.logical()
        );
    }
}

fn digest(s: &str) -> ContentId {
    ContentId::for_bytes(s.as_bytes())
}

fn violation(code: &str, message: &str) -> ViolationSummary {
    ViolationSummary {
        severity: "Violation".into(),
        code: code.into(),
        message: message.into(),
    }
}

#[async_trait::async_trait]
impl ValidationClient for FakeValidator {
    async fn validate(
        &self,
        request: &ValidationRequest,
    ) -> Result<ValidatorResponse, ValidationClientError> {
        self.invocations
            .lock()
            .unwrap()
            .push(request.invocation_id.to_string());
        self.calls.fetch_add(1, Ordering::SeqCst);
        let inflight = self.dedup.lock().unwrap().as_mut().map(|seen| {
            seen.entry(request.invocation_id.to_string())
                .or_default()
                .clone()
        });
        let result = match inflight {
            Some(cell) => cell
                .get_or_try_init(|| self.validate_once(request))
                .await
                .cloned(),
            None => self.validate_once(request).await,
        };
        if self.crash_after_answer.swap(false, Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        result
    }
    fn describe(&self) -> String {
        "deterministic fake validator".into()
    }
}

impl FakeValidator {
    /// One logical validation: snapshots the live environment when it starts.
    async fn validate_once(
        &self,
        request: &ValidationRequest,
    ) -> Result<ValidatorResponse, ValidationClientError> {
        let kb_revision = self.kb_revision.lock().unwrap().clone();
        self.logical.fetch_add(1, Ordering::SeqCst);
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let mode = self.mode.lock().unwrap().clone();
        match mode {
            Mode::Normal | Mode::WrongCandidate | Mode::WrongRevision => {}
            Mode::Unavailable => {
                return Err(ValidationClientError::Unavailable(
                    "validator answered HTTP 503".into(),
                ));
            }
            Mode::Refuses => {
                return Err(ValidationClientError::Rejected(
                    "validator refused the request (HTTP 400)".into(),
                ));
            }
            Mode::Slow(ref d) => tokio::time::sleep(*d).await,
            Mode::Blocked(ref n) => n.notified().await,
        }
        let hints = &request.requested;
        let ontology_version = hints
            .ontology
            .as_ref()
            .map_or("O1".to_owned(), |o| o.version.clone());
        // The fake's catalog: revision "catalog-<X>" hydrates external version <X>.
        let external = if matches!(mode, Mode::WrongRevision) {
            "D-A".to_owned()
        } else {
            hints
                .sources_revision
                .as_deref()
                .and_then(|r| r.strip_prefix("catalog-"))
                .unwrap_or("D-A")
                .to_owned()
        };
        let reasoning = hints.reasoning_profile.clone().map(|profile| Reasoning {
            profile,
            implementation: "sculpin-python-reasoner".into(),
            version: "0.9".into(),
        });
        let text = request.candidate.quads.join("\n");
        let mut violations = Vec::new();
        if text.contains("invalid-shacl") {
            violations.push(violation("sh:MinCountConstraintComponent", "missing label"));
        }
        if reasoning.as_ref().is_some_and(|r| r.profile == "owl-rl") && text.contains("disjoint") {
            violations.push(violation(
                "reasoning:owl-disjoint",
                "inferred membership of disjoint classes",
            ));
        }
        if external == "D-B" && text.contains("abox-dependent") {
            violations.push(violation(
                "vabox:measurement-out-of-range",
                "external measurement contradicts the candidate",
            ));
        }
        let outcome = if violations.is_empty() {
            ValidationOutcome::conforms()
        } else {
            ValidationOutcome {
                kind: OutcomeKind::Violations,
                violation_count: violations.len() as u32,
                violations,
            }
        };
        let candidate_commit = if matches!(mode, Mode::WrongCandidate) {
            ledger_core::CommitId(digest("some other candidate"))
        } else {
            request.candidate.commit.clone()
        };
        Ok(ValidatorResponse {
            protocol: VALIDATION_RESPONSE_PROTOCOL.into(),
            candidate_commit,
            candidate_state_digest: request.candidate.state_digest.clone(),
            context: EffectiveContext {
                base_kb: BaseKb {
                    kb_id: "urn:exodus:kb:material-science".into(),
                    revision: kb_revision,
                },
                ontology: Some(Ontology {
                    id: "urn:sculpin:ontology:core".into(),
                    version: ontology_version,
                }),
                shapes: ShapeSet {
                    id: "urn:sculpin:shapes:material".into(),
                    version: "S1".into(),
                },
                reasoning,
                // Sculpin's catalog revision follows the external version in force.
                sources_revision: Some(format!("catalog-{external}")),
                // Which dataset a run hydrates depends on the candidate (provenance only).
                virtual_contexts: vec![VirtualContextRef {
                    dataset_id: if text.contains("reference-data") {
                        "urn:sculpin:datasource:reference".into()
                    } else {
                        "urn:sculpin:datasource:lab".into()
                    },
                    source_version: external.clone(),
                    object_refs: vec!["s3://lab/run-1.parquet".into()],
                    query_spec_digest: digest("query"),
                    hydration_plan_digest: digest("plan"),
                }],
                validator: ValidatorVersions {
                    service_version: "2026.09.1".into(),
                    configuration_version: "cfg-1".into(),
                },
            },
            outcome,
            report: ReportReference {
                digest: digest(&format!("report for {}", request.candidate.commit)),
                reference: Some("urn:sculpin:validation-report:fake".into()),
            },
        })
    }
}

// ---------------------------------------------------------------------------------------
// Harness (runtime identity behind the router, owner for provisioning and assertions)

struct Harness {
    owner: PostgresLedgerStore,
    app: Router,
    validator: Arc<FakeValidator>,
    runtime_url: String,
    limits: ApiLimits,
}

type Reply = (StatusCode, Value);

async fn harness_with(limits: ApiLimits, with_validator: bool) -> Harness {
    let url = database_url();
    let owner = PostgresLedgerStore::connect_and_migrate(&url, V1Binding::Reject)
        .await
        .unwrap();
    static SETUP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SETUP
        .get_or_init(|| async {
            use sqlx::Connection;
            let mut conn = sqlx::postgres::PgConnection::connect(&database_url())
                .await
                .unwrap();
            ledger_store::schema::migrate_all_on(&mut conn).await.unwrap();
            sqlx::query(
                "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'ledger_rt_api') THEN \
                 CREATE ROLE ledger_rt_api LOGIN PASSWORD 'rt-api-test-secret'; END IF; END $$",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            ledger_store::schema::grant_runtime_role(&mut conn, "ledger_rt_api")
                .await
                .unwrap();
            conn.close().await.unwrap();
        })
        .await;
    let runtime_url = {
        let (scheme, rest) = url.split_once("://").unwrap();
        let (_, host_part) = rest.rsplit_once('@').unwrap();
        format!("{scheme}://ledger_rt_api:rt-api-test-secret@{host_part}")
    };
    let validator = FakeValidator::new();
    let service = with_validator.then_some(SERVICE_ID);
    let app = app_for(&runtime_url, limits, &validator, service, service).await;
    Harness {
        owner,
        app,
        validator,
        runtime_url,
        limits,
    }
}

/// A router over the runtime identity with an explicit validator configuration: `trust` is
/// the trusted service id (`LEDGER_VALIDATOR_SERVICE_ID`), `client` the service the
/// configured endpoint belongs to (`LEDGER_VALIDATOR_URL`), both optional and independent.
async fn app_for(
    runtime_url: &str,
    limits: ApiLimits,
    validator: &Arc<FakeValidator>,
    trust: Option<&str>,
    client: Option<&str>,
) -> Router {
    app_with_failpoint(runtime_url, limits, validator, trust, client, None).await
}

async fn app_with_failpoint(
    runtime_url: &str,
    limits: ApiLimits,
    validator: &Arc<FakeValidator>,
    trust: Option<&str>,
    client: Option<&str>,
    failpoint: Option<FailPoint>,
) -> Router {
    let mut store = PostgresLedgerStore::connect_with(
        runtime_url,
        V1Binding::Reject,
        DbSessionLimits::default(),
    )
    .await
    .expect("runtime identity verify-only start-up");
    if let Some(point) = failpoint {
        store = store.with_validation_failpoint(point);
    }
    let auth: SharedAuthenticator = Arc::new(
        DevHs256Authenticator::new(
            ISSUER.into(),
            AUDIENCE.into(),
            SECRET,
            ClaimsPolicy::default(),
        )
        .unwrap(),
    );
    let mut state = AppState::new(store, auth, limits, AcceptancePolicy::RequireValidation);
    if let Some(trust) = trust {
        state = state.with_validation_trust(ValidationTrustPolicy::single(trust).unwrap());
    }
    if let Some(service_id) = client {
        state = state.with_validation(ValidationService {
            client: validator.clone(),
            service_id: service_id.into(),
        });
    }
    ledger_api::router(state)
}

async fn harness() -> Harness {
    harness_with(ApiLimits::default(), true).await
}

impl Harness {
    /// The same database and fake validator behind a freshly started server with another
    /// validator configuration (a restart / redeploy).
    async fn restarted(&self, trust: Option<&str>, client: Option<&str>) -> Harness {
        Harness {
            owner: self.owner.clone(),
            app: app_for(
                &self.runtime_url,
                self.limits,
                &self.validator,
                trust,
                client,
            )
            .await,
            validator: self.validator.clone(),
            runtime_url: self.runtime_url.clone(),
            limits: self.limits,
        }
    }

    /// Like [`Harness::restarted`] with the validation record transaction failing at `point`
    /// (the server dies after the validator answered, before the record commits).
    async fn crashing_at(&self, point: FailPoint) -> Harness {
        Harness {
            owner: self.owner.clone(),
            app: app_with_failpoint(
                &self.runtime_url,
                self.limits,
                &self.validator,
                Some(SERVICE_ID),
                Some(SERVICE_ID),
                Some(point),
            )
            .await,
            validator: self.validator.clone(),
            runtime_url: self.runtime_url.clone(),
            limits: self.limits,
        }
    }

    async fn graph(&self, tenant: &str) -> GraphId {
        let id = GraphId::new(unique("val-api")).unwrap();
        self.owner
            .graphs()
            .create(&NewGraph {
                graph_id: id.clone(),
                tenant_id: TenantId::new(tenant).unwrap(),
                knowledge_base_id: Some("urn:exodus:kb:material-science".into()),
                purpose: None,
                status: GraphStatus::Active,
            })
            .await
            .unwrap();
        id
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        bearer: &str,
        key: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn prepare(&self, g: &GraphId, t: &str, head: Option<&str>, quad: &str) -> String {
        let (status, body) = self
            .call(
                "POST",
                &format!("/v1/graphs/{g}/proposals"),
                t,
                Some(&unique("p")),
                Some(json!({
                    "ref": "main", "expected_head": head,
                    "operations": [{"op": "add", "quad": quad}],
                    "activity": "cognitive-correction", "message": "m",
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["candidate"].as_str().unwrap().to_owned()
    }

    async fn validate(&self, g: &GraphId, t: &str, c: &str, key: &str, requested: Value) -> Reply {
        self.call(
            "POST",
            &format!("/v1/graphs/{g}/proposals/{c}/validations"),
            t,
            Some(key),
            Some(json!({ "requested": requested })),
        )
        .await
    }

    async fn accept(
        &self,
        g: &GraphId,
        t: &str,
        c: &str,
        head: Option<&str>,
        key: &str,
        validation: Option<(&Value, &Value)>,
    ) -> Reply {
        let mut body = json!({"ref": "main", "expected_head": head, "reason": "reviewed"});
        if let Some((vid, env)) = validation {
            body["validation_id"] = vid.clone();
            body["semantic_environment_id"] = env.clone();
        }
        self.call(
            "POST",
            &format!("/v1/graphs/{g}/proposals/{c}/accept"),
            t,
            Some(key),
            Some(body),
        )
        .await
    }

    async fn head(&self, g: &GraphId) -> Option<(String, i64)> {
        self.owner
            .ref_head(g, "main")
            .await
            .unwrap()
            .map(|(h, v)| (h.to_string(), v))
    }

    async fn count(&self, sql: &str, g: &GraphId) -> i64 {
        sqlx::query(sql)
            .bind(g.as_str())
            .fetch_one(self.owner.pool())
            .await
            .unwrap()
            .get::<i64, _>(0)
    }

    /// Every table an accepted-workflow step or a validation writes, for the graph.
    async fn footprint(&self, g: &GraphId) -> Vec<i64> {
        let mut out = Vec::new();
        for sql in [
            "SELECT count(*) FROM ref_events WHERE graph_id = $1",
            "SELECT count(*) FROM decisions WHERE graph_id = $1",
            "SELECT count(*) FROM projection_outbox WHERE graph_id = $1",
            "SELECT count(*) FROM decision_validations WHERE graph_id = $1",
            "SELECT count(*) FROM validation_records WHERE graph_id = $1",
            "SELECT count(*) FROM semantic_execution_contexts WHERE graph_id = $1",
            "SELECT count(*) FROM idempotency WHERE graph_id = $1 AND operation IN ('accept', 'validate')",
            "SELECT coalesce(max(version), 0) FROM refs WHERE graph_id = $1",
        ] {
            out.push(self.count(sql, g).await);
        }
        out
    }
}

const ROLES: [&str; 4] = [
    "ledger.read",
    "ledger.propose",
    "ledger.review",
    "ledger.validate",
];

fn assert_code(reply: &Reply, status: StatusCode, code: &str) {
    assert_eq!(reply.0, status, "{}", reply.1);
    assert_eq!(reply.1["code"], code, "{}", reply.1);
}

/// The environment id an orchestrator would name for `context` with another ontology
/// version: computed from the frozen layout without the ledger, as Sculpin would.
fn environment_with_ontology(context: &Value, version: &str) -> Value {
    let mut context: SemanticExecutionContext = serde_json::from_value(context.clone()).unwrap();
    context.ontology.as_mut().unwrap().version = version.into();
    json!(context.environment_id().unwrap().to_string())
}

// ---------------------------------------------------------------------------------------
// ADR-0014 scenarios

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn valid_candidate_is_validated_then_accepted_and_the_ref_moves() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let (status, v) = h.validate(&g, &t, &c, "v1", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["conforms"], true);
    assert_eq!(v["candidate"], c);
    assert_eq!(
        v["record"]["validator"]["service_id"], SERVICE_ID,
        "service id is ledger configuration"
    );
    assert_eq!(
        v["context"]["candidate_state_digest"],
        v["record"]["candidate_state_digest"]
    );
    assert_eq!(h.head(&g).await, None, "validation never moves a ref");
    let (status, a) = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a1",
            Some((&v["validation_id"], &v["semantic_environment_id"])),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(h.head(&g).await, Some((c.clone(), 1)));
    let linked = h
        .count(
            "SELECT count(*) FROM decision_validations WHERE graph_id = $1",
            &g,
        )
        .await;
    assert_eq!(linked, 1);
    // Lost accept response: the same key replays the same decision.
    let (status, replay) = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a1",
            Some((&v["validation_id"], &v["semantic_environment_id"])),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["decision_id"], a["decision_id"]);
    // The record is readable, scoped to its candidate.
    let (status, read) = h
        .call(
            "GET",
            &format!(
                "/v1/graphs/{g}/proposals/{c}/validations/{}",
                v["validation_id"].as_str().unwrap()
            ),
            &t,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["record"], v["record"]);
    assert_eq!(
        read["semantic_environment_id"],
        v["semantic_environment_id"]
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn invalid_shacl_candidate_is_refused_and_can_be_rejected_citing_its_validation() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:invalid-shacl> <urn:p> \"x\" .")
        .await;
    let (status, v) = h.validate(&g, &t, &c, "v1", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["conforms"], false);
    assert_eq!(
        v["record"]["outcome"]["violations"][0]["code"],
        "sh:MinCountConstraintComponent"
    );
    let before = h.footprint(&g).await;
    let refused = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a1",
            Some((&v["validation_id"], &v["semantic_environment_id"])),
        )
        .await;
    assert_code(&refused, StatusCode::CONFLICT, "VALIDATION_REJECTED");
    assert_eq!(h.head(&g).await, None);
    assert_eq!(
        h.footprint(&g).await,
        before,
        "a refused acceptance leaves nothing"
    );
    let (status, rejected) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals/{c}/reject"),
            &t,
            Some("r1"),
            Some(json!({"ref": "main", "reason": "SHACL violations", "validation_id": v["validation_id"]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rejected}");
    let row = sqlx::query(
        "SELECT d.decision, (SELECT dv.validation_id FROM decision_validations dv WHERE dv.decision_id = d.decision_id) AS cited \
         FROM decisions d WHERE d.graph_id = $1",
    )
    .bind(g.as_str())
    .fetch_one(h.owner.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("decision"), "rejected");
    assert_eq!(
        row.get::<String, _>("cited"),
        v["validation_id"].as_str().unwrap()
    );
    assert_eq!(h.head(&g).await, None, "rejection never moves a ref");
    let commits = h
        .count("SELECT count(*) FROM commit_index WHERE graph_id = $1", &g)
        .await;
    assert_eq!(commits, 1, "the rejected candidate stays in history");
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn reasoning_derived_violation_names_the_reasoning_profile_and_is_refused() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:disjoint> <urn:p> \"x\" .")
        .await;
    // Without reasoning the candidate conforms (no directly asserted shape error)...
    let (_, plain) = h.validate(&g, &t, &c, "v-plain", json!({})).await;
    assert_eq!(plain["conforms"], true, "{plain}");
    assert!(
        plain["context"].get("reasoning").is_none(),
        "no reasoning ran"
    );
    // ...with OWL-RL reasoning it does not, and the context names the profile.
    let (status, v) = h
        .validate(&g, &t, &c, "v-owl", json!({"reasoning_profile": "owl-rl"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["conforms"], false);
    assert_eq!(v["context"]["reasoning"]["profile"], "owl-rl");
    assert_eq!(
        v["record"]["outcome"]["violations"][0]["code"],
        "reasoning:owl-disjoint"
    );
    assert_ne!(
        v["semantic_environment_id"],
        plain["semantic_environment_id"]
    );
    let refused = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a1",
            Some((&v["validation_id"], &v["semantic_environment_id"])),
        )
        .await;
    assert_code(&refused, StatusCode::CONFLICT, "VALIDATION_REJECTED");
    assert_eq!(h.head(&g).await, None);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn virtual_abox_versions_yield_distinct_contexts_that_coexist_immutably() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(
            &g,
            &t,
            None,
            "<urn:material:abox-dependent> <urn:p> \"x\" .",
        )
        .await;
    let pin = |v: &str| json!({"sources_revision": format!("catalog-{v}")});
    let (sa, a) = h.validate(&g, &t, &c, "v-a", pin("D-A")).await;
    let (sb, b) = h.validate(&g, &t, &c, "v-b", pin("D-B")).await;
    assert_eq!(
        (sa, sb),
        (StatusCode::CREATED, StatusCode::CREATED),
        "{a} {b}"
    );
    assert_eq!(a["conforms"], true, "external version A conforms");
    assert_eq!(b["conforms"], false, "external version B does not");
    assert_ne!(a["semantic_context_id"], b["semantic_context_id"]);
    assert_ne!(a["semantic_environment_id"], b["semantic_environment_id"]);
    assert_eq!(a["context"]["virtual_contexts"][0]["source_version"], "D-A");
    assert_eq!(b["context"]["virtual_contexts"][0]["source_version"], "D-B");
    // Only identifying provenance is stored: no triple table exists for external data.
    let records = h
        .count(
            "SELECT count(*) FROM validation_records WHERE graph_id = $1",
            &g,
        )
        .await;
    assert_eq!(records, 2, "both records coexist");
    for v in [&a, &b] {
        let (status, read) = h
            .call(
                "GET",
                &format!(
                    "/v1/graphs/{g}/proposals/{c}/validations/{}",
                    v["validation_id"].as_str().unwrap()
                ),
                &t,
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(read["record"], v["record"]);
    }
    // Acceptance on B is refused, on A succeeds.
    let refused = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a-b",
            Some((&b["validation_id"], &b["semantic_environment_id"])),
        )
        .await;
    assert_code(&refused, StatusCode::CONFLICT, "VALIDATION_REJECTED");
    let (status, _) = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a-a",
            Some((&a["validation_id"], &a["semantic_environment_id"])),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.head(&g).await, Some((c, 1)));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn stale_environment_is_refused_and_revalidation_under_the_required_one_accepts() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    // Validated under O1 / S1 / D-A...
    let (_, v1) = h.validate(&g, &t, &c, "v-o1", json!({})).await;
    assert_eq!(v1["conforms"], true, "{v1}");
    assert_eq!(v1["context"]["ontology"]["version"], "O1");
    // ...but the orchestrator requires O2 / S1 / D-A (Sculpin declares that environment
    // current; its id is computable without the ledger or a candidate).
    let required = environment_with_ontology(&v1["context"], "O2");
    assert_ne!(required, v1["semantic_environment_id"]);
    let before = h.footprint(&g).await;
    let stale = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a-stale",
            Some((&v1["validation_id"], &required)),
        )
        .await;
    assert_code(&stale, StatusCode::CONFLICT, "VALIDATION_STALE");
    assert_eq!(h.head(&g).await, None, "no ref movement");
    assert_eq!(h.footprint(&g).await, before);
    // Revalidation of the unchanged candidate under O2 produces a second record whose
    // environment is exactly the required one; acceptance with it succeeds.
    let (_, v2) = h
        .validate(
            &g,
            &t,
            &c,
            "v-o2",
            json!({"ontology": {"id": "urn:sculpin:ontology:core", "version": "O2"}}),
        )
        .await;
    assert_eq!(v2["semantic_environment_id"], required, "{v2}");
    assert_ne!(v2["validation_id"], v1["validation_id"]);
    let (status, a) = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a-o2",
            Some((&v2["validation_id"], &required)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(h.head(&g).await, Some((c.clone(), 1)));
    let records = h
        .count(
            "SELECT count(*) FROM validation_records WHERE graph_id = $1",
            &g,
        )
        .await;
    assert_eq!(records, 2, "the stale record remains auditable");
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn validator_outage_keeps_the_ledger_operable_and_acceptance_fail_closed() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    h.validator.set(Mode::Unavailable);
    let before = h.footprint(&g).await;
    let down = h.validate(&g, &t, &c, "v1", json!({})).await;
    assert_code(
        &down,
        StatusCode::SERVICE_UNAVAILABLE,
        "VALIDATOR_UNAVAILABLE",
    );
    assert_eq!(h.footprint(&g).await, before, "nothing recorded");
    // Prepare and reads still work while the validator is down.
    let c2 = h
        .prepare(&g, &t, None, "<urn:material:b> <urn:label> \"B\" .")
        .await;
    let (status, _) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/commits/{c2}/state"),
            &t,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    // Acceptance without a validation is fail-closed.
    let required = h.accept(&g, &t, &c, None, "a1", None).await;
    assert_code(&required, StatusCode::CONFLICT, "VALIDATION_REQUIRED");
    assert_eq!(h.head(&g).await, None, "history unchanged");
    // A timeout is an outage too; the retry with the same key after recovery succeeds.
    let short = ApiLimits {
        validator_timeout: Duration::from_millis(200),
        ..ApiLimits::default()
    };
    let slow = harness_with(short, true).await;
    let gs = slow.graph("tenant-v").await;
    let cs = slow
        .prepare(&gs, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    slow.validator.set(Mode::Slow(Duration::from_secs(2)));
    let timed_out = slow.validate(&gs, &t, &cs, "v1", json!({})).await;
    assert_code(
        &timed_out,
        StatusCode::SERVICE_UNAVAILABLE,
        "VALIDATOR_UNAVAILABLE",
    );
    slow.validator.set(Mode::Normal);
    let (status, retried) = slow.validate(&gs, &t, &cs, "v1", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{retried}");
    // No validator configured at all: the same stable code.
    let none = harness_with(ApiLimits::default(), false).await;
    let gn = none.graph("tenant-v").await;
    let cn = none
        .prepare(&gn, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let unconfigured = none.validate(&gn, &t, &cn, "v1", json!({})).await;
    assert_code(
        &unconfigured,
        StatusCode::SERVICE_UNAVAILABLE,
        "VALIDATOR_UNAVAILABLE",
    );
    // A validator that refuses, or answers about another candidate, is VALIDATOR_ERROR
    // and nothing is recorded.
    h.validator.set(Mode::Refuses);
    let refused = h.validate(&g, &t, &c, "v-refused", json!({})).await;
    assert_code(&refused, StatusCode::BAD_GATEWAY, "VALIDATOR_ERROR");
    h.validator.set(Mode::WrongCandidate);
    let wrong = h.validate(&g, &t, &c, "v-wrong", json!({})).await;
    assert_code(&wrong, StatusCode::BAD_GATEWAY, "VALIDATOR_ERROR");
    assert_eq!(h.footprint(&g).await, before);
}

/// ADR-0019 trust anchor across restarts: trust is `LEDGER_VALIDATOR_SERVICE_ID`, never the
/// presence of an endpoint. S1-trusted records keep satisfying acceptance through a validator
/// outage; a server trusting S2 or trusting nothing refuses them; nothing moves on refusal.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn validator_trust_is_independent_of_the_endpoint_and_fails_closed() {
    const S2: &str = "urn:sculpin:service:another-validator";
    let h = harness().await; // trusts S1 and can call it
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let (status, v) = h.validate(&g, &t, &c, "v1", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let cited = (&v["validation_id"], &v["semantic_environment_id"]);

    // Restart trusting S1 with no endpoint (outage / endpoint removed).
    let outage = h.restarted(Some(SERVICE_ID), None).await;
    let c2 = outage
        .prepare(&g, &t, None, "<urn:material:b> <urn:label> \"B\" .")
        .await;
    let before = outage.footprint(&g).await;
    let calls = h.validator.calls();
    let down = outage.validate(&g, &t, &c2, "v2", json!({})).await;
    assert_code(
        &down,
        StatusCode::SERVICE_UNAVAILABLE,
        "VALIDATOR_UNAVAILABLE",
    );
    assert_eq!(h.validator.calls(), calls, "no endpoint: never called");
    assert_eq!(outage.footprint(&g).await, before, "nothing recorded");
    // The completed validation still replays without an endpoint.
    let (status, replay) = outage.validate(&g, &t, &c, "v1", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["validation_id"], v["validation_id"]);

    // A server trusting S2 (and able to call S2) refuses the S1 record: stale/untrusted.
    let other = h.restarted(Some(S2), Some(S2)).await;
    let refused = other.accept(&g, &t, &c, None, "a-s2", Some(cited)).await;
    assert_code(&refused, StatusCode::CONFLICT, "VALIDATION_STALE");
    assert!(!refused.1.to_string().contains(SERVICE_ID), "{}", refused.1);
    // A development server with no trust anchor refuses every validated acceptance.
    let untrusting = h.restarted(None, None).await;
    let refused = untrusting
        .accept(&g, &t, &c, None, "a-none", Some(cited))
        .await;
    assert_code(&refused, StatusCode::CONFLICT, "VALIDATION_STALE");
    // Neither refusal disclosed anything about the record, and nothing moved.
    assert!(!refused.1.to_string().contains(SERVICE_ID), "{}", refused.1);
    assert_eq!(outage.footprint(&g).await, before);
    assert_eq!(h.head(&g).await, None);

    // Records of S2 are never accepted by the S1-trusting outage server.
    let c3 = other
        .prepare(&g, &t, None, "<urn:material:c> <urn:label> \"C\" .")
        .await;
    let (status, v2) = other.validate(&g, &t, &c3, "v3", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{v2}");
    assert_eq!(v2["record"]["validator"]["service_id"], S2);
    let refused = outage
        .accept(
            &g,
            &t,
            &c3,
            None,
            "a-c3",
            Some((&v2["validation_id"], &v2["semantic_environment_id"])),
        )
        .await;
    assert_code(&refused, StatusCode::CONFLICT, "VALIDATION_STALE");

    // A foreign tenant cannot learn the record exists, whatever the trust configuration.
    let foreign = token("tenant-other", "orchestrator", &ROLES);
    for server in [&outage, &untrusting] {
        let hidden = server
            .accept(&g, &foreign, &c, None, "a-foreign", Some(cited))
            .await;
        assert_code(&hidden, StatusCode::NOT_FOUND, "NOT_FOUND");
    }

    // The S1-trusting outage server accepts the earlier S1 record: the ref moves.
    let (status, accepted) = outage.accept(&g, &t, &c, None, "a1", Some(cited)).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(h.head(&g).await, Some((c.clone(), 1)));
}

// ---------------------------------------------------------------------------------------
// Idempotency, security and limits

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn validation_retries_replay_and_conflicting_reuse_of_a_key_is_refused() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let (s1, first) = h.validate(&g, &t, &c, "v1", json!({})).await;
    assert_eq!(s1, StatusCode::CREATED);
    let calls = h.validator.calls();
    let (s2, again) = h.validate(&g, &t, &c, "v1", json!({})).await;
    assert_eq!(s2, StatusCode::OK, "{again}");
    assert_eq!(again["replayed"], true);
    assert_eq!(again["validation_id"], first["validation_id"]);
    assert_eq!(again["record"], first["record"]);
    assert_eq!(
        h.validator.calls(),
        calls,
        "a replay never calls the validator"
    );
    let conflict = h
        .validate(&g, &t, &c, "v1", json!({"reasoning_profile": "owl-rl"}))
        .await;
    assert_code(&conflict, StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT");
    // Accept with the same key but a different validation is a conflict too (request v2).
    let (_, other) = h
        .validate(&g, &t, &c, "v2", json!({"reasoning_profile": "rdfs"}))
        .await;
    let (sa, _) = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a1",
            Some((&first["validation_id"], &first["semantic_environment_id"])),
        )
        .await;
    assert_eq!(sa, StatusCode::OK);
    let conflict = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a1",
            Some((&other["validation_id"], &other["semantic_environment_id"])),
        )
        .await;
    assert_code(&conflict, StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT");
}

/// Two identical ledger requests racing under one `Idempotency-Key` both reach the validator
/// (neither is recorded while the first is in flight), carry the same invocation identity,
/// and — because the validator honours it — resolve to one logical validation in the
/// environment in force when it started, even though the validator's environment moved in
/// between. One idempotency result and one record are stored; both responses name it.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn concurrent_same_key_validations_are_one_logical_invocation() {
    let h = harness().await;
    h.validator.honour_invocation_identity();
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let gate = h.validator.hold_next_validation();
    let (calls, logical) = (h.validator.calls(), h.validator.logical());
    let first = h.validate(&g, &t, &c, "v-same", json!({}));
    let second = async {
        h.validator.wait_for_logical(logical + 1).await;
        // The validator's live environment moves while the first validation is in flight.
        h.validator.set_kb_revision("kbrev-8");
        h.validate(&g, &t, &c, "v-same", json!({})).await
    };
    let release = async {
        h.validator.wait_for_calls(calls + 2).await;
        gate.notify_one();
    };
    let ((s1, r1), (s2, r2), ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(first, second, release)
    })
    .await
    .expect("the race resolves promptly");
    let mut statuses = [s1, s2];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CREATED],
        "{r1} / {r2}"
    );
    let invocations = h.validator.invocations();
    let sent = &invocations[calls..];
    assert_eq!(sent.len(), 2, "both requests reached the validator");
    assert_eq!(
        sent[0], sent[1],
        "same logical request, same invocation identity"
    );
    assert_eq!(h.validator.logical(), logical + 1, "one logical validation");
    for field in [
        "validation_id",
        "semantic_environment_id",
        "semantic_context_id",
        "record",
    ] {
        assert_eq!(r1[field], r2[field], "{field}");
    }
    assert_eq!(
        r1["context"]["base_kb"]["revision"], "kbrev-7",
        "the environment in force when the logical validation started wins, not arrival order"
    );
    assert_eq!(
        h.count(
            "SELECT count(*) FROM idempotency WHERE graph_id = $1 AND idempotency_key = 'v-same'",
            &g
        )
        .await,
        1
    );
    assert_eq!(
        h.count(
            "SELECT count(*) FROM validation_records WHERE graph_id = $1",
            &g
        )
        .await,
        1
    );
    // Control: the environment really moved — another logical request (another key) sees it
    // under another invocation identity.
    let (status, other) = h.validate(&g, &t, &c, "v-other", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{other}");
    assert_eq!(other["context"]["base_kb"]["revision"], "kbrev-8");
    assert_ne!(h.validator.invocations().last(), Some(&sent[0]));
    assert_ne!(
        other["semantic_environment_id"],
        r1["semantic_environment_id"]
    );
}

/// The validator answered but the answer never reached the ledger (connection lost, client
/// gone: the waiting request is dropped). Nothing was recorded. The retry — on a restarted
/// server, same key and body — carries the same invocation identity, so the validator returns
/// the same logical result even though its environment has moved since.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn a_retry_after_a_lost_answer_reuses_the_invocation() {
    let h = harness().await;
    h.validator.honour_invocation_identity();
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let before = h.footprint(&g).await;
    let (calls, logical) = (h.validator.calls(), h.validator.logical());
    h.validator.crash_after_next_answer();
    // Drop the request once the validator has computed its answer (the logical validation
    // completes within the poll that counts it: no await follows the snapshot).
    tokio::select! {
        reply = h.validate(&g, &t, &c, "v-lost", json!({})) => {
            panic!("the answer must never arrive: {reply:?}")
        }
        () = h.validator.wait_for_logical(logical + 1) => {}
    }
    assert_eq!(
        h.validator.logical(),
        logical + 1,
        "the validator did validate"
    );
    assert_eq!(h.footprint(&g).await, before, "nothing recorded");
    h.validator.set_kb_revision("kbrev-9");
    let restarted = h.restarted(Some(SERVICE_ID), Some(SERVICE_ID)).await;
    let (status, v) = restarted.validate(&g, &t, &c, "v-lost", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let invocations = h.validator.invocations();
    assert_eq!(invocations.len(), calls + 2);
    assert_eq!(
        invocations[calls],
        invocations[calls + 1],
        "retry reuses the identity"
    );
    assert_eq!(
        h.validator.logical(),
        logical + 1,
        "no second logical validation"
    );
    assert_eq!(v["context"]["base_kb"]["revision"], "kbrev-7");
    // It is now durable: a further retry replays without calling the validator.
    let (status, replay) = restarted.validate(&g, &t, &c, "v-lost", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["validation_id"], v["validation_id"]);
    assert_eq!(h.validator.calls(), calls + 2);
}

/// The ledger received the validator's answer and then failed inside the record transaction
/// (every record failpoint: after the context, after the record, before commit). Nothing was
/// recorded; the retry on a healthy server reuses the invocation identity and records the
/// original logical result although the validator's environment moved in between.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn a_retry_after_a_crash_between_answer_and_record_reuses_the_invocation() {
    let h = harness().await;
    h.validator.honour_invocation_identity();
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    for (n, point) in [
        FailPoint::AfterLineageValidation, // after the context insert
        FailPoint::AfterDecision,          // after the record insert
        FailPoint::BeforeCommit,           // after the idempotency result
    ]
    .into_iter()
    .enumerate()
    {
        h.validator.set_kb_revision("kbrev-7");
        let c = h
            .prepare(
                &g,
                &t,
                None,
                &format!("<urn:material:{n}> <urn:label> \"X\" ."),
            )
            .await;
        let key = format!("v-crash-{n}");
        let before = h.footprint(&g).await;
        let (calls, logical) = (h.validator.calls(), h.validator.logical());
        let crashing = h.crashing_at(point).await;
        let (status, failed) = crashing.validate(&g, &t, &c, &key, json!({})).await;
        assert!(status.is_server_error(), "{point:?}: {status} {failed}");
        assert_eq!(
            h.validator.logical(),
            logical + 1,
            "{point:?}: validator answered"
        );
        assert_eq!(h.footprint(&g).await, before, "{point:?}: nothing recorded");
        h.validator.set_kb_revision("kbrev-9");
        let (status, v) = h.validate(&g, &t, &c, &key, json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{point:?}: {v}");
        let invocations = h.validator.invocations();
        assert_eq!(invocations.len(), calls + 2, "{point:?}");
        assert_eq!(
            invocations[calls],
            invocations[calls + 1],
            "{point:?}: same identity"
        );
        assert_eq!(
            h.validator.logical(),
            logical + 1,
            "{point:?}: one logical validation"
        );
        assert_eq!(v["context"]["base_kb"]["revision"], "kbrev-7", "{point:?}");
    }
}

/// The same key with different hints is a different invocation; racing them, the request
/// that records first wins and the other is `IDEMPOTENCY_CONFLICT` — never a second result.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn same_key_with_other_hints_is_another_invocation_and_conflicts() {
    let h = harness().await;
    h.validator.honour_invocation_identity();
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let gate = h.validator.hold_next_validation();
    let (calls, logical) = (h.validator.calls(), h.validator.logical());
    let held = h.validate(&g, &t, &c, "v-k", json!({}));
    let other_then_release = async {
        h.validator.wait_for_logical(logical + 1).await;
        let other = h.validate(&g, &t, &c, "v-k", json!({"reasoning_profile": "rdfs"}));
        let check = async {
            h.validator.wait_for_calls(calls + 2).await;
            let invocations = h.validator.invocations();
            assert_ne!(
                invocations[calls],
                invocations[calls + 1],
                "other body, other invocation (checked before the gate opens)"
            );
        };
        let (reply, ()) = tokio::join!(other, check);
        gate.notify_one();
        reply
    };
    let (held, other) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(held, other_then_release)
    })
    .await
    .expect("the race resolves promptly");
    assert_eq!(other.0, StatusCode::CREATED, "{}", other.1);
    assert_code(&held, StatusCode::CONFLICT, "IDEMPOTENCY_CONFLICT");
    let invocations = h.validator.invocations();
    assert_ne!(
        invocations[calls],
        invocations[calls + 1],
        "other body, other invocation"
    );
    assert_eq!(
        h.count(
            "SELECT count(*) FROM idempotency WHERE graph_id = $1 AND idempotency_key = 'v-k'",
            &g
        )
        .await,
        1
    );
    assert_eq!(
        h.count(
            "SELECT count(*) FROM validation_records WHERE graph_id = $1",
            &g
        )
        .await,
        1
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn capabilities_separate_proposing_validating_and_reviewing() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let proposer = token("tenant-v", "agent", &["ledger.read", "ledger.propose"]);
    let validator = token(
        "tenant-v",
        "validator-svc",
        &["ledger.read", "ledger.validate"],
    );
    let reviewer = token("tenant-v", "reviewer", &["ledger.read", "ledger.review"]);
    let c = h
        .prepare(&g, &proposer, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let forbidden = h.validate(&g, &proposer, &c, "v1", json!({})).await;
    assert_code(&forbidden, StatusCode::FORBIDDEN, "FORBIDDEN");
    let forbidden = h.validate(&g, &reviewer, &c, "v1", json!({})).await;
    assert_code(&forbidden, StatusCode::FORBIDDEN, "FORBIDDEN");
    let (status, v) = h.validate(&g, &validator, &c, "v1", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    // Validating grants no power over refs.
    let forbidden = h
        .accept(
            &g,
            &validator,
            &c,
            None,
            "a1",
            Some((&v["validation_id"], &v["semantic_environment_id"])),
        )
        .await;
    assert_code(&forbidden, StatusCode::FORBIDDEN, "FORBIDDEN");
    assert_eq!(h.head(&g).await, None);
    // The validation request records the authenticated requester, not a body claim.
    let requester: String =
        sqlx::query_scalar("SELECT principal_id FROM validation_records WHERE graph_id = $1")
            .bind(g.as_str())
            .fetch_one(h.owner.pool())
            .await
            .unwrap();
    assert!(requester.ends_with("validator-svc"), "{requester}");
    let (status, _) = h
        .accept(
            &g,
            &reviewer,
            &c,
            None,
            "a1",
            Some((&v["validation_id"], &v["semantic_environment_id"])),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn clients_cannot_forge_validation_identity_and_foreign_records_are_invisible() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let other = h.graph("tenant-other").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let foreign = token("tenant-other", "intruder", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let path = format!("/v1/graphs/{g}/proposals/{c}/validations");
    // Unknown fields (validator identity, tenant, principal, recorded_at, a record) are refused.
    for smuggled in [
        json!({"requested": {}, "validator_identity": {"service_id": "x"}}),
        json!({"requested": {}, "tenant_id": "tenant-other"}),
        json!({"requested": {}, "recorded_at": "2020-01-01T00:00:00Z"}),
        json!({"requested": {"validator": {"service_id": "x"}}}),
        json!({"outcome": {"kind": "conforms"}}),
    ] {
        let reply = h
            .call(
                "POST",
                &path,
                &t,
                Some(&unique("k")),
                Some(smuggled.clone()),
            )
            .await;
        assert_code(&reply, StatusCode::BAD_REQUEST, "INVALID_REQUEST");
    }
    let (_, v) = h.validate(&g, &t, &c, "v1", json!({})).await;
    let vid = v["validation_id"].as_str().unwrap().to_owned();
    // A foreign tenant cannot validate or read (the graph is NOT_FOUND, like nonexistence).
    let reply = h.validate(&g, &foreign, &c, "v1", json!({})).await;
    assert_code(&reply, StatusCode::NOT_FOUND, "NOT_FOUND");
    let reply = h
        .call("GET", &format!("{path}/{vid}"), &foreign, None, None)
        .await;
    assert_code(&reply, StatusCode::NOT_FOUND, "NOT_FOUND");
    // Via its own graph, another graph's record is VALIDATION_NOT_FOUND like an unknown id.
    let reply = h
        .call(
            "GET",
            &format!("/v1/graphs/{other}/proposals/{c}/validations/{vid}"),
            &foreign,
            None,
            None,
        )
        .await;
    assert_code(&reply, StatusCode::NOT_FOUND, "VALIDATION_NOT_FOUND");
    let unknown = format!("sha256:{}", "e".repeat(64));
    let reply = h
        .call("GET", &format!("{path}/{unknown}"), &t, None, None)
        .await;
    assert_code(&reply, StatusCode::NOT_FOUND, "VALIDATION_NOT_FOUND");
    let c2 = h
        .prepare(&g, &t, None, "<urn:material:b> <urn:label> \"B\" .")
        .await;
    let reply = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/proposals/{c2}/validations/{vid}"),
            &t,
            None,
            None,
        )
        .await;
    assert_code(&reply, StatusCode::NOT_FOUND, "VALIDATION_NOT_FOUND");
    let malformed = h
        .call("GET", &format!("{path}/not-an-id"), &t, None, None)
        .await;
    assert_code(&malformed, StatusCode::NOT_FOUND, "VALIDATION_NOT_FOUND");
    // A reviewer cannot invent a validation id, nor cite another candidate's record.
    let invented = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a1",
            Some((&json!(unknown), &v["semantic_environment_id"])),
        )
        .await;
    assert_code(&invented, StatusCode::NOT_FOUND, "VALIDATION_NOT_FOUND");
    let (_, v2) = h.validate(&g, &t, &c2, "v-c2", json!({})).await;
    let crossed = h
        .accept(
            &g,
            &t,
            &c,
            None,
            "a2",
            Some((&v2["validation_id"], &v2["semantic_environment_id"])),
        )
        .await;
    assert_code(&crossed, StatusCode::CONFLICT, "LINEAGE_MISMATCH");
    // Half a binding is refused.
    let half = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals/{c}/accept"),
            &t,
            Some("a3"),
            Some(json!({"ref": "main", "validation_id": vid})),
        )
        .await;
    assert_code(&half, StatusCode::BAD_REQUEST, "INVALID_REQUEST");
    assert_eq!(h.head(&g).await, None);
    // Error bodies never echo validator report content or SQL.
    for reply in [&invented, &crossed] {
        let message = reply.1["message"].as_str().unwrap();
        for leak in ["sha256:", "SELECT", "validation_records", "tenant-other"] {
            assert!(!message.contains(leak), "leak {leak:?} in {message}");
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn validation_resource_limits_have_stable_codes() {
    let limits = ApiLimits {
        max_validation_metadata_bytes: 300,
        max_validation_state_bytes: 60,
        max_concurrent_validations: 1,
        ..ApiLimits::default()
    };
    let h = harness_with(limits, true).await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h.prepare(&g, &t, None, "<urn:m:a> <urn:p> \"1\" .").await;
    let long = "x".repeat(400);
    let reply = h
        .validate(&g, &t, &c, "v-meta", json!({"reasoning_profile": long}))
        .await;
    assert_code(&reply, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
    // A candidate state larger than what may be shipped to the validator.
    let big = h
        .prepare(
            &g,
            &t,
            None,
            &format!("<urn:m:b> <urn:p> \"{}\" .", "y".repeat(80)),
        )
        .await;
    let reply = h.validate(&g, &t, &big, "v-big", json!({})).await;
    assert_code(&reply, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
    assert_eq!(
        h.validator.calls(),
        0,
        "limits are enforced before any outbound call"
    );
    // The validation budget: while one call is in flight, another is refused immediately.
    let gate = Arc::new(tokio::sync::Notify::new());
    h.validator.set(Mode::Blocked(gate.clone()));
    let app = h.app.clone();
    let path = format!("/v1/graphs/{g}/proposals/{c}/validations");
    let t2 = t.clone();
    let in_flight = tokio::spawn(async move {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {t2}"))
            .header("idempotency-key", "v-slow")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        app.oneshot(request).await.unwrap().status()
    });
    for _ in 0..200 {
        if h.validator.calls() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        h.validator.calls(),
        1,
        "the first validation reached the validator"
    );
    let c2 = h.prepare(&g, &t, None, "<urn:m:c> <urn:p> \"2\" .").await;
    let busy = h.validate(&g, &t, &c2, "v-busy", json!({})).await;
    assert_code(&busy, StatusCode::SERVICE_UNAVAILABLE, "RESOURCE_LIMIT");
    gate.notify_one();
    assert_eq!(in_flight.await.unwrap(), StatusCode::CREATED);
    h.validator.set(Mode::Normal);
    let (status, _) = h.validate(&g, &t, &c2, "v-busy", json!({})).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the budget is released after the call"
    );
}

/// The environment an orchestrator names is computable from what Sculpin declares current,
/// before any candidate or record exists; candidates hydrating different external datasets
/// under the same declared environment are accepted with that one id (review round 2).
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn one_declared_environment_accepts_candidates_that_hydrate_different_sources() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let declared = SemanticEnvironment {
        base_kb: BaseKb {
            kb_id: "urn:exodus:kb:material-science".into(),
            revision: "kbrev-7".into(),
        },
        ontology: Some(Ontology {
            id: "urn:sculpin:ontology:core".into(),
            version: "O1".into(),
        }),
        shapes: ShapeSet {
            id: "urn:sculpin:shapes:material".into(),
            version: "S1".into(),
        },
        reasoning: None,
        sources_revision: Some("catalog-D-A".into()),
        validator_service_version: "2026.09.1".into(),
        validator_configuration_version: "cfg-1".into(),
    };
    let declared_id = json!(declared.id().unwrap().to_string());
    let c1 = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let (_, v1) = h.validate(&g, &t, &c1, "v1", json!({})).await;
    assert_eq!(v1["semantic_environment_id"], declared_id, "{v1}");
    let (status, _) = h
        .accept(
            &g,
            &t,
            &c1,
            None,
            "a1",
            Some((&v1["validation_id"], &declared_id)),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let c2 = h
        .prepare(
            &g,
            &t,
            Some(&c1),
            "<urn:material:reference-data> <urn:label> \"R\" .",
        )
        .await;
    let (_, v2) = h.validate(&g, &t, &c2, "v2", json!({})).await;
    assert_ne!(
        v2["context"]["virtual_contexts"][0]["dataset_id"],
        v1["context"]["virtual_contexts"][0]["dataset_id"],
        "the two runs hydrated different sources"
    );
    assert_ne!(v2["semantic_context_id"], v1["semantic_context_id"]);
    assert_eq!(v2["semantic_environment_id"], declared_id);
    let (status, a2) = h
        .accept(
            &g,
            &t,
            &c2,
            Some(&c1),
            "a2",
            Some((&v2["validation_id"], &declared_id)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{a2}");
    assert_eq!(h.head(&g).await, Some((c2, 2)));
}

/// Review round 3: a validator that honours a hint by silently returning another catalog
/// revision is refused, and a source-version pin cannot be requested at all (it could report
/// the current revision and alias the current environment).
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn pins_are_not_hints_and_an_ignored_revision_hint_is_a_validator_error() {
    let h = harness().await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h
        .prepare(&g, &t, None, "<urn:material:a> <urn:label> \"A\" .")
        .await;
    let pinned = h
        .validate(
            &g,
            &t,
            &c,
            "v-pin",
            json!({"source_pins": [{"dataset_id": "urn:sculpin:datasource:lab", "source_version": "v40"}]}),
        )
        .await;
    assert_code(&pinned, StatusCode::BAD_REQUEST, "INVALID_REQUEST");
    h.validator.set(Mode::WrongRevision);
    let ignored = h
        .validate(
            &g,
            &t,
            &c,
            "v-rev",
            json!({"sources_revision": "catalog-D-B"}),
        )
        .await;
    assert_code(&ignored, StatusCode::BAD_GATEWAY, "VALIDATOR_ERROR");
    let records = h
        .count(
            "SELECT count(*) FROM validation_records WHERE graph_id = $1",
            &g,
        )
        .await;
    assert_eq!(records, 0);
}

// ---- Phase 4: branches over HTTP (ADR-0022; Plan 0008) -----------------------------------

const ADMIN: [&str; 5] = [
    "ledger.read",
    "ledger.propose",
    "ledger.validate",
    "ledger.review",
    "ledger.admin",
];

impl Harness {
    async fn prepare_on(
        &self,
        g: &GraphId,
        t: &str,
        branch: &str,
        head: &str,
        quad: &str,
    ) -> Reply {
        self.call(
            "POST",
            &format!("/v1/graphs/{g}/proposals"),
            t,
            Some(&unique("p")),
            Some(json!({
                "ref": branch, "expected_head": head,
                "operations": [{"op": "add", "quad": quad}],
                "activity": "cognitive-correction", "message": "m",
            })),
        )
        .await
    }

    /// prepare → validate → accept on `branch` from `head`; returns the new head.
    async fn step_on(&self, g: &GraphId, t: &str, branch: &str, head: &str, quad: &str) -> String {
        self.step_adding(g, t, branch, head, &[quad]).await
    }

    /// [`Self::step_on`] adding several quads in one commit.
    async fn step_adding(
        &self,
        g: &GraphId,
        t: &str,
        branch: &str,
        head: &str,
        quads: &[&str],
    ) -> String {
        let operations: Vec<Value> = quads
            .iter()
            .map(|q| json!({"op": "add", "quad": q}))
            .collect();
        let (status, p) = self
            .call(
                "POST",
                &format!("/v1/graphs/{g}/proposals"),
                t,
                Some(&unique("p")),
                Some(json!({
                    "ref": branch, "expected_head": head, "operations": operations,
                    "activity": "cognitive-correction", "message": "m",
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{p}");
        let c = p["candidate"].as_str().unwrap().to_owned();
        let (status, v) = self.validate(g, t, &c, &unique("v"), json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        let body = json!({
            "ref": branch, "expected_head": head, "reason": "reviewed",
            "validation_id": v["validation_id"], "semantic_environment_id": v["semantic_environment_id"],
        });
        let (status, a) = self
            .call(
                "POST",
                &format!("/v1/graphs/{g}/proposals/{c}/accept"),
                t,
                Some(&unique("a")),
                Some(body),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{a}");
        c
    }

    /// Validated genesis + (n-1) advances on main; returns the heads in order.
    async fn main_history(&self, g: &GraphId, t: &str, n: usize) -> Vec<String> {
        let first = self
            .prepare(g, t, None, &format!("<urn:m:{g}:1> <urn:p> \"1\" ."))
            .await;
        let (_, v) = self.validate(g, t, &first, &unique("v"), json!({})).await;
        let (status, a) = self
            .accept(
                g,
                t,
                &first,
                None,
                &unique("a"),
                Some((&v["validation_id"], &v["semantic_environment_id"])),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{a}");
        let mut heads = vec![first];
        for i in 2..=n {
            let head = heads.last().unwrap().clone();
            heads.push(
                self.step_on(
                    g,
                    t,
                    "main",
                    &head,
                    &format!("<urn:m:{g}:{i}> <urn:p> \"{i}\" ."),
                )
                .await,
            );
        }
        heads
    }

    async fn create_branch(&self, g: &GraphId, t: &str, key: &str, body: Value) -> Reply {
        self.call(
            "POST",
            &format!("/v1/graphs/{g}/branches"),
            t,
            Some(key),
            Some(body),
        )
        .await
    }

    async fn lifecycle(&self, g: &GraphId, t: &str, op: &str, key: &str, name: &str) -> Reply {
        self.call(
            "POST",
            &format!("/v1/graphs/{g}/branches/{op}"),
            t,
            Some(key),
            Some(json!({"name": name, "reason": format!("{op} by test")})),
        )
        .await
    }

    async fn branch_status(&self, g: &GraphId, t: &str, name: &str) -> Reply {
        self.call(
            "GET",
            &format!("/v1/graphs/{g}/branches/status?name={name}"),
            t,
            None,
            None,
        )
        .await
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn a_validated_multi_step_cognitive_workflow_runs_on_a_branch_while_main_stays_put() {
    let h = harness().await;
    let g = h.graph("tenant-br").await;
    let admin = token("tenant-br", "operator", &ADMIN);
    let agent = token("tenant-br", "agent-17", &ROLES);
    let c100 = h.main_history(&g, &admin, 1).await.remove(0);
    // The agent creates its workspace from main's head: O(1), nothing copied.
    let (status, created) = h
        .create_branch(
            &g,
            &agent,
            "t17",
            json!({"name": "agent/task-17", "source": "main"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["event"]["head"], c100);
    assert_eq!(created["event"]["version"], 1);
    assert_eq!(created["event"]["operation"], "created");
    // Three proposal → validation → acceptance cycles on the branch.
    let mut head = c100.clone();
    let mut heads = Vec::new();
    for step in 101..=103 {
        head = h
            .step_on(
                &g,
                &agent,
                "agent/task-17",
                &head,
                &format!("<urn:task17:c{step}> <urn:p> \"{step}\" ."),
            )
            .await;
        heads.push(head.clone());
    }
    assert_eq!(
        h.head(&g).await,
        Some((c100.clone(), 1)),
        "main never moved"
    );
    let (status, b) = h.branch_status(&g, &agent, "agent/task-17").await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(
        (b["head"].as_str().unwrap(), b["version"].as_i64().unwrap()),
        (heads[2].as_str(), 4)
    );
    assert_eq!(
        (
            b["origin"].as_str(),
            b["source"].as_str(),
            b["source_commit"].as_str()
        ),
        (Some("created"), Some("main"), Some(c100.as_str()))
    );
    // Deterministic historical reconstruction at C101, C102, C103.
    for (i, c) in heads.iter().enumerate() {
        let path = format!("/v1/graphs/{g}/commits/{c}/state");
        let (s1, a) = h.call("GET", &path, &agent, None, None).await;
        let (s2, b) = h.call("GET", &path, &agent, None, None).await;
        assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
        assert_eq!(a, b);
        assert_eq!(a["quads"].as_array().unwrap().len(), i + 2);
    }
    // History: lifecycle and movements are distinct surfaces.
    let (status, history) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/branches/history?name=agent/task-17"),
            &agent,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history["lifecycle"].as_array().unwrap().len(), 1);
    assert_eq!(
        history["lifecycle"][0]["principal_id"],
        created["event"]["principal_id"]
    );
    let versions: Vec<i64> = history["movements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["new_version"].as_i64().unwrap())
        .collect();
    assert_eq!(versions, vec![4, 3, 2, 1]);
    let (status, log) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/branches/log?name=agent%2Ftask-17&limit=10"),
            &agent,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{log}");
    assert_eq!(log["commits"], json!([heads[2], heads[1], heads[0], c100]));
    // Branch acceptance writes the ordinary atomic records, and no projection alarm: its
    // outbox rows exist but projection observability counts only `main`.
    let outbox_branch = h
        .count("SELECT count(*) FROM projection_outbox WHERE graph_id = $1 AND branch = 'agent/task-17'", &g)
        .await;
    assert_eq!(outbox_branch, 3);
    // Both global readings in one snapshot: other tests write outbox rows concurrently.
    let mut snapshot = h.owner.pool().begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *snapshot)
        .await
        .unwrap();
    let unconfigured = ledger_store::ProjectionRepository::unconfigured_pending_on(&mut snapshot)
        .await
        .unwrap();
    let main_pending = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM projection_outbox o WHERE o.branch = 'main' AND o.delivered_at IS NULL \
         AND NOT EXISTS (SELECT 1 FROM projection_state s WHERE s.graph_id = o.graph_id AND s.branch = o.branch \
                         AND s.status <> 'disabled')",
    )
    .fetch_one(&mut *snapshot)
    .await
    .unwrap();
    snapshot.rollback().await.unwrap();
    assert_eq!(
        unconfigured, main_pending,
        "non-main branch traffic is not a projection backlog"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn branch_authority_follows_capabilities_and_foreign_tenants_see_nothing() {
    let h = harness().await;
    let g = h.graph("tenant-bz").await;
    let admin = token("tenant-bz", "operator", &ADMIN);
    let agent = token("tenant-bz", "agent", &ROLES);
    let reader = token("tenant-bz", "reader", &["ledger.read"]);
    let reviewer = token("tenant-bz", "reviewer", &["ledger.read", "ledger.review"]);
    let foreign = token("tenant-other", "intruder", &ADMIN);
    h.main_history(&g, &admin, 1).await;
    let plain = json!({"name": "agent/x", "source": "main"});
    assert_code(
        &h.create_branch(&g, &reader, "r1", plain.clone()).await,
        StatusCode::FORBIDDEN,
        "FORBIDDEN",
    );
    let (status, _) = h.create_branch(&g, &agent, "a1", plain.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    // Protected branches are administrative.
    let protected = json!({"name": "release/1", "source": "main", "policy": {"protected": true}});
    assert_code(
        &h.create_branch(&g, &agent, "a2", protected.clone()).await,
        StatusCode::FORBIDDEN,
        "FORBIDDEN",
    );
    let (status, _) = h.create_branch(&g, &admin, "ad1", protected).await;
    assert_eq!(status, StatusCode::CREATED);
    // Delete / restore are administrative.
    for t in [&agent, &reviewer, &reader] {
        assert_code(
            &h.lifecycle(&g, t, "delete", &unique("d"), "agent/x").await,
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
        );
    }
    let (status, _) = h.lifecycle(&g, &admin, "delete", "del-x", "agent/x").await;
    assert_eq!(status, StatusCode::OK);
    // main is never deleted, not even by an administrator.
    assert_code(
        &h.lifecycle(&g, &admin, "delete", "del-main", "main").await,
        StatusCode::CONFLICT,
        "BRANCH_POLICY_VIOLATION",
    );
    // Readers see status; foreign tenants see nothing, not even existence.
    let (status, list) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/branches"),
            &reader,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = list["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["agent/x", "main", "release/1"]);
    for path in [
        format!("/v1/graphs/{g}/branches"),
        format!("/v1/graphs/{g}/branches/status?name=main"),
        format!("/v1/graphs/{g}/branches/history?name=main"),
    ] {
        assert_code(
            &h.call("GET", &path, &foreign, None, None).await,
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
        );
    }
    assert_code(
        &h.create_branch(
            &g,
            &foreign,
            "fx",
            json!({"name": "stolen", "source": "main"}),
        )
        .await,
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
    );
    // A slash in a name is never path structure: no such route exists.
    let (status, _) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/branches/agent/x"),
            &reader,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Percent-encoded and raw query names address the same branch.
    let (s1, a) = h.branch_status(&g, &reader, "agent/x").await;
    let (s2, b) = h.branch_status(&g, &reader, "agent%2Fx").await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    assert_eq!(a, b);
    assert_eq!(a["status"], "deleted");
    assert_code(
        &h.branch_status(&g, &reader, "agent/none").await,
        StatusCode::NOT_FOUND,
        "BRANCH_NOT_FOUND",
    );
    // Unknown fields are refused (no hidden override flags).
    assert_code(
        &h.create_branch(
            &g,
            &admin,
            "ov",
            json!({"name": "y", "source": "main", "force": true}),
        )
        .await,
        StatusCode::BAD_REQUEST,
        "INVALID_REQUEST",
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn historical_branch_points_must_be_reachable_and_failures_disclose_nothing() {
    let h = harness().await;
    let g = h.graph("tenant-bh").await;
    let admin = token("tenant-bh", "operator", &ADMIN);
    let c = h.main_history(&g, &admin, 5).await;
    let (status, a) = h
        .create_branch(
            &g,
            &admin,
            "ba",
            json!({"name": "branch-A", "source": "main"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{a}");
    assert_eq!(a["event"]["head"], c[4]);
    let (status, b) = h
        .create_branch(
            &g,
            &admin,
            "bb",
            json!({"name": "branch-B", "source": "main", "from_commit": c[1]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    assert_eq!(b["event"]["head"], c[1]);
    // Unreachable (a commit only on branch-A beyond C5, from branch-B), foreign-graph and
    // unknown commits: the same status, code and message.
    let beyond = h
        .step_on(
            &g,
            &admin,
            "branch-A",
            &c[4],
            "<urn:beyond> <urn:p> \"x\" .",
        )
        .await;
    let other = h.graph("tenant-other-bh").await;
    let other_admin = token("tenant-other-bh", "operator", &ADMIN);
    let foreign_commit = h.main_history(&other, &other_admin, 1).await.remove(0);
    let unknown = format!("sha256:{}", "e".repeat(64));
    let mut replies = Vec::new();
    for (i, point) in [beyond, foreign_commit, unknown].iter().enumerate() {
        let reply = h
            .create_branch(
                &g,
                &admin,
                &format!("bad{i}"),
                json!({"name": format!("bad-{i}"), "source": "branch-B", "from_commit": point}),
            )
            .await;
        assert_code(
            &reply,
            StatusCode::UNPROCESSABLE_ENTITY,
            "BRANCH_POINT_UNREACHABLE",
        );
        replies.push(reply.1["message"].clone());
    }
    assert!(replies.windows(2).all(|w| w[0] == w[1]), "{replies:?}");
    assert_code(
        &h.branch_status(&g, &admin, "bad-0").await,
        StatusCode::NOT_FOUND,
        "BRANCH_NOT_FOUND",
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn deleted_branches_are_readable_but_frozen_and_lifecycle_retries_replay() {
    let h = harness().await;
    let g = h.graph("tenant-bd").await;
    let admin = token("tenant-bd", "operator", &ADMIN);
    let c = h.main_history(&g, &admin, 1).await;
    let body = json!({"name": "work/1", "source": "main"});
    let (status, first) = h.create_branch(&g, &admin, "cw", body.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    // Create retry: the original result; another request under the key: conflict.
    let (status, again) = h.create_branch(&g, &admin, "cw", body).await;
    assert_eq!(
        (status, again["replayed"].clone()),
        (StatusCode::OK, json!(true))
    );
    assert_eq!(again["event"], first["event"]);
    assert_code(
        &h.create_branch(
            &g,
            &admin,
            "cw",
            json!({"name": "work/2", "source": "main"}),
        )
        .await,
        StatusCode::CONFLICT,
        "IDEMPOTENCY_CONFLICT",
    );
    let h1 = h
        .step_on(&g, &admin, "work/1", &c[0], "<urn:w:1> <urn:p> \"1\" .")
        .await;
    let (status, deleted) = h.lifecycle(&g, &admin, "delete", "dw", "work/1").await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let (status, replay) = h.lifecycle(&g, &admin, "delete", "dw", "work/1").await;
    assert_eq!(
        (status, replay["replayed"].clone()),
        (StatusCode::OK, json!(true))
    );
    assert_eq!(replay["event"], deleted["event"]);
    assert_code(
        &h.lifecycle(&g, &admin, "delete", "dw2", "work/1").await,
        StatusCode::CONFLICT,
        "BRANCH_STATE_CONFLICT",
    );
    // History and state still readable; prepare and accept refused.
    let (status, _) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/commits/{h1}/state"),
            &admin,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_code(
        &h.prepare_on(&g, &admin, "work/1", &h1, "<urn:w:2> <urn:p> \"2\" .")
            .await,
        StatusCode::CONFLICT,
        "BRANCH_DELETED",
    );
    // Restore: same head and version; the workflow resumes.
    let (status, restored) = h.lifecycle(&g, &admin, "restore", "rw", "work/1").await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert_eq!(
        (
            restored["event"]["head"].as_str(),
            restored["event"]["version"].as_i64()
        ),
        (Some(h1.as_str()), Some(2))
    );
    let (_, replay) = h.lifecycle(&g, &admin, "restore", "rw", "work/1").await;
    assert_eq!(replay["replayed"], true);
    let h2 = h
        .step_on(&g, &admin, "work/1", &h1, "<urn:w:2> <urn:p> \"2\" .")
        .await;
    let (_, b) = h.branch_status(&g, &admin, "work/1").await;
    assert_eq!(
        (
            b["head"].as_str(),
            b["version"].as_i64(),
            b["status"].as_str()
        ),
        (Some(h2.as_str()), Some(3), Some("active"))
    );
    let (_, history) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/branches/history?name=work/1"),
            &admin,
            None,
            None,
        )
        .await;
    let ops: Vec<&str> = history["lifecycle"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["operation"].as_str().unwrap())
        .collect();
    assert_eq!(ops, vec!["created", "deleted", "restored"]);
}

/// Phase 5 over HTTP (ADR-0023/0024): read-only preview, propose, and apply onto `main`
/// bound to a validation of the merged candidate. The merged state's acceptability depends
/// on the external (Virtual A-Box) source version: validated under D-A it conforms, under
/// D-B it does not; an apply naming another environment than its validation's is stale.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn a_validated_merge_onto_main_binds_the_merged_state_in_its_semantic_environment() {
    let h = harness().await;
    let g = h.graph("tenant-mg").await;
    let admin = token("tenant-mg", "operator", &ADMIN);
    let agent = token("tenant-mg", "agent-7", &ROLES);
    let reviewer = token("tenant-mg", "reviewer-1", &ROLES);
    let reader = token("tenant-mg", "reader", &["ledger.read"]);
    let c100 = h.main_history(&g, &admin, 1).await.remove(0);
    let (status, created) = h
        .create_branch(
            &g,
            &agent,
            "mg-b",
            json!({"name": "agent/m", "source": "main"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    // Divergence: the branch adds A-Box-dependent material, main adds something else.
    let s1 = h
        .step_on(
            &g,
            &agent,
            "agent/m",
            &c100,
            "<urn:material:abox-dependent> <urn:p> \"m\" .",
        )
        .await;
    let m1 = h
        .step_on(
            &g,
            &admin,
            "main",
            &c100,
            "<urn:main:other> <urn:p> \"o\" .",
        )
        .await;
    let body = json!({"source": "agent/m", "target": "main"});
    // Preview is a read: the reader may preview, nothing is written.
    let (status, p) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/merges/preview"),
            &reader,
            None,
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{p}");
    assert_eq!(p["classification"], "divergent");
    assert_eq!(p["merge_base"], c100);
    assert_eq!(
        (p["ahead"].as_i64(), p["behind"].as_i64()),
        (Some(1), Some(1))
    );
    let token_v = p["preview_token"].as_str().unwrap().to_owned();
    // Propose needs the propose capability.
    let mut propose = body.clone();
    propose["preview_token"] = json!(token_v);
    let (status, _) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/merges/propose"),
            &reader,
            Some("mg-p0"),
            Some(propose.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, pr) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/merges/propose"),
            &agent,
            Some("mg-p1"),
            Some(propose.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{pr}");
    let candidate = pr["candidate"].as_str().unwrap().to_owned();
    assert_eq!(pr["merged_state_digest"], p["merged_state_digest"]);
    assert_eq!(
        h.head(&g).await,
        Some((m1.clone(), 2)),
        "propose moves nothing"
    );
    // Validate the merged candidate under two external versions.
    let pin = |v: &str| json!({"sources_revision": format!("catalog-{v}")});
    let (sa, va) = h
        .validate(&g, &agent, &candidate, "mg-va", pin("D-A"))
        .await;
    let (sb, vb) = h
        .validate(&g, &agent, &candidate, "mg-vb", pin("D-B"))
        .await;
    assert_eq!(
        (sa, sb),
        (StatusCode::CREATED, StatusCode::CREATED),
        "{va} {vb}"
    );
    assert_eq!(
        (va["conforms"].as_bool(), vb["conforms"].as_bool()),
        (Some(true), Some(false))
    );
    let apply = |validation: &Value, environment: &Value| {
        json!({
            "proposal_id": pr["proposal_id"], "preview_token": token_v,
            "validation_id": validation["validation_id"],
            "semantic_environment_id": environment["semantic_environment_id"],
            "reason": "merge reviewed",
        })
    };
    let path = format!("/v1/graphs/{g}/merges/apply");
    // Non-conforming under D-B: rejected.
    let (status, e) = h
        .call(
            "POST",
            &path,
            &reviewer,
            Some("mg-a1"),
            Some(apply(&vb, &vb)),
        )
        .await;
    assert_eq!(
        (status, e["code"].as_str()),
        (StatusCode::CONFLICT, Some("VALIDATION_REJECTED")),
        "{e}"
    );
    // The D-A validation cited in the D-B environment: stale (the A-Box version changed).
    let (status, e) = h
        .call(
            "POST",
            &path,
            &reviewer,
            Some("mg-a2"),
            Some(apply(&va, &vb)),
        )
        .await;
    assert_eq!(e["code"].as_str(), Some("VALIDATION_STALE"), "{status} {e}");
    // The deployment floor (RequireValidation): no validation cited → refused.
    let (status, e) = h
        .call(
            "POST",
            &path,
            &reviewer,
            Some("mg-a-none"),
            Some(json!({"proposal_id": pr["proposal_id"], "preview_token": token_v})),
        )
        .await;
    assert_eq!(
        (status, e["code"].as_str()),
        (StatusCode::CONFLICT, Some("VALIDATION_REQUIRED")),
        "{e}"
    );
    // A validation of another candidate (the source tip) never validates the merge.
    let (_, vs) = h.validate(&g, &agent, &s1, "mg-vs", pin("D-A")).await;
    let (status, e) = h
        .call(
            "POST",
            &path,
            &reviewer,
            Some("mg-a-src"),
            Some(apply(&vs, &vs)),
        )
        .await;
    assert_eq!(
        (status, e["code"].as_str()),
        (StatusCode::CONFLICT, Some("LINEAGE_MISMATCH")),
        "{e}"
    );
    // Apply needs review; another tenant sees no graph.
    let proposer_only = token("tenant-mg", "p-only", &["ledger.read", "ledger.propose"]);
    let (status, _) = h
        .call(
            "POST",
            &path,
            &proposer_only,
            Some("mg-a-po"),
            Some(apply(&va, &va)),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let other = token("tenant-other", "x", &ROLES);
    let (status, _) = h
        .call(
            "POST",
            &path,
            &other,
            Some("mg-a-ot"),
            Some(apply(&va, &va)),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Conforming in its own environment: applied onto main.
    let (status, a) = h
        .call(
            "POST",
            &path,
            &reviewer,
            Some("mg-a3"),
            Some(apply(&va, &va)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(a["head"], candidate);
    assert_eq!(h.head(&g).await, Some((candidate.clone(), 3)));
    let (_, again) = h
        .call(
            "POST",
            &path,
            &reviewer,
            Some("mg-a3"),
            Some(apply(&va, &va)),
        )
        .await;
    assert_eq!(again["replayed"], true);
    // The merged state holds both sides.
    let (status, st) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/commits/{candidate}/state"),
            &agent,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let quads: Vec<&str> = st["quads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q.as_str().unwrap())
        .collect();
    assert!(quads.contains(&"<urn:material:abox-dependent> <urn:p> \"m\" ."));
    assert!(quads.contains(&"<urn:main:other> <urn:p> \"o\" ."));
    // Repeating the merge: contained. Another tenant sees nothing.
    let (_, p2) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/merges/preview"),
            &reader,
            None,
            Some(body.clone()),
        )
        .await;
    assert_eq!(p2["classification"], "already_contained");
    let foreign = token("tenant-other", "x", &ROLES);
    let (status, _) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/merges/preview"),
            &foreign,
            None,
            Some(body),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// ADR-0024 conflict report byte budget: large legal terms in conflicting slots. Under a tiny
/// budget the preview lists a deterministic prefix within the budget and says so; the
/// classification, exact conflict count, merged-state digest and preview token are those of a
/// server with the default budget, and a token previewed under the tiny budget proposes.
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn the_conflict_report_has_a_byte_budget_that_never_changes_the_merge() {
    const TINY: usize = 4 * 1024;
    let big_terms = ApiLimits {
        max_term_bytes: 64 * 1024,
        ..ApiLimits::default()
    };
    let h = harness_with(
        ApiLimits {
            max_merge_conflict_report_bytes: TINY,
            ..big_terms
        },
        true,
    )
    .await;
    let roomy = Harness {
        owner: h.owner.clone(),
        app: app_for(
            &h.runtime_url,
            big_terms,
            &h.validator,
            Some(SERVICE_ID),
            Some(SERVICE_ID),
        )
        .await,
        validator: h.validator.clone(),
        runtime_url: h.runtime_url.clone(),
        limits: big_terms,
    };
    let g = h.graph("tenant-cr").await;
    let admin = token("tenant-cr", "operator", &ADMIN);
    let agent = token("tenant-cr", "agent-3", &ROLES);
    let reader = token("tenant-cr", "reader", &["ledger.read"]);
    let c1 = h.main_history(&g, &admin, 1).await.remove(0);
    let (status, created) = h
        .create_branch(
            &g,
            &agent,
            "cr-b",
            json!({"name": "agent/cr", "source": "main"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    // Six slots both sides fill differently, each with a ~16 KiB literal full of characters
    // that N-Quads and then JSON escape again.
    const SLOTS: usize = 6;
    let heavy = |side: &str, i: usize| {
        let mut lit = format!("{side}{i}-");
        while lit.len() < 16 * 1024 {
            lit.push_str("q\\\"b\\nc\\\\é");
        }
        format!("<urn:cr:slot:{i}> <urn:cr:p> \"{lit}\" .")
    };
    let target_quads: Vec<String> = (0..SLOTS).map(|i| heavy("t", i)).collect();
    let source_quads: Vec<String> = (0..SLOTS).map(|i| heavy("s", i)).collect();
    fn refs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }
    h.step_adding(&g, &admin, "main", &c1, &refs(&target_quads))
        .await;
    h.step_adding(&g, &agent, "agent/cr", &c1, &refs(&source_quads))
        .await;

    async fn preview(server: &Harness, g: &GraphId, reader: &str, strategy: &str) -> Value {
        let body = json!({"source": "agent/cr", "target": "main", "strategy": strategy});
        let (status, p) = server
            .call(
                "POST",
                &format!("/v1/graphs/{g}/merges/preview"),
                reader,
                None,
                Some(body),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{p}");
        p
    }
    for strategy in ["abort", "union", "take-source"] {
        let tiny = preview(&h, &g, &reader, strategy).await;
        let full = preview(&roomy, &g, &reader, strategy).await;
        // The merge is the same; only the report differs.
        for field in [
            "classification",
            "conflict_count",
            "merged_state_digest",
            "preview_token",
            "merge_base",
            "target_delta",
            "source_delta",
        ] {
            assert_eq!(tiny[field], full[field], "{strategy}: {field}");
        }
        assert_eq!(tiny["conflict_count"], SLOTS);
        // The default budget holds the whole report; the tiny one does not.
        assert_eq!(full["conflicts_truncated"], false);
        assert_eq!(full["conflicts"].as_array().unwrap().len(), SLOTS);
        assert_eq!(tiny["conflicts_truncated"], true);
        let details = serde_json::to_string(&tiny["conflicts"]).unwrap();
        assert!(
            details.len() <= TINY + 2,
            "{strategy}: {} bytes of detail over a {TINY}-byte budget",
            details.len()
        );
        // Whole quads only, each a quad of its side in the full report; a deterministic
        // prefix of the full report.
        let listed = tiny["conflicts"].as_array().unwrap();
        assert!(!listed.is_empty(), "the slot key fits the tiny budget");
        for (i, c) in listed.iter().enumerate() {
            let f = &full["conflicts"][i];
            assert_eq!(
                (&c["subject"], &c["predicate"]),
                (&f["subject"], &f["predicate"])
            );
            for side in ["base", "target", "source"] {
                let (cq, fq) = (
                    c[side]["quads"].as_array().unwrap(),
                    f[side]["quads"].as_array().unwrap(),
                );
                assert!(fq.starts_with(cq), "{strategy}: {side} is a prefix");
                assert_eq!(c[side]["truncated"], cq.len() < fq.len());
            }
        }
        // The same inputs report exactly the same prefix.
        assert_eq!(
            preview(&h, &g, &reader, strategy).await["conflicts"],
            tiny["conflicts"]
        );
        if strategy == "abort" {
            assert_eq!(tiny["classification"], "conflicted");
            assert!(tiny.get("preview_token").is_none());
        }
    }
    // A token previewed under the tiny budget is the merge's token: it proposes.
    let tiny = preview(&h, &g, &reader, "union").await;
    let (status, pr) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/merges/propose"),
            &agent,
            Some("cr-propose"),
            Some(json!({
                "source": "agent/cr", "target": "main", "strategy": "union",
                "preview_token": tiny["preview_token"],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{pr}");
    assert_eq!(pr["merged_state_digest"], tiny["merged_state_digest"]);
    assert_eq!(pr["conflict_count"], SLOTS);
}
