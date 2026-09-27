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
use ledger_store::{DbSessionLimits, GraphStatus, NewGraph, PostgresLedgerStore, V1Binding};
use ledger_validation_protocol::{
    BaseKb, EffectiveContext, Ontology, OutcomeKind, Reasoning, ReportReference,
    SemanticExecutionContext, ShapeSet, VALIDATION_RESPONSE_PROTOCOL, ValidationClient,
    ValidationClientError, ValidationOutcome, ValidationRequest, ValidatorResponse,
    ValidatorVersions, ViolationSummary, VirtualContextRef,
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
    Slow(Duration),
    /// Wait until the notify fires (to saturate the validation budget).
    Blocked(Arc<tokio::sync::Notify>),
}

struct FakeValidator {
    mode: Mutex<Mode>,
    calls: AtomicUsize,
}

impl FakeValidator {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            mode: Mutex::new(Mode::Normal),
            calls: AtomicUsize::new(0),
        })
    }
    fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mode = self.mode.lock().unwrap().clone();
        match mode {
            Mode::Normal | Mode::WrongCandidate => {}
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
        let external = hints
            .source_pins
            .first()
            .map_or("D-A".to_owned(), |p| p.source_version.clone());
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
                    revision: "kbrev-7".into(),
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
                virtual_contexts: vec![VirtualContextRef {
                    dataset_id: "urn:sculpin:datasource:lab".into(),
                    source_version: external,
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
    fn describe(&self) -> String {
        "deterministic fake validator".into()
    }
}

// ---------------------------------------------------------------------------------------
// Harness (runtime identity behind the router, owner for provisioning and assertions)

struct Harness {
    owner: PostgresLedgerStore,
    app: Router,
    validator: Arc<FakeValidator>,
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
    let store = PostgresLedgerStore::connect_with(
        &runtime_url,
        V1Binding::Reject,
        DbSessionLimits::default(),
    )
    .await
    .expect("runtime identity verify-only start-up");
    let auth: SharedAuthenticator = Arc::new(
        DevHs256Authenticator::new(
            ISSUER.into(),
            AUDIENCE.into(),
            SECRET,
            ClaimsPolicy::default(),
        )
        .unwrap(),
    );
    let validator = FakeValidator::new();
    let mut state = AppState::new(store, auth, limits, AcceptancePolicy::RequireValidation);
    if with_validator {
        state = state.with_validation(ValidationService {
            client: validator.clone(),
            service_id: SERVICE_ID.into(),
        });
    }
    Harness {
        owner,
        app: ledger_api::router(state),
        validator,
    }
}

async fn harness() -> Harness {
    harness_with(ApiLimits::default(), true).await
}

impl Harness {
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
    let pin = |v: &str| json!({"source_pins": [{"dataset_id": "urn:sculpin:datasource:lab", "source_version": v}]});
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
        max_virtual_contexts: 2,
        max_validation_metadata_bytes: 300,
        max_validation_state_bytes: 60,
        max_concurrent_validations: 1,
        ..ApiLimits::default()
    };
    let h = harness_with(limits, true).await;
    let g = h.graph("tenant-v").await;
    let t = token("tenant-v", "orchestrator", &ROLES);
    let c = h.prepare(&g, &t, None, "<urn:m:a> <urn:p> \"1\" .").await;
    let pins: Vec<Value> = (0..3)
        .map(|i| json!({"dataset_id": format!("ds-{i}"), "source_version": "1"}))
        .collect();
    let reply = h
        .validate(&g, &t, &c, "v-pins", json!({"source_pins": pins}))
        .await;
    assert_code(&reply, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
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
