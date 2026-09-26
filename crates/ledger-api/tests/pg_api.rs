//! End-to-end HTTP tests over a real PostgreSQL database: authentication, authorization,
//! tenant isolation, idempotent replay (lost responses), fail-closed acceptance, resource
//! limits and the safe error envelope. Every test is `#[ignore]` and runs through
//! `scripts/test-integration.sh` with `LEDGER_TEST_DATABASE_URL`.

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use ledger_api::{
    AcceptancePolicy, ApiLimits, AppState,
    auth::{ClaimsPolicy, DevHs256Authenticator},
};
use ledger_core::{GraphId, TenantId};
use ledger_store::{GraphStatus, NewGraph, PostgresLedgerStore, ReconstructionLimits, V1Binding};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tower::ServiceExt;

const ISSUER: &str = "https://dev-issuer.example/";
const AUDIENCE: &str = "api://sculpin-ledger-test";
const SECRET: &[u8] = b"development-only-secret-that-is-at-least-32-bytes-long";
const ALL: [&str; 3] = ["ledger.read", "ledger.propose", "ledger.review"];

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

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn token_with(claims: Value) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(SECRET),
    )
    .unwrap()
}

fn claims(tenant: &str, subject: &str, roles: &[&str]) -> Value {
    json!({
        "iss": ISSUER, "aud": AUDIENCE, "exp": now() + 600, "nbf": now() - 5,
        "tid": tenant, "oid": subject, "sculpin_principal_type": "agent", "roles": roles,
    })
}

fn token(tenant: &str, subject: &str, roles: &[&str]) -> String {
    token_with(claims(tenant, subject, roles))
}

struct Harness {
    store: PostgresLedgerStore,
    app: Router,
}

async fn harness_with(
    acceptance: AcceptancePolicy,
    limits: ApiLimits,
    policy: ClaimsPolicy,
) -> Harness {
    let store = PostgresLedgerStore::connect(&database_url(), V1Binding::Reject)
        .await
        .unwrap();
    let auth = Arc::new(
        DevHs256Authenticator::new(ISSUER.into(), AUDIENCE.into(), SECRET, policy).unwrap(),
    );
    let app = ledger_api::router(AppState::new(store.clone(), auth, limits, acceptance));
    Harness { store, app }
}

async fn harness(acceptance: AcceptancePolicy, limits: ApiLimits) -> Harness {
    harness_with(acceptance, limits, ClaimsPolicy::default()).await
}

type Reply = (StatusCode, Value, Option<String>);

impl Harness {
    async fn graph(&self, tenant: &str) -> GraphId {
        let id = GraphId::new(unique("api")).unwrap();
        self.store
            .graphs()
            .create(&NewGraph {
                graph_id: id.clone(),
                tenant_id: TenantId::new(tenant).unwrap(),
                knowledge_base_id: None,
                purpose: None,
                status: GraphStatus::Active,
            })
            .await
            .unwrap();
        id
    }

    async fn raw(&self, request: Request<Body>) -> Reply {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let correlation = response
            .headers()
            .get("x-correlation-id")
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value, correlation)
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        key: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        self.call_raw(method, path, bearer, key, body.map(|b| b.to_string()))
            .await
    }

    async fn call_raw(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        key: Option<&str>,
        body: Option<String>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(bearer) = bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        self.raw(request).await
    }

    async fn count(&self, table: &str, graph: &GraphId) -> i64 {
        sqlx::query(&format!(
            "SELECT count(*) AS n FROM {table} WHERE graph_id = $1"
        ))
        .bind(graph.as_str())
        .fetch_one(self.store.pool())
        .await
        .unwrap()
        .get::<i64, _>("n")
    }
}

fn prepare_body(expected_head: Option<&str>, quads: &[(&str, &str)], message: &str) -> Value {
    json!({
        "ref": "main",
        "expected_head": expected_head,
        "operations": quads.iter().map(|(op, q)| json!({"op": op, "quad": q})).collect::<Vec<_>>(),
        "activity": "cognitive-correction",
        "event_time": "2026-09-24T13:00:00+02:00",
        "evidence_refs": ["urn:evidence:1"],
        "source_system": "api-test",
        "message": message,
    })
}

const LEAKS: [&str; 12] = [
    "sqlx",
    "SELECT",
    "INSERT",
    "relation",
    "/data",
    "constraint",
    "postgres",
    "duplicate key",
    "violates",
    "ledger_store",
    "panicked",
    "127.0.0.1",
];

fn assert_error(response: &Reply, status: StatusCode, code: &str) {
    assert_eq!(response.0, status, "{:?}", response.1);
    assert_eq!(response.1["code"], code, "{:?}", response.1);
    let correlation = response
        .2
        .as_deref()
        .expect("correlation header on every response");
    assert_eq!(response.1["correlation_id"], correlation);
    let message = response.1["message"].as_str().unwrap();
    for leak in LEAKS {
        assert!(
            !message.contains(leak),
            "error message leaks internals: {message}"
        );
    }
}

fn strip(v: &Value) -> Value {
    let mut v = v.clone();
    v["correlation_id"] = Value::Null;
    v
}

fn dev() -> AcceptancePolicy {
    AcceptancePolicy::AllowUnvalidatedDevelopmentOnly
}

/// Prepare and accept one quad onto `main`; returns the new head.
async fn commit(h: &Harness, g: &GraphId, bearer: &str, quad: &str) -> String {
    let (_, head, _) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/refs?name=main"),
            Some(bearer),
            None,
            None,
        )
        .await;
    let expected = head.get("head").and_then(Value::as_str).map(str::to_owned);
    let body = prepare_body(expected.as_deref(), &[("add", quad)], "commit");
    let (status, prepared, _) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals"),
            Some(bearer),
            Some(&unique("k")),
            Some(body),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{prepared:?}");
    let candidate = prepared["candidate"].as_str().unwrap().to_owned();
    let (status, accepted, _) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals/{candidate}/accept"),
            Some(bearer),
            Some(&unique("k")),
            Some(json!({"ref": "main", "expected_head": expected, "reason": "ok"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{accepted:?}");
    candidate
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn authentication_is_verified_and_fails_closed() {
    let h = harness(dev(), ApiLimits::default()).await;
    let g = h.graph("tenant-a").await;
    let path = format!("/v1/graphs/{g}/refs?name=main");
    assert_error(
        &h.call("GET", &path, None, None, None).await,
        StatusCode::UNAUTHORIZED,
        "UNAUTHENTICATED",
    );
    // Wrong signature.
    let forged = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims("tenant-a", "x", &["ledger.read"]),
        &jsonwebtoken::EncodingKey::from_secret(b"another-secret-that-is-also-32-bytes-long!!"),
    )
    .unwrap();
    assert_error(
        &h.call("GET", &path, Some(&forged), None, None).await,
        StatusCode::UNAUTHORIZED,
        "UNAUTHENTICATED",
    );
    let good = || claims("tenant-a", "x", &["ledger.read"]);
    let mut expired = good();
    expired["exp"] = json!(now() - 600);
    let mut wrong_audience = good();
    wrong_audience["aud"] = json!("api://other");
    let mut wrong_issuer = good();
    wrong_issuer["iss"] = json!("https://other/");
    let mut not_yet = good();
    not_yet["nbf"] = json!(now() + 600);
    let mut no_tenant = good();
    no_tenant.as_object_mut().unwrap().remove("tid");
    let mut no_principal = good();
    no_principal.as_object_mut().unwrap().remove("oid");
    let mut no_type = good();
    no_type
        .as_object_mut()
        .unwrap()
        .remove("sculpin_principal_type");
    let mut no_exp = good();
    no_exp.as_object_mut().unwrap().remove("exp");
    let mut no_aud = good();
    no_aud.as_object_mut().unwrap().remove("aud");
    let unknown_role = claims("tenant-a", "x", &["Directory.Read"]);
    for bad in [
        expired,
        wrong_audience,
        wrong_issuer,
        not_yet,
        no_tenant,
        no_principal,
        no_type,
        no_exp,
        no_aud,
        unknown_role,
    ] {
        assert_error(
            &h.call("GET", &path, Some(&token_with(bad)), None, None)
                .await,
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );
    }
    // `alg: none` with otherwise perfect claims: the only defect is the algorithm.
    let b64 = |v: &Value| {
        use std::fmt::Write;
        let raw = v.to_string();
        let mut out = String::new();
        // URL-safe base64 without padding, hand-rolled to avoid a dependency.
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        for chunk in raw.as_bytes().chunks(3) {
            let n = chunk.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b))
                << (8 * (3 - chunk.len()));
            for i in 0..=chunk.len() {
                out.write_char(T[((n >> (18 - 6 * i)) & 63) as usize] as char)
                    .unwrap();
            }
        }
        out
    };
    let none = format!(
        "{}.{}.",
        b64(&json!({"alg": "none", "typ": "JWT"})),
        b64(&good())
    );
    assert_error(
        &h.call("GET", &path, Some(&none), None, None).await,
        StatusCode::UNAUTHORIZED,
        "UNAUTHENTICATED",
    );
    // RS256 header on an HS256-signed token: the development authenticator pins HS256.
    let confused = format!(
        "{}.{}.{}",
        b64(&json!({"alg": "RS256", "typ": "JWT"})),
        b64(&good()),
        "sig"
    );
    assert_error(
        &h.call("GET", &path, Some(&confused), None, None).await,
        StatusCode::UNAUTHORIZED,
        "UNAUTHENTICATED",
    );
    // Identity headers are ignored: only the token counts.
    let ok = token("tenant-a", "reader", &["ledger.read"]);
    let request = Request::builder()
        .method("GET")
        .uri(&path)
        .header(header::AUTHORIZATION, format!("Bearer {ok}"))
        .header("x-tenant-id", "tenant-b")
        .header("x-principal-id", "urn:sculpin:human:admin")
        .body(Body::empty())
        .unwrap();
    assert_error(&h.raw(request).await, StatusCode::NOT_FOUND, "NOT_FOUND");
    // The scheme name is case-insensitive; other schemes are not bearer tokens.
    let request = Request::builder()
        .method("GET")
        .uri(&path)
        .header(header::AUTHORIZATION, format!("bearer {ok}"))
        .body(Body::empty())
        .unwrap();
    assert_error(&h.raw(request).await, StatusCode::NOT_FOUND, "NOT_FOUND");
    for scheme in ["Basic", "Token", "Bearer:"] {
        let request = Request::builder()
            .method("GET")
            .uri(&path)
            .header(header::AUTHORIZATION, format!("{scheme} {ok}"))
            .body(Body::empty())
            .unwrap();
        assert_error(
            &h.raw(request).await,
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn capability_matrix_is_exact_per_route() {
    let h = harness(dev(), ApiLimits::default()).await;
    let g = h.graph("tenant-a").await;
    let candidate = format!("sha256:{}", "a".repeat(64));
    let routes: [(&str, String, Option<Value>, &str); 5] = [
        (
            "POST",
            format!("/v1/graphs/{g}/proposals"),
            Some(prepare_body(
                None,
                &[("add", "<urn:s> <urn:p> \"1\" .")],
                "m",
            )),
            "ledger.propose",
        ),
        (
            "POST",
            format!("/v1/graphs/{g}/proposals/{candidate}/accept"),
            Some(json!({"ref": "main", "expected_head": null})),
            "ledger.review",
        ),
        (
            "POST",
            format!("/v1/graphs/{g}/proposals/{candidate}/reject"),
            Some(json!({"ref": "main", "reason": "no"})),
            "ledger.review",
        ),
        (
            "GET",
            format!("/v1/graphs/{g}/refs?name=main"),
            None,
            "ledger.read",
        ),
        (
            "GET",
            format!("/v1/graphs/{g}/commits/{candidate}/state"),
            None,
            "ledger.read",
        ),
    ];
    for role in [
        "ledger.read",
        "ledger.propose",
        "ledger.review",
        "ledger.admin",
    ] {
        let t = token("tenant-a", &format!("actor-{role}"), &[role]);
        for (method, path, body, needs) in &routes {
            let r = h
                .call(method, path, Some(&t), Some(&unique("k")), body.clone())
                .await;
            if role == *needs {
                assert_ne!(
                    r.1["code"], "FORBIDDEN",
                    "{role} must pass {method} {path}: {:?}",
                    r.1
                );
                assert_ne!(r.1["code"], "UNAUTHENTICATED");
            } else {
                assert_error(&r, StatusCode::FORBIDDEN, "FORBIDDEN");
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn capabilities_gate_the_review_flow() {
    let h = harness(dev(), ApiLimits::default()).await;
    let g = h.graph("tenant-a").await;
    let proposer = token("tenant-a", "proposer", &["ledger.propose"]);
    let reviewer = token("tenant-a", "reviewer", &["ledger.review"]);
    let body = prepare_body(None, &[("add", "<urn:s> <urn:p> \"1\" .")], "m");
    let proposals = format!("/v1/graphs/{g}/proposals");
    let (status, prepared, _) = h
        .call("POST", &proposals, Some(&proposer), Some("k1"), Some(body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{prepared:?}");
    let candidate = prepared["candidate"].as_str().unwrap().to_owned();
    let accept = format!("/v1/graphs/{g}/proposals/{candidate}/accept");
    let accept_body = json!({"ref": "main", "expected_head": null, "reason": "ok"});
    assert_error(
        &h.call(
            "POST",
            &accept,
            Some(&proposer),
            Some("k2"),
            Some(accept_body.clone()),
        )
        .await,
        StatusCode::FORBIDDEN,
        "FORBIDDEN",
    );
    let (status, accepted, _) = h
        .call(
            "POST",
            &accept,
            Some(&reviewer),
            Some("k2"),
            Some(accept_body),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{accepted:?}");
    assert_eq!(accepted["head"], candidate);
    assert_eq!(accepted["ref_version"], 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn persisted_actor_and_correlation_come_from_the_verified_token() {
    let policy = ClaimsPolicy {
        on_behalf_of_claim: Some("obo".into()),
        ..ClaimsPolicy::default()
    };
    let h = harness_with(dev(), ApiLimits::default(), policy).await;
    let g = h.graph("tenant-a").await;
    let mut c = claims("tenant-a", "agent-7", &ALL);
    c["obo"] = json!("alice");
    let t = token_with(c);
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/graphs/{g}/proposals"))
        .header(header::AUTHORIZATION, format!("Bearer {t}"))
        .header("idempotency-key", "identity-1")
        .header("x-correlation-id", "client-corr-identity")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            prepare_body(None, &[("add", "<urn:s> <urn:p> \"1\" .")], "m").to_string(),
        ))
        .unwrap();
    let (status, prepared, correlation) = h.raw(request).await;
    assert_eq!(status, StatusCode::CREATED, "{prepared:?}");
    assert_eq!(correlation.as_deref(), Some("client-corr-identity"));
    let row = sqlx::query(
        "SELECT tenant_id, principal_id, principal_type, on_behalf_of, correlation_id FROM proposals WHERE proposal_id = $1",
    )
    .bind(prepared["proposal_id"].as_i64().unwrap())
    .fetch_one(h.store.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("tenant_id"), "tenant-a");
    assert_eq!(
        row.get::<String, _>("principal_id"),
        "urn:sculpin:agent:agent-7"
    );
    assert_eq!(row.get::<String, _>("principal_type"), "agent");
    assert_eq!(
        row.get::<Option<String>, _>("on_behalf_of").as_deref(),
        Some("urn:sculpin:human:alice")
    );
    assert_eq!(
        row.get::<Option<String>, _>("correlation_id").as_deref(),
        Some("client-corr-identity")
    );
    // Accept by a human reviewer: decision and ref event carry the reviewer, not the proposer.
    let mut rc = claims("tenant-a", "reviewer-1", &ALL);
    rc["sculpin_principal_type"] = json!("human");
    let reviewer = token_with(rc);
    let candidate = prepared["candidate"].as_str().unwrap();
    let (status, accepted, correlation) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals/{candidate}/accept"),
            Some(&reviewer),
            Some("identity-2"),
            Some(json!({"ref": "main", "expected_head": null})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{accepted:?}");
    let row = sqlx::query(
        "SELECT d.principal_id AS dp, d.principal_type AS dt, d.on_behalf_of AS dobo, d.correlation_id AS dc, \
         e.principal_id AS ep, e.correlation_id AS ec, i.principal_type AS it, i.on_behalf_of AS iobo \
         FROM decisions d JOIN ref_events e ON e.event_id = d.ref_event_id \
         JOIN idempotency i ON i.result_decision_id = d.decision_id WHERE d.decision_id = $1",
    )
    .bind(accepted["decision_id"].as_i64().unwrap())
    .fetch_one(h.store.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("dp"), "urn:sculpin:human:reviewer-1");
    assert_eq!(row.get::<String, _>("dt"), "human");
    assert_eq!(row.get::<Option<String>, _>("dobo"), None);
    assert_eq!(row.get::<String, _>("ep"), "urn:sculpin:human:reviewer-1");
    assert_eq!(row.get::<Option<String>, _>("dc"), correlation.clone());
    assert_eq!(row.get::<Option<String>, _>("ec"), correlation);
    assert_eq!(row.get::<String, _>("it"), "human");
    assert_eq!(row.get::<Option<String>, _>("iobo"), None);
    // Delegation is part of the idempotency namespace at the API: the same key by the same
    // agent without delegation is a new request, not a replay.
    let plain = token("tenant-a", "agent-7", &ALL);
    let (status, again, _) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals"),
            Some(&plain),
            Some("identity-1"),
            Some(prepare_body(
                None,
                &[("add", "<urn:s> <urn:p> \"1\" .")],
                "m",
            )),
        )
        .await;
    assert!(
        status == StatusCode::CREATED || again["code"] == "HEAD_CHANGED",
        "not a replay of the delegated request: {again:?}"
    );
    assert_ne!(again["replayed"], true);
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn foreign_tenant_graphs_and_commits_are_indistinguishable_from_nonexistent() {
    let h = harness(dev(), ApiLimits::default()).await;
    let ga = h.graph("tenant-a").await;
    let gb = h.graph("tenant-b").await;
    let a = token("tenant-a", "actor", &ALL);
    let b = token("tenant-b", "actor", &ALL);
    let head_a = commit(&h, &ga, &a, "<urn:a> <urn:p> \"1\" .").await;
    let _head_b = commit(&h, &gb, &b, "<urn:b> <urn:p> \"1\" .").await;
    let before = (
        h.count("proposals", &ga).await,
        h.count("decisions", &ga).await,
        h.count("idempotency", &ga).await,
    );

    let missing = GraphId::new(unique("missing")).unwrap();
    let foreign = h
        .call(
            "GET",
            &format!("/v1/graphs/{ga}/refs?name=main"),
            Some(&b),
            None,
            None,
        )
        .await;
    let nonexistent = h
        .call(
            "GET",
            &format!("/v1/graphs/{missing}/refs?name=main"),
            Some(&b),
            None,
            None,
        )
        .await;
    assert_error(&foreign, StatusCode::NOT_FOUND, "NOT_FOUND");
    assert_error(&nonexistent, StatusCode::NOT_FOUND, "NOT_FOUND");
    assert_eq!(strip(&foreign.1), strip(&nonexistent.1));
    assert!(!foreign.1.to_string().contains(ga.as_str()));

    for path in [
        format!("/v1/graphs/{gb}/commits/{head_a}/state"),
        format!("/v1/graphs/{ga}/commits/{head_a}/state"),
        format!("/v1/graphs/{gb}/commits/sha256:{}/state", "0".repeat(64)),
        format!("/v1/graphs/{gb}/commits/not-a-commit/state"),
    ] {
        let r = h.call("GET", &path, Some(&b), None, None).await;
        assert_error(&r, StatusCode::NOT_FOUND, "NOT_FOUND");
        assert!(!r.1.to_string().contains(ga.as_str()), "{:?}", r.1);
        assert!(!r.1.to_string().contains(&head_a), "{:?}", r.1);
    }
    // Foreign mutations: prepare, accept and reject are NOT_FOUND with the same envelope
    // as a nonexistent graph, and nothing is written.
    let body = prepare_body(None, &[("add", "<urn:x> <urn:p> \"1\" .")], "m");
    let foreign_writes = [
        h.call(
            "POST",
            &format!("/v1/graphs/{ga}/proposals"),
            Some(&b),
            Some("kb"),
            Some(body.clone()),
        )
        .await,
        h.call(
            "POST",
            &format!("/v1/graphs/{ga}/proposals/{head_a}/accept"),
            Some(&b),
            Some("kb1"),
            Some(json!({"ref": "main", "expected_head": null})),
        )
        .await,
        h.call(
            "POST",
            &format!("/v1/graphs/{ga}/proposals/{head_a}/reject"),
            Some(&b),
            Some("kb2"),
            Some(json!({"ref": "main", "reason": "no"})),
        )
        .await,
    ];
    let missing_write = h
        .call(
            "POST",
            &format!("/v1/graphs/{missing}/proposals"),
            Some(&b),
            Some("kb"),
            Some(body),
        )
        .await;
    assert_error(&missing_write, StatusCode::NOT_FOUND, "NOT_FOUND");
    for r in &foreign_writes {
        assert_error(r, StatusCode::NOT_FOUND, "NOT_FOUND");
        assert_eq!(strip(&r.1), strip(&missing_write.1));
    }
    let after = (
        h.count("proposals", &ga).await,
        h.count("decisions", &ga).await,
        h.count("idempotency", &ga).await,
    );
    assert_eq!(before, after, "foreign requests must write nothing");
    // A's own view is intact.
    let (status, r, _) = h
        .call(
            "GET",
            &format!("/v1/graphs/{ga}/refs?name=main"),
            Some(&a),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(r["head"], head_a);
    let (status, state, _) = h
        .call(
            "GET",
            &format!("/v1/graphs/{ga}/commits/{head_a}/state"),
            Some(&a),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{state:?}");
    assert_eq!(state["quads"], json!(["<urn:a> <urn:p> \"1\" ."]));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn lost_responses_replay_identically_and_conflicts_are_detected() {
    let h = harness(dev(), ApiLimits::default()).await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let proposals = format!("/v1/graphs/{g}/proposals");
    let first_body = r#"{"message":"m","ref":"feature/curation-2026","activity":"a",
        "operations":[{"op":"add","quad":"<urn:s> <urn:p> \"2\" ."},{"op":"add","quad":"<urn:s> <urn:p> \"1\" ."}],
        "evidence_refs":["urn:e:2","urn:e:1"],"expected_head":null,"event_time":"2026-09-24T13:00:00+02:00"}"#;
    let (s1, first, c1) = h
        .call_raw(
            "POST",
            &proposals,
            Some(&t),
            Some("lost-1"),
            Some(first_body.into()),
        )
        .await;
    assert_eq!(s1, StatusCode::CREATED, "{first:?}");
    assert_eq!(first["replayed"], false);
    // The client never saw `first` and retries with different JSON key order, reversed
    // operation order, reordered/duplicated evidence and the same instant in UTC: the same
    // canonical request, the same durable result.
    let retry_body = r#"{"event_time":"2026-09-24T11:00:00Z","expected_head":null,"ref":"feature/curation-2026",
        "evidence_refs":["urn:e:1","urn:e:2","urn:e:1"],
        "operations":[{"quad":"<urn:s> <urn:p> \"1\" .","op":"add"},{"quad":"<urn:s> <urn:p> \"2\" .","op":"add"}],
        "activity":"a","message":"m"}"#;
    let (s2, second, c2) = h
        .call_raw(
            "POST",
            &proposals,
            Some(&t),
            Some("lost-1"),
            Some(retry_body.into()),
        )
        .await;
    assert_eq!(s2, StatusCode::OK, "{second:?}");
    assert_eq!(second["replayed"], true);
    for field in [
        "proposal_id",
        "candidate",
        "requested_patch",
        "effective_patch",
    ] {
        assert_eq!(
            first[field], second[field],
            "{field} must replay identically"
        );
    }
    assert_ne!(c1, c2, "each attempt has its own correlation id");
    // Same key, different request: conflict.
    let different = prepare_body(None, &[("add", "<urn:other> <urn:p> \"1\" .")], "m");
    assert_error(
        &h.call(
            "POST",
            &proposals,
            Some(&t),
            Some("lost-1"),
            Some(different),
        )
        .await,
        StatusCode::CONFLICT,
        "IDEMPOTENCY_CONFLICT",
    );
    // Another actor of the same tenant with the same key is an independent namespace; the
    // v2 envelope carries provenance, so the candidate differs while the delta is the same.
    let other = token("tenant-a", "someone-else", &["ledger.propose"]);
    let (s3, third, _) = h
        .call_raw(
            "POST",
            &proposals,
            Some(&other),
            Some("lost-1"),
            Some(first_body.into()),
        )
        .await;
    assert_eq!(s3, StatusCode::CREATED, "{third:?}");
    assert_ne!(third["candidate"], first["candidate"]);
    assert_ne!(third["proposal_id"], first["proposal_id"]);
    assert_eq!(third["effective_patch"], first["effective_patch"]);

    // Accept, lose the response, retry: identical, and the ref advanced exactly once.
    let candidate = first["candidate"].as_str().unwrap().to_owned();
    let accept = format!("/v1/graphs/{g}/proposals/{candidate}/accept");
    let accept_body = json!({"ref": "feature/curation-2026", "expected_head": null});
    let (s4, a1, _) = h
        .call(
            "POST",
            &accept,
            Some(&t),
            Some("lost-2"),
            Some(accept_body.clone()),
        )
        .await;
    assert_eq!(s4, StatusCode::OK, "{a1:?}");
    let (s5, a2, _) = h
        .call("POST", &accept, Some(&t), Some("lost-2"), Some(accept_body))
        .await;
    assert_eq!(s5, StatusCode::OK, "{a2:?}");
    assert_eq!(a2["replayed"], true);
    for field in [
        "decision_id",
        "ref_event_id",
        "outbox_id",
        "ref_version",
        "head",
    ] {
        assert_eq!(a1[field], a2[field]);
    }
    let (_, r, _) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/refs?name=feature/curation-2026"),
            Some(&t),
            None,
            None,
        )
        .await;
    assert_eq!(r["version"], 1);
    assert_eq!(r["head"], candidate);
    let outbox: i64 =
        sqlx::query("SELECT count(*) AS n FROM projection_outbox WHERE graph_id = $1")
            .bind(g.as_str())
            .fetch_one(h.store.pool())
            .await
            .unwrap()
            .get("n");
    assert_eq!(outbox, 1, "exactly one outbox row for one ref move");
    // Re-accepting the already accepted candidate under a new key with a stale expected
    // head is HEAD_CHANGED; accepting the other actor's identical delta onto the moved ref
    // is a lineage error, never a duplicate publication.
    assert_error(
        &h.call(
            "POST",
            &accept,
            Some(&t),
            Some("lost-3"),
            Some(json!({"ref": "feature/curation-2026", "expected_head": null, "reason": "again"})),
        )
        .await,
        StatusCode::CONFLICT,
        "HEAD_CHANGED",
    );
    let third_candidate = third["candidate"].as_str().unwrap();
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals/{third_candidate}/accept"),
            Some(&t),
            Some("lost-4"),
            Some(json!({"ref": "feature/curation-2026", "expected_head": candidate})),
        )
        .await;
    assert_eq!(r.0, StatusCode::CONFLICT, "{:?}", r.1);
    assert!(
        r.1["code"] == "LINEAGE_MISMATCH" || r.1["code"] == "HEAD_CHANGED",
        "{:?}",
        r.1
    );
    let (_, r, _) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/refs?name=feature/curation-2026"),
            Some(&t),
            None,
            None,
        )
        .await;
    assert_eq!(r["version"], 1, "the ref did not move");
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn request_shape_is_strict_and_client_cannot_supply_identity_fields() {
    let h = harness(dev(), ApiLimits::default()).await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &["ledger.propose", "ledger.review"]);
    let proposals = format!("/v1/graphs/{g}/proposals");
    let good = prepare_body(None, &[("add", "<urn:s> <urn:p> \"1\" .")], "m");
    // Missing, empty, oversized or control-character Idempotency-Key.
    assert_error(
        &h.call("POST", &proposals, Some(&t), None, Some(good.clone()))
            .await,
        StatusCode::BAD_REQUEST,
        "INVALID_REQUEST",
    );
    for key in ["", &"k".repeat(257), "tab\there"] {
        let request = Request::builder()
            .method("POST")
            .uri(&proposals)
            .header(header::AUTHORIZATION, format!("Bearer {t}"))
            .header("idempotency-key", key)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(good.to_string()))
            .unwrap();
        let r = h.raw(request).await;
        assert_eq!(r.0, StatusCode::BAD_REQUEST, "key {key:?}: {:?}", r.1);
    }
    // Client-supplied identity or server-only fields are unknown fields → rejected.
    for (field, value) in [
        ("tenant_id", json!("tenant-b")),
        ("principal_id", json!("urn:sculpin:human:admin")),
        ("principal_type", json!("human")),
        ("on_behalf_of", json!("urn:sculpin:human:x")),
        ("request_digest", json!("sha256:00")),
        ("recorded_at", json!("2026-01-01T00:00:00Z")),
        ("author", json!("someone")),
        ("validation_policy", json!("skip")),
    ] {
        let mut b = good.clone();
        b[field] = value;
        assert_error(
            &h.call("POST", &proposals, Some(&t), Some("shape"), Some(b))
                .await,
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
        );
    }
    // Blank nodes, malformed quads, bad op names, empty patch, too many evidence refs,
    // empty activity/message, control characters.
    for operations in [
        json!([{"op": "add", "quad": "_:b <urn:p> \"1\" ."}]),
        json!([{"op": "add", "quad": "not a quad"}]),
        json!([{"op": "upsert", "quad": "<urn:s> <urn:p> \"1\" ."}]),
        json!([]),
    ] {
        let mut b = good.clone();
        b["operations"] = operations;
        assert_error(
            &h.call("POST", &proposals, Some(&t), Some("shape"), Some(b))
                .await,
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
        );
    }
    let mut b = good.clone();
    b["evidence_refs"] = json!((0..65).map(|i| format!("urn:e:{i}")).collect::<Vec<_>>());
    assert_error(
        &h.call("POST", &proposals, Some(&t), Some("shape"), Some(b))
            .await,
        StatusCode::BAD_REQUEST,
        "INVALID_REQUEST",
    );
    for (field, value) in [
        ("activity", json!("")),
        ("message", json!("")),
        ("message", json!("line\nbreak")),
        ("ref", json!("")),
        ("ref", json!("r".repeat(129))),
    ] {
        let mut b = good.clone();
        b[field] = value;
        assert_error(
            &h.call("POST", &proposals, Some(&t), Some("shape"), Some(b))
                .await,
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
        );
    }
    // Deleting a quad absent from the base: BASE_MISMATCH (strict prepare).
    let missing_delete = prepare_body(None, &[("delete", "<urn:s> <urn:p> \"1\" .")], "m");
    assert_error(
        &h.call(
            "POST",
            &proposals,
            Some(&t),
            Some("del"),
            Some(missing_delete),
        )
        .await,
        StatusCode::PRECONDITION_FAILED,
        "BASE_MISMATCH",
    );
    // Reason bounds on accept/reject.
    let candidate = format!("sha256:{}", "a".repeat(64));
    for (path, body) in [
        (
            format!("/v1/graphs/{g}/proposals/{candidate}/accept"),
            json!({"ref": "main", "expected_head": null, "reason": ""}),
        ),
        (
            format!("/v1/graphs/{g}/proposals/{candidate}/accept"),
            json!({"ref": "main", "expected_head": null, "reason": "r".repeat(4097)}),
        ),
        (
            format!("/v1/graphs/{g}/proposals/{candidate}/reject"),
            json!({"ref": "main", "reason": ""}),
        ),
        (
            format!("/v1/graphs/{g}/proposals/{candidate}/reject"),
            json!({"ref": "main", "reason": "r".repeat(4097)}),
        ),
        (
            format!("/v1/graphs/{g}/proposals/not-a-commit/reject"),
            json!({"ref": "main", "reason": "no"}),
        ),
    ] {
        assert_error(
            &h.call("POST", &path, Some(&t), Some("shape"), Some(body))
                .await,
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
        );
    }
    // Non-JSON content type still yields the envelope.
    let request = Request::builder()
        .method("POST")
        .uri(&proposals)
        .header(header::AUTHORIZATION, format!("Bearer {t}"))
        .header("idempotency-key", "ct")
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from("{}"))
        .unwrap();
    assert_error(
        &h.raw(request).await,
        StatusCode::BAD_REQUEST,
        "INVALID_REQUEST",
    );
    // Correlation id supplied by the client is echoed in header and body; an invalid one
    // is replaced.
    let request = Request::builder()
        .method("GET")
        .uri(format!("/v1/graphs/{g}/refs?name=main"))
        .header("x-correlation-id", "client-corr-42")
        .body(Body::empty())
        .unwrap();
    let r = h.raw(request).await;
    assert_error(&r, StatusCode::UNAUTHORIZED, "UNAUTHENTICATED");
    assert_eq!(r.2.as_deref(), Some("client-corr-42"));
    let request = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-correlation-id", "has space")
        .body(Body::empty())
        .unwrap();
    let r = h.raw(request).await;
    assert_ne!(r.2.as_deref(), Some("has space"));
    assert!(r.2.is_some());
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn acceptance_fails_closed_without_validation_by_default() {
    let h = harness(AcceptancePolicy::RequireValidation, ApiLimits::default()).await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let (status, prepared, _) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals"),
            Some(&t),
            Some("p"),
            Some(prepare_body(
                None,
                &[("add", "<urn:s> <urn:p> \"1\" .")],
                "m",
            )),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{prepared:?}");
    let candidate = prepared["candidate"].as_str().unwrap();
    let before = (
        h.count("decisions", &g).await,
        h.count("ref_events", &g).await,
        h.count("idempotency", &g).await,
    );
    assert_error(
        &h.call(
            "POST",
            &format!("/v1/graphs/{g}/proposals/{candidate}/accept"),
            Some(&t),
            Some("a"),
            Some(json!({"ref": "main", "expected_head": null})),
        )
        .await,
        StatusCode::CONFLICT,
        "VALIDATION_REQUIRED",
    );
    assert_eq!(
        before,
        (
            h.count("decisions", &g).await,
            h.count("ref_events", &g).await,
            h.count("idempotency", &g).await
        ),
        "a refused accept records nothing"
    );
    assert_error(
        &h.call(
            "GET",
            &format!("/v1/graphs/{g}/refs?name=main"),
            Some(&t),
            None,
            None,
        )
        .await,
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
    );
    // Reject still works without validation (it publishes nothing).
    let (status, rejected, _) = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals/{candidate}/reject"),
            Some(&t),
            Some("r"),
            Some(json!({"ref": "main", "reason": "declined"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rejected:?}");
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn resource_limits_are_enforced_with_a_stable_code() {
    let limits = ApiLimits {
        body_bytes: 2048,
        max_operations: 2,
        max_term_bytes: 200,
        max_metadata_bytes: 300,
        reconstruction: ReconstructionLimits {
            max_depth: 10,
            max_quads: 2,
            max_bytes: 1024 * 1024,
        },
        max_state_export_bytes: 1024 * 1024,
        request_timeout: Duration::from_secs(30),
        max_concurrent_expensive: 4,
    };
    let h = harness(dev(), limits).await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let proposals = format!("/v1/graphs/{g}/proposals");
    let many = prepare_body(
        None,
        &[
            ("add", "<urn:s> <urn:p> \"1\" ."),
            ("add", "<urn:s> <urn:p> \"2\" ."),
            ("add", "<urn:s> <urn:p> \"3\" ."),
        ],
        "m",
    );
    let r = h
        .call("POST", &proposals, Some(&t), Some("ops"), Some(many))
        .await;
    assert_error(&r, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
    assert!(r.1["message"].as_str().unwrap().contains("operations"));
    let long = format!("<urn:s> <urn:p> \"{}\" .", "x".repeat(300));
    let r = h
        .call(
            "POST",
            &proposals,
            Some(&t),
            Some("term"),
            Some(prepare_body(None, &[("add", &long)], "m")),
        )
        .await;
    assert_error(&r, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
    assert!(r.1["message"].as_str().unwrap().contains("quad"));
    let mut meta = prepare_body(None, &[("add", "<urn:s> <urn:p> \"1\" .")], "m");
    meta["message"] = json!("m".repeat(299));
    let r = h
        .call("POST", &proposals, Some(&t), Some("meta"), Some(meta))
        .await;
    assert_error(&r, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
    assert!(r.1["message"].as_str().unwrap().contains("metadata"));
    // Transport body limit: a semantically tiny request padded with JSON whitespace so
    // only the byte limit can fire.
    let padded = format!(
        "{}{}",
        " ".repeat(2100),
        prepare_body(None, &[("add", "<urn:s> <urn:p> \"1\" .")], "m")
    );
    let r = h
        .call_raw("POST", &proposals, Some(&t), Some("body"), Some(padded))
        .await;
    assert_error(&r, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
    assert!(
        r.1["message"].as_str().unwrap().contains("request body"),
        "{:?}",
        r.1
    );

    // Reconstruction quads: two single-quad commits fit exactly; the third is refused at
    // PREPARE (the candidate would exceed the limit), so nothing unreadable is ever accepted.
    let h1 = commit(&h, &g, &t, "<urn:a> <urn:p> \"1\" .").await;
    let h2 = commit(&h, &g, &t, "<urn:a> <urn:p> \"2\" .").await;
    let r = h
        .call(
            "POST",
            &proposals,
            Some(&t),
            Some("third"),
            Some(prepare_body(
                Some(&h2),
                &[("add", "<urn:a> <urn:p> \"3\" .")],
                "m",
            )),
        )
        .await;
    assert_error(&r, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
    assert!(
        r.1["message"].as_str().unwrap().contains("quads"),
        "{:?}",
        r.1
    );
    // A replacement at capacity is fine (delete one, add one → still 2 quads).
    let (status, replaced, _) = h
        .call(
            "POST",
            &proposals,
            Some(&t),
            Some("replace"),
            Some(prepare_body(
                Some(&h2),
                &[
                    ("delete", "<urn:a> <urn:p> \"2\" ."),
                    ("add", "<urn:a> <urn:p> \"9\" ."),
                ],
                "m",
            )),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{replaced:?}");
    for head in [&h1, &h2] {
        let (status, state, _) = h
            .call(
                "GET",
                &format!("/v1/graphs/{g}/commits/{head}/state"),
                Some(&t),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{state:?}");
    }
    let (_, state, _) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/commits/{h2}/state"),
            Some(&t),
            None,
            None,
        )
        .await;
    assert_eq!(
        state["quads"].as_array().unwrap().len(),
        2,
        "boundary: exactly max_quads is readable"
    );

    // Depth: a chain of two is the ceiling; the third prepare is refused by depth alone.
    let deep = harness(
        dev(),
        ApiLimits {
            reconstruction: ReconstructionLimits {
                max_depth: 2,
                max_quads: 1000,
                max_bytes: 1024 * 1024,
            },
            ..ApiLimits::default()
        },
    )
    .await;
    let gd = deep.graph("tenant-a").await;
    let d1 = commit(&deep, &gd, &t, "<urn:d> <urn:p> \"1\" .").await;
    let d2 = commit(&deep, &gd, &t, "<urn:d> <urn:p> \"2\" .").await;
    let r = deep
        .call(
            "POST",
            &format!("/v1/graphs/{gd}/proposals"),
            Some(&t),
            Some("d3"),
            Some(prepare_body(
                Some(&d2),
                &[("add", "<urn:d> <urn:p> \"3\" .")],
                "m",
            )),
        )
        .await;
    assert_error(&r, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
    assert!(
        r.1["message"].as_str().unwrap().contains("depth"),
        "{:?}",
        r.1
    );
    let (status, _, _) = deep
        .call(
            "GET",
            &format!("/v1/graphs/{gd}/commits/{d1}/state"),
            Some(&t),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = deep
        .call(
            "GET",
            &format!("/v1/graphs/{gd}/commits/{d2}/state"),
            Some(&t),
            None,
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "boundary: exactly max_depth is readable"
    );

    // Bytes: the state's canonical N-Quads size is bounded on prepare and on read; the
    // export limit bounds reads even when reconstruction would allow more.
    let bytes = harness(
        dev(),
        ApiLimits {
            reconstruction: ReconstructionLimits {
                max_depth: 100,
                max_quads: 1000,
                max_bytes: 60,
            },
            max_state_export_bytes: 30,
            ..ApiLimits::default()
        },
    )
    .await;
    let gb = bytes.graph("tenant-a").await;
    let b1 = commit(&bytes, &gb, &t, "<urn:b> <urn:p> \"1\" .").await; // 25 bytes + newline
    let r = bytes
        .call(
            "POST",
            &format!("/v1/graphs/{gb}/proposals"),
            Some(&t),
            Some("b2"),
            Some(prepare_body(
                Some(&b1),
                &[
                    ("add", "<urn:b> <urn:p> \"2\" ."),
                    ("add", "<urn:b> <urn:p> \"3\" ."),
                ],
                "m",
            )),
        )
        .await;
    assert_error(&r, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
    assert!(
        r.1["message"].as_str().unwrap().contains("bytes"),
        "{:?}",
        r.1
    );
    let b2 = commit(&bytes, &gb, &t, "<urn:b> <urn:p> \"2\" .").await; // 52 bytes: within reconstruction, beyond export
    let (status, _, _) = bytes
        .call(
            "GET",
            &format!("/v1/graphs/{gb}/commits/{b1}/state"),
            Some(&t),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_error(
        &bytes
            .call(
                "GET",
                &format!("/v1/graphs/{gb}/commits/{b2}/state"),
                Some(&t),
                None,
                None,
            )
            .await,
        StatusCode::PAYLOAD_TOO_LARGE,
        "RESOURCE_LIMIT",
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn readiness_reports_the_database_and_openapi_is_served() {
    let h = harness(dev(), ApiLimits::default()).await;
    let (status, body, _) = h.call("GET", "/ready", None, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let (status, doc, _) = h.call("GET", "/openapi.json", None, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(doc["paths"].is_object());
    let (status, _, _) = h.call("GET", "/health", None, None, None).await;
    assert_eq!(status, StatusCode::OK);
    // The removed bootstrap endpoint does not exist and unknown routes use the envelope.
    let r = h
        .call("POST", "/v1/commits", None, None, Some(json!({})))
        .await;
    assert_error(&r, StatusCode::NOT_FOUND, "NOT_FOUND");
}
