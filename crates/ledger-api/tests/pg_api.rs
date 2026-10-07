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
    auth::{ClaimsPolicy, DevHs256Authenticator, OidcAuthenticator, SharedAuthenticator},
};
use ledger_core::{GraphId, TenantId};
use ledger_store::{
    DbSessionLimits, GraphStatus, NewGraph, PostgresLedgerStore, ReconstructionLimits, V1Binding,
};
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
    /// Runtime-identity store behind the router.
    store: PostgresLedgerStore,
    /// Owner-identity store for provisioning and raw assertions.
    owner: PostgresLedgerStore,
    app: Router,
}

/// Owner migrates and grants once; the served store connects as a least-privilege runtime
/// role (created here if absent), so every HTTP test runs under production privileges.
async fn harness_with(
    acceptance: AcceptancePolicy,
    limits: ApiLimits,
    policy: ClaimsPolicy,
) -> Harness {
    let auth: SharedAuthenticator = Arc::new(
        DevHs256Authenticator::new(ISSUER.into(), AUDIENCE.into(), SECRET, policy).unwrap(),
    );
    harness_full(acceptance, limits, auth, DbSessionLimits::default()).await
}

/// One "replica": its own runtime-identity pool and authenticator over the shared database.
async fn harness_full(
    acceptance: AcceptancePolicy,
    limits: ApiLimits,
    auth: SharedAuthenticator,
    session: DbSessionLimits,
) -> Harness {
    harness_in(&database_url(), acceptance, limits, auth, session).await
}

/// The test URL with its database name replaced (`…/ledger?…` → `…/<name>?…`).
fn url_for_database(base: &str, name: &str) -> String {
    let (head, query) = match base.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (base, None),
    };
    let slash = head.rfind('/').expect("database url has a path");
    let mut url = format!("{}/{name}", &head[..slash]);
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    url
}

/// A throwaway database (owner URL) for tests that take table locks or otherwise disturb
/// every other session of the database; the shared database stays undisturbed.
async fn fresh_database(prefix: &str) -> String {
    let base = database_url();
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    let name = unique(prefix).replace('-', "_").to_lowercase();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    url_for_database(&base, &name)
}

/// `harness_full` against an explicit owner URL (shared or throwaway database).
async fn harness_in(
    url: &str,
    acceptance: AcceptancePolicy,
    limits: ApiLimits,
    auth: SharedAuthenticator,
    session: DbSessionLimits,
) -> Harness {
    harness_in_hooked(url, acceptance, limits, auth, session, None).await
}

/// `harness_in` whose served store carries a test-only pause hook (Plan 0013 M1): the
/// request lifecycle tests pause the workflow transaction behind a live router.
async fn harness_in_hooked(
    url: &str,
    acceptance: AcceptancePolicy,
    limits: ApiLimits,
    auth: SharedAuthenticator,
    session: DbSessionLimits,
    hook: Option<ledger_store::test_hooks::PauseHook>,
) -> Harness {
    let owner = PostgresLedgerStore::connect_and_migrate(url, V1Binding::Reject)
        .await
        .unwrap();
    // Role creation touches cluster-wide catalog rows and the grant on the shared database
    // touches its per-database ACLs; tests in this binary run in parallel, so this happens
    // exactly once per process. Throwaway databases grant separately below (per-database
    // ACLs only; nothing else runs against a throwaway database).
    static SETUP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SETUP
        .get_or_init(|| async {
            use sqlx::Connection;
            let mut conn = sqlx::postgres::PgConnection::connect(&database_url())
                .await
                .unwrap();
            // The shared database may not have been migrated yet when a throwaway-database
            // test reaches this first: migrate it before granting (idempotent).
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
    if url != database_url() {
        // A throwaway database: grants are per database, so the (cluster-wide) role is
        // granted here as well; nothing else runs against this database concurrently.
        use sqlx::Connection;
        let mut conn = sqlx::postgres::PgConnection::connect(url).await.unwrap();
        ledger_store::schema::grant_runtime_role(&mut conn, "ledger_rt_api")
            .await
            .unwrap();
        conn.close().await.unwrap();
    }
    let runtime_url = {
        let (scheme, rest) = url.split_once("://").unwrap();
        let (_, host_part) = rest.rsplit_once('@').unwrap();
        format!("{scheme}://ledger_rt_api:rt-api-test-secret@{host_part}")
    };
    let mut store = PostgresLedgerStore::connect_with(&runtime_url, V1Binding::Reject, session)
        .await
        .expect("runtime role connects with verify-only startup");
    if let Some(hook) = hook {
        store = store.with_workflow_pause_hook(hook);
    }
    let app = ledger_api::router(AppState::new(store.clone(), auth, limits, acceptance));
    Harness { store, owner, app }
}

async fn harness(acceptance: AcceptancePolicy, limits: ApiLimits) -> Harness {
    harness_with(acceptance, limits, ClaimsPolicy::default()).await
}

type Reply = (StatusCode, Value, Option<String>);

impl Harness {
    async fn graph(&self, tenant: &str) -> GraphId {
        let id = GraphId::new(unique("api")).unwrap();
        self.owner
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
        .fetch_one(self.owner.pool())
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

const LEAKS: [&str; 17] = [
    "canceling statement",
    "pg_sleep",
    "pool timed out",
    "error returned from database",
    "localhost",
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
    .fetch_one(h.owner.pool())
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
    .fetch_one(h.owner.pool())
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
    // Genesis onto `main` (ADR-0022: only `main` is born by genesis; other branches are
    // created explicitly and have their own suite).
    let h = harness(dev(), ApiLimits::default()).await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let proposals = format!("/v1/graphs/{g}/proposals");
    let first_body = r#"{"message":"m","ref":"main","activity":"a",
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
    let retry_body = r#"{"event_time":"2026-09-24T11:00:00Z","expected_head":null,"ref":"main",
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
    let accept_body = json!({"ref": "main", "expected_head": null});
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
            &format!("/v1/graphs/{g}/refs?name=main"),
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
            .fetch_one(h.owner.pool())
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
            Some(json!({"ref": "main", "expected_head": null, "reason": "again"})),
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
            Some(json!({"ref": "main", "expected_head": candidate})),
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
            &format!("/v1/graphs/{g}/refs?name=main"),
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
        ..ApiLimits::default()
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
    // The served store runs as the least-privilege runtime role, not the owner.
    let (who, is_super): (String, bool) = sqlx::query_as(
        "SELECT current_user::text, (SELECT rolsuper FROM pg_roles WHERE rolname = current_user)",
    )
    .fetch_one(h.store.pool())
    .await
    .unwrap();
    assert_eq!(who, "ledger_rt_api");
    assert!(!is_super);
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

// =========================================================================================
// Plan 0005 §4 — two replicas, one key source: JWKS rotation, expiry boundaries, replay
// =========================================================================================

const RSA_A: &str = include_str!("fixtures/test-rsa-a.pkcs8");
const RSA_C: &str = include_str!("fixtures/test-rsa-c.pkcs8");
const JWKS_INITIAL: &str = include_str!("fixtures/jwks-initial.json");
const JWKS_ROTATED: &str = include_str!("fixtures/jwks-rotated.json");
const OIDC_ISS: &str = "https://issuer.test/";
const OIDC_AUD: &str = "api://ledger";

/// A local JWKS endpoint whose document can be swapped; counts fetches.
struct JwksServer {
    url: String,
    document: Arc<std::sync::Mutex<String>>,
    hits: Arc<std::sync::atomic::AtomicUsize>,
}

async fn jwks_server(initial: &str) -> JwksServer {
    let document = Arc::new(std::sync::Mutex::new(initial.to_owned()));
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (d, h) = (document.clone(), hits.clone());
    let app = Router::new().route(
        "/keys",
        axum::routing::get(move || {
            let d = d.clone();
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    d.lock().unwrap().clone(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/keys", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    JwksServer {
        url,
        document,
        hits,
    }
}

fn rs256(kid: &str, pem: &str, claims: &Value) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.to_owned());
    jsonwebtoken::encode(
        &header,
        claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
    )
    .unwrap()
}

fn oidc_claims(exp_offset: i64, nbf_offset: i64) -> Value {
    let now = now() as i64;
    json!({
        "iss": OIDC_ISS, "aud": OIDC_AUD, "exp": now + exp_offset, "nbf": now + nbf_offset,
        "tid": "t1", "oid": "actor-1", "sculpin_principal_type": "agent", "roles": ALL,
    })
}

fn oidc(url: &str) -> SharedAuthenticator {
    Arc::new(
        OidcAuthenticator::new(
            OIDC_ISS.into(),
            OIDC_AUD.into(),
            url.to_owned(),
            ClaimsPolicy::default(),
        )
        .with_refresh_policy(Duration::ZERO, Duration::from_secs(3600)),
    )
}

/// Two identical requests answered by two replicas: both succeed, exactly one is the
/// original execution, and the durable result is the same.
fn assert_replayed_pair(a: &Reply, b: &Reply, fields: &[&str]) {
    for r in [a, b] {
        assert!(r.0 == StatusCode::CREATED || r.0 == StatusCode::OK, "{r:?}");
    }
    let originals = [a, b]
        .iter()
        .filter(|r| r.1["replayed"] == Value::Bool(false))
        .count();
    assert_eq!(originals, 1, "exactly one original execution: {a:?} {b:?}");
    for f in fields {
        assert_eq!(a.1[*f], b.1[*f], "{f} differs across replicas: {a:?} {b:?}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn two_replicas_share_one_key_source_and_replay_identically_across_rotation() {
    let jwks = jwks_server(JWKS_INITIAL).await;
    let a = harness_full(
        dev(),
        ApiLimits::default(),
        oidc(&jwks.url),
        DbSessionLimits::default(),
    )
    .await;
    let b = harness_full(
        dev(),
        ApiLimits::default(),
        oidc(&jwks.url),
        DbSessionLimits::default(),
    )
    .await;
    let g = a.graph("t1").await;
    let proposals = format!("/v1/graphs/{g}/proposals");
    let refs = format!("/v1/graphs/{g}/refs?name=main");
    let t_a = rs256("kid-a", RSA_A, &oidc_claims(300, -5));

    // 1. Concurrent identical prepare and accept landing on different replicas.
    let body = prepare_body(None, &[("add", "<urn:s> <urn:p> \"1\" .")], "m");
    let (pa, pb) = tokio::join!(
        a.call(
            "POST",
            &proposals,
            Some(&t_a),
            Some("mr-p1"),
            Some(body.clone())
        ),
        b.call(
            "POST",
            &proposals,
            Some(&t_a),
            Some("mr-p1"),
            Some(body.clone())
        )
    );
    assert_replayed_pair(
        &pa,
        &pb,
        &[
            "proposal_id",
            "candidate",
            "requested_patch",
            "effective_patch",
        ],
    );
    let candidate = pa.1["candidate"].as_str().unwrap().to_owned();
    assert_eq!(
        a.count("proposals", &g).await,
        1,
        "one durable proposal for the pair"
    );
    let accept = format!("{proposals}/{candidate}/accept");
    let accept_body = json!({"ref": "main", "expected_head": null, "reason": "multi-replica"});
    let (aa, ab) = tokio::join!(
        a.call(
            "POST",
            &accept,
            Some(&t_a),
            Some("mr-a1"),
            Some(accept_body.clone())
        ),
        b.call(
            "POST",
            &accept,
            Some(&t_a),
            Some("mr-a1"),
            Some(accept_body.clone())
        )
    );
    assert_replayed_pair(
        &aa,
        &ab,
        &[
            "decision_id",
            "ref_event_id",
            "outbox_id",
            "ref_version",
            "head",
        ],
    );
    assert_eq!(aa.1["ref_version"], 1);
    assert_eq!(
        a.count("ref_events", &g).await,
        1,
        "the ref moved exactly once"
    );
    assert_eq!(a.count("projection_outbox", &g).await, 1);

    // 2. Rotation: a new `kid` appears at the issuer; each replica picks it up on first
    //    sight (unknown kid → refresh) without a restart or any shared state between them.
    *jwks.document.lock().unwrap() = JWKS_ROTATED.to_owned();
    let t_c = rs256("kid-c", RSA_C, &oidc_claims(300, -5));
    let hits = jwks.hits.load(Ordering::SeqCst);
    for h in [&a, &b] {
        let (status, r, _) = h.call("GET", &refs, Some(&t_c), None, None).await;
        assert_eq!(status, StatusCode::OK, "{r:?}");
        assert_eq!(r["head"], candidate);
    }
    assert!(
        jwks.hits.load(Ordering::SeqCst) >= hits + 2,
        "each replica fetched the rotated document"
    );

    // 3. Withdrawal: the issuer drops kid-a. A refresh (forced here by an unknown kid) makes
    //    the withdrawn key stop validating on each replica; kid-c keeps working.
    let mut only_c: Value = serde_json::from_str(JWKS_ROTATED).unwrap();
    only_c["keys"] = Value::Array(
        only_c["keys"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|k| k["kid"] == "kid-c")
            .cloned()
            .collect(),
    );
    *jwks.document.lock().unwrap() = only_c.to_string();
    let t_unknown = rs256("kid-zzz", RSA_A, &oidc_claims(300, -5));
    for h in [&a, &b] {
        assert_error(
            &h.call("GET", &refs, Some(&t_unknown), None, None).await,
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );
        assert_error(
            &h.call("GET", &refs, Some(&t_a), None, None).await,
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );
        let (status, _, _) = h.call("GET", &refs, Some(&t_c), None, None).await;
        assert_eq!(status, StatusCode::OK);
    }

    // 4. Expiry boundaries (leeway is 30 s): inside the leeway is accepted, beyond it is
    //    refused, identically on both replicas; same for `nbf`.
    for h in [&a, &b] {
        for (exp, nbf, ok) in [
            (-10, -5, true),
            (-25, -5, true),
            (-35, -5, false),
            (-60, -5, false),
            (300, 10, true),
            (300, 60, false),
            (300, 25, true),
            (300, 35, false),
        ] {
            let t = rs256("kid-c", RSA_C, &oidc_claims(exp, nbf));
            let r = h.call("GET", &refs, Some(&t), None, None).await;
            if ok {
                assert_eq!(r.0, StatusCode::OK, "exp{exp} nbf{nbf}: {r:?}");
            } else {
                assert_error(&r, StatusCode::UNAUTHORIZED, "UNAUTHENTICATED");
            }
        }
    }

    // 5. Idempotency is bound to the complete actor, not to the token: the step-1 accept
    //    retried after rotation, with the new key, on the other replica, replays.
    let (status, r, _) = b
        .call(
            "POST",
            &accept,
            Some(&t_c),
            Some("mr-a1"),
            Some(accept_body),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{r:?}");
    assert_eq!(r["replayed"], true);
    assert_eq!(r["decision_id"], aa.1["decision_id"]);
    assert_eq!(a.count("ref_events", &g).await, 1);

    // 6. Production refresh policy (60 s minimum interval, 1 h max key age) with an
    //    injectable clock: a token under a `kid` published *after* the last fetch is refused
    //    while the throttle holds and accepted once the interval has elapsed. This is the
    //    documented rotation latency: issuers publish keys ahead of use (Entra: days), so
    //    an unknown `kid` at request time is a rotation the ledger has not fetched yet.
    let now = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let clock_now = now.clone();
    let c_auth: SharedAuthenticator = Arc::new(
        OidcAuthenticator::new(
            OIDC_ISS.into(),
            OIDC_AUD.into(),
            jwks.url.clone(),
            ClaimsPolicy::default(),
        )
        .with_clock(Arc::new(move || *clock_now.lock().unwrap())),
    );
    let c = harness_full(
        dev(),
        ApiLimits::default(),
        c_auth,
        DbSessionLimits::default(),
    )
    .await;
    let (status, _, _) = c.call("GET", &refs, Some(&t_c), None, None).await;
    assert_eq!(status, StatusCode::OK, "first fetch loads kid-c");
    *jwks.document.lock().unwrap() = JWKS_ROTATED.to_owned();
    let t_a2 = rs256("kid-a", RSA_A, &oidc_claims(300, -5));
    assert_error(
        &c.call("GET", &refs, Some(&t_a2), None, None).await,
        StatusCode::UNAUTHORIZED,
        "UNAUTHENTICATED",
    );
    *now.lock().unwrap() += Duration::from_secs(61);
    let (status, _, _) = c.call("GET", &refs, Some(&t_a2), None, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "after the refresh interval the new kid is fetched"
    );
}

// =========================================================================================
// Plan 0005 §9 — adversarial resource limits: admission control under a slow database,
// edge timeout, slow-loris body, body-size boundary, pool health
// =========================================================================================

/// Runtime-role sessions of *this* database currently waiting on a heavyweight lock
/// (observability barrier; the test runs on a throwaway database so nothing else counts).
async fn runtime_sessions_waiting(owner: &PostgresLedgerStore) -> i64 {
    sqlx::query(
        "SELECT count(*) AS n FROM pg_stat_activity \
         WHERE usename = 'ledger_rt_api' AND datname = current_database() \
           AND wait_event_type = 'Lock'",
    )
    .fetch_one(owner.pool())
    .await
    .unwrap()
    .get::<i64, _>("n")
}

async fn wait_until(mut condition: impl AsyncFnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !condition().await {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|p| p.parse::<u64>().ok())
        })
        .map(|pages| pages * 4)
        .unwrap_or(0)
}

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn expensive_operations_are_admission_controlled_under_a_slow_database() {
    // Two expensive slots; the database side gives up on a lock after 2 s (→ 503
    // DEPENDENCY_TIMEOUT), well inside the edge timeout.
    let limits = ApiLimits {
        max_concurrent_expensive: 2,
        request_timeout: Duration::from_secs(20),
        ..ApiLimits::default()
    };
    let session = DbSessionLimits {
        lock_timeout: Duration::from_secs(2),
        ..DbSessionLimits::default()
    };
    let auth: SharedAuthenticator = Arc::new(
        DevHs256Authenticator::new(
            ISSUER.into(),
            AUDIENCE.into(),
            SECRET,
            ClaimsPolicy::default(),
        )
        .unwrap(),
    );
    // A table lock disturbs every session of a database: use a throwaway one.
    let url = fresh_database("api_saturation").await;
    let h = harness_in(&url, dev(), limits, auth, session).await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let head = commit(&h, &g, &t, "<urn:a> <urn:p> \"1\" .").await;
    let state_path = format!("/v1/graphs/{g}/commits/{head}/state");
    let refs = format!("/v1/graphs/{g}/refs?name=main");
    let rss_before = rss_kib();

    // The owner makes every object read wait: the runtime cannot read immutable_objects
    // until this transaction ends (a slow/blocked database, not a slow authenticator).
    let mut blocker = h.owner.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE immutable_objects IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    // Two expensive reads take both slots and block inside PostgreSQL.
    let hold1 = tokio::spawn({
        let (h, p, t) = (h.app.clone(), state_path.clone(), t.clone());
        async move { call_app(h, "GET", &p, Some(&t)).await }
    });
    let hold2 = tokio::spawn({
        let (h, p, t) = (h.app.clone(), state_path.clone(), t.clone());
        async move { call_app(h, "GET", &p, Some(&t)).await }
    });
    wait_until(
        async || runtime_sessions_waiting(&h.owner).await >= 2,
        "two runtime sessions blocked on the table lock",
    )
    .await;
    // Saturated: the third expensive request is refused immediately with a stable code,
    // for reads and for prepare alike; cheap paths keep working.
    let r = h.call("GET", &state_path, Some(&t), None, None).await;
    assert_error(&r, StatusCode::SERVICE_UNAVAILABLE, "RESOURCE_LIMIT");
    assert!(
        r.1["message"].as_str().unwrap().contains("concurrent"),
        "{r:?}"
    );
    let refused_at = std::time::Instant::now();
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals"),
            Some(&t),
            Some("saturated"),
            Some(prepare_body(
                Some(&head),
                &[("add", "<urn:a> <urn:p> \"2\" .")],
                "m",
            )),
        )
        .await;
    assert_error(&r, StatusCode::SERVICE_UNAVAILABLE, "RESOURCE_LIMIT");
    assert!(
        r.1["message"].as_str().unwrap().contains("concurrent"),
        "{r:?}"
    );
    assert!(
        refused_at.elapsed() < Duration::from_secs(1),
        "refusal must be immediate, took {:?}",
        refused_at.elapsed()
    );
    let (status, r, _) = h.call("GET", &refs, Some(&t), None, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "cheap ref read under saturation: {r:?}"
    );
    let (status, _, _) = h.call("GET", "/ready", None, None, None).await;
    assert_eq!(status, StatusCode::OK, "readiness under saturation");
    // Pool bound at the peak of the episode: the runtime never opens more sessions than
    // its configured pool, even with two blocked expensive reads plus cheap traffic.
    let pool_max = i64::from(DbSessionLimits::default().max_connections);
    let peak: i64 = sqlx::query(
        "SELECT count(*) AS n FROM pg_stat_activity \
         WHERE usename = 'ledger_rt_api' AND datname = current_database()",
    )
    .fetch_one(h.owner.pool())
    .await
    .unwrap()
    .get("n");
    assert!(
        peak <= pool_max,
        "runtime sessions at peak {peak} > pool {pool_max}"
    );
    // The blocked reads end with the database's own timeout, classified as a dependency
    // timeout (retryable), never as success or as an internal error.
    for held in [hold1, hold2] {
        let r = held.await.unwrap();
        assert_error(&r, StatusCode::SERVICE_UNAVAILABLE, "DEPENDENCY_TIMEOUT");
    }
    // Slots are released with the failed requests: capacity is back while the lock is
    // still held (the next expensive request blocks again instead of being refused).
    let hold3 = tokio::spawn({
        let (h, p, t) = (h.app.clone(), state_path.clone(), t.clone());
        async move { call_app(h, "GET", &p, Some(&t)).await }
    });
    wait_until(
        async || runtime_sessions_waiting(&h.owner).await >= 1,
        "a runtime session blocked again",
    )
    .await;
    blocker.rollback().await.unwrap();
    let r = hold3.await.unwrap();
    assert_eq!(r.0, StatusCode::OK, "released: {r:?}");
    assert_eq!(r.1["quads"].as_array().unwrap().len(), 1);
    // Pool and memory health over the episode: sessions never exceed the pool, later
    // requests all succeed, resident memory did not balloon.
    let sessions: i64 =
        sqlx::query("SELECT count(*) AS n FROM pg_stat_activity WHERE usename = 'ledger_rt_api' AND datname = current_database()")
            .fetch_one(h.owner.pool())
            .await
            .unwrap()
            .get("n");
    assert!(
        sessions <= pool_max,
        "runtime sessions after the episode {sessions} > pool {pool_max}"
    );
    for _ in 0..20 {
        let (status, _, _) = h.call("GET", &state_path, Some(&t), None, None).await;
        assert_eq!(status, StatusCode::OK);
    }
    let rss_after = rss_kib();
    println!("rss before {rss_before} KiB, after {rss_after} KiB");
    // Whole-process RSS (other tests run in parallel in this binary): recorded as a
    // measurement only; the qualification run's memory evidence is the stress harness.
    let _ = (rss_before, rss_after);
}

async fn call_app(app: Router, method: &str, path: &str, bearer: Option<&str>) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(bearer) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
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

#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn edge_timeout_slow_loris_and_body_boundary_are_bounded() {
    // Edge timeout under a slow database: the database waits longer than the request is
    // allowed to take, so the edge answers RESOURCE_LIMIT and the client is never left
    // hanging (the store's own timeout is 10 s here, longer than the edge's 1 s).
    let limits = ApiLimits {
        body_bytes: 600,
        request_timeout: Duration::from_secs(1),
        ..ApiLimits::default()
    };
    let session = DbSessionLimits {
        lock_timeout: Duration::from_secs(10),
        ..DbSessionLimits::default()
    };
    let auth: SharedAuthenticator = Arc::new(
        DevHs256Authenticator::new(
            ISSUER.into(),
            AUDIENCE.into(),
            SECRET,
            ClaimsPolicy::default(),
        )
        .unwrap(),
    );
    let url = fresh_database("api_edge").await;
    let h = harness_in(&url, dev(), limits, auth, session).await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let head = commit(&h, &g, &t, "<urn:a> <urn:p> \"1\" .").await;
    let state_path = format!("/v1/graphs/{g}/commits/{head}/state");
    let mut blocker = h.owner.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE immutable_objects IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let r = h.call("GET", &state_path, Some(&t), None, None).await;
    assert_error(&r, StatusCode::SERVICE_UNAVAILABLE, "RESOURCE_LIMIT");
    assert!(
        r.1["message"].as_str().unwrap().contains("time limit"),
        "{r:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "edge timeout fired at {:?}",
        started.elapsed()
    );
    blocker.rollback().await.unwrap();
    // The abandoned query finishes once the lock is gone; wait for the pool to be quiet
    // before timing further requests (observability, not sleep).
    wait_until(
        async || runtime_sessions_waiting(&h.owner).await == 0,
        "no runtime session left waiting",
    )
    .await;

    // Slow-loris body: one byte every 100 ms, never finishing. The edge timeout covers body
    // reading, so the request ends with RESOURCE_LIMIT after ~1 s instead of holding a
    // connection open indefinitely.
    let proposals = format!("/v1/graphs/{g}/proposals");
    let drip = futures_util::stream::unfold(0u32, |i| async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Some((
            Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b" ")),
            i + 1,
        ))
    });
    let request = Request::builder()
        .method("POST")
        .uri(&proposals)
        .header(header::AUTHORIZATION, format!("Bearer {t}"))
        .header("idempotency-key", "loris")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(drip))
        .unwrap();
    let started = std::time::Instant::now();
    let r = h.raw(request).await;
    assert_error(&r, StatusCode::SERVICE_UNAVAILABLE, "RESOURCE_LIMIT");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "slow-loris cut at {:?}",
        started.elapsed()
    );

    // Body-size boundary: exactly `body_bytes` is accepted, one byte more is refused with the
    // stable code before any parsing.
    let core = prepare_body(Some(&head), &[("add", "<urn:a> <urn:p> \"2\" .")], "m").to_string();
    assert!(
        core.len() < 600,
        "fixture must fit under the limit: {}",
        core.len()
    );
    let exact = format!("{}{core}", " ".repeat(600 - core.len()));
    assert_eq!(exact.len(), 600);
    let r = h
        .call_raw("POST", &proposals, Some(&t), Some("exact"), Some(exact))
        .await;
    assert_eq!(r.0, StatusCode::CREATED, "exactly at the limit: {r:?}");
    let over = format!("{}{core}", " ".repeat(601 - core.len()));
    let r = h
        .call_raw("POST", &proposals, Some(&t), Some("over"), Some(over))
        .await;
    assert_error(&r, StatusCode::PAYLOAD_TOO_LARGE, "RESOURCE_LIMIT");
    assert!(
        r.1["message"].as_str().unwrap().contains("request body"),
        "{r:?}"
    );
    // Nothing leaked from the refused attempts; the accepted one is a proposal.
    assert_eq!(h.count("proposals", &g).await, 2);
}

// =============================================================================================
// Plan 0013 M1 — request lifecycle over HTTP (pool exhaustion, admission, timeout outcome).
// Classification per test (PRESERVATION / CHARACTERIZATION / FUTURE ACCEPTANCE `future_`);
// the `future_` tests are excluded from the suite run (`--skip future_`) and run once for the
// red evidence. Prefix `p7a_` selects the suite.
// =============================================================================================

fn p7a_auth() -> SharedAuthenticator {
    Arc::new(
        DevHs256Authenticator::new(
            ISSUER.into(),
            AUDIENCE.into(),
            SECRET,
            ClaimsPolicy::default(),
        )
        .unwrap(),
    )
}

/// Prepare one candidate onto the current head over HTTP; `(expected_head, candidate)`.
async fn p7a_prepare(
    h: &Harness,
    g: &GraphId,
    t: &str,
    quad: &str,
    key: &str,
) -> (Option<String>, String) {
    let (_, head, _) = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/refs?name=main"),
            Some(t),
            None,
            None,
        )
        .await;
    let expected = head.get("head").and_then(Value::as_str).map(str::to_owned);
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals"),
            Some(t),
            Some(key),
            Some(prepare_body(expected.as_deref(), &[("add", quad)], "m")),
        )
        .await;
    assert_eq!(r.0, StatusCode::CREATED, "{r:?}");
    (expected, r.1["candidate"].as_str().unwrap().to_owned())
}

fn p7a_accept_body(expected: &Option<String>) -> Value {
    json!({"ref": "main", "expected_head": expected, "reason": "ok"})
}

/// A request issued on a detached task (so several can be in flight at once), answered as a
/// `Reply`.
fn p7a_spawn(
    h: &Harness,
    method: &'static str,
    path: String,
    t: &str,
    key: Option<String>,
    body: Option<Value>,
) -> tokio::task::JoinHandle<Reply> {
    let (app, t) = (h.app.clone(), t.to_owned());
    tokio::spawn(async move {
        let mut request = Request::builder()
            .method(method)
            .uri(&path)
            .header(header::AUTHORIZATION, format!("Bearer {t}"));
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
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let correlation = response
            .headers()
            .get("x-correlation-id")
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value, correlation)
    })
}

/// CHARACTERIZATION (F3, F4). With every pooled connection held elsewhere, a cheap read,
/// readiness, a prepare and an accept all wait the hard-coded 10 s pool acquire timeout and
/// then answer 503 `DEPENDENCY_UNAVAILABLE`; the read's message tells a key-less caller to
/// retry with an idempotency key. Nothing is written; once a connection is free everything
/// answers normally. (ADR-0026 bounds the wait by the request deadline and gives reads their
/// own guidance; M2 changes the envelope, M3 the headroom.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn p7a_pool_exhaustion_waits_the_acquire_timeout_then_fails_every_request_class_alike() {
    let limits = ApiLimits {
        request_timeout: Duration::from_secs(30),
        ..ApiLimits::default()
    };
    let session = DbSessionLimits {
        max_connections: 2,
        ..DbSessionLimits::default()
    };
    let h = harness_full(dev(), limits, p7a_auth(), session).await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let head = commit(&h, &g, &t, "<urn:a> <urn:p> \"1\" .").await;
    let (expected, candidate) =
        p7a_prepare(&h, &g, &t, "<urn:a> <urn:p> \"2\" .", "exh-prep").await;
    assert_eq!(expected.as_deref(), Some(head.as_str()));
    // Every connection of the served store is held.
    let hold_a = h.store.pool().acquire().await.unwrap();
    let hold_b = h.store.pool().acquire().await.unwrap();
    let refs = format!("/v1/graphs/{g}/refs?name=main");
    let proposals = format!("/v1/graphs/{g}/proposals");
    let accept_path = format!("/v1/graphs/{g}/proposals/{candidate}/accept");
    let started = std::time::Instant::now();
    let (read, ready, prepare, accept) = tokio::join!(
        h.call("GET", &refs, Some(&t), None, None),
        h.call("GET", "/ready", None, None, None),
        h.call(
            "POST",
            &proposals,
            Some(&t),
            Some("exh-prep-2"),
            Some(prepare_body(
                Some(&head),
                &[("add", "<urn:a> <urn:p> \"3\" .")],
                "m"
            )),
        ),
        h.call(
            "POST",
            &accept_path,
            Some(&t),
            Some("exh-accept"),
            Some(p7a_accept_body(&expected)),
        ),
    );
    let waited = started.elapsed();
    for (what, r) in [
        ("read", &read),
        ("ready", &ready),
        ("prepare", &prepare),
        ("accept", &accept),
    ] {
        assert_error(r, StatusCode::SERVICE_UNAVAILABLE, "DEPENDENCY_UNAVAILABLE");
        println!("{what}: {} {:?}", r.0, r.1["message"]);
    }
    // The key-less read is told to retry "with the same idempotency key" (F4).
    assert!(
        read.1["message"]
            .as_str()
            .unwrap()
            .contains("idempotency key"),
        "{read:?}"
    );
    assert!(
        waited >= Duration::from_secs(9) && waited < Duration::from_secs(20),
        "the hard-coded pool acquire timeout (10 s) governs the wait: {waited:?}"
    );
    println!("pool exhaustion: all four classes failed after {waited:?}");
    assert_eq!(
        h.count("idempotency", &g).await,
        3,
        "nothing was written by the refused requests"
    );
    drop(hold_a);
    drop(hold_b);
    let r = h.call("GET", &refs, Some(&t), None, None).await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    let r = h.call("GET", "/ready", None, None, None).await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    let r = h
        .call(
            "POST",
            &accept_path,
            Some(&t),
            Some("exh-accept"),
            Some(p7a_accept_body(&expected)),
        )
        .await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    assert_eq!(
        r.1["replayed"], false,
        "the refused accept never ran: the retry executes"
    );
}

/// Session limits for the saturation scenarios: the blocked accepts must outlive the probes
/// (a read and a readiness check may each wait the 10 s pool timeout), so the lock wait and
/// the statement are allowed 40 s / 60 s.
fn p7a_saturation_session(max_connections: u32, lock_timeout: Duration) -> DbSessionLimits {
    DbSessionLimits {
        max_connections,
        lock_timeout,
        statement_timeout: Duration::from_secs(60),
        ..DbSessionLimits::default()
    }
}

/// An owner transaction holding `main`'s ref row `FOR UPDATE`: every accept blocks on its
/// `SELECT … FOR UPDATE` inside PostgreSQL, while plain reads of the row (the refs route)
/// are not blocked — only connections are.
async fn p7a_hold_main(h: &Harness, g: &GraphId) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut blocker = h.owner.pool().begin().await.unwrap();
    sqlx::query("SELECT head FROM refs WHERE graph_id = $1 AND branch = 'main' FOR UPDATE")
        .bind(g.as_str())
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    blocker
}

/// CHARACTERIZATION (F3). Accept takes no admission permit, so three accepts blocked inside
/// PostgreSQL (on `main`'s row lock) occupy a three-connection pool entirely; the next cheap
/// read (which the row lock does not block) and readiness probe find no connection, wait the
/// 10 s acquire timeout and fail 503 `DEPENDENCY_UNAVAILABLE` — there is no reserved headroom
/// for cheap traffic. Once the blocker is gone, exactly one accept lands and the other two
/// are `HEAD_CHANGED`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn p7a_heavy_writes_without_admission_take_every_connection_and_cheap_reads_starve() {
    let limits = ApiLimits {
        request_timeout: Duration::from_secs(60),
        ..ApiLimits::default()
    };
    let url = fresh_database("api_p7a_sat").await;
    let h = harness_in(
        &url,
        dev(),
        limits,
        p7a_auth(),
        p7a_saturation_session(3, Duration::from_secs(40)),
    )
    .await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let head = commit(&h, &g, &t, "<urn:a> <urn:p> \"1\" .").await;
    let mut candidates = Vec::new();
    for i in 0..3 {
        let (expected, c) = p7a_prepare(
            &h,
            &g,
            &t,
            &format!("<urn:a> <urn:p> \"{}\" .", i + 2),
            &format!("sat-prep-{i}"),
        )
        .await;
        assert_eq!(expected.as_deref(), Some(head.as_str()));
        candidates.push(c);
    }
    let blocker = p7a_hold_main(&h, &g).await;
    let accepts: Vec<_> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            p7a_spawn(
                &h,
                "POST",
                format!("/v1/graphs/{g}/proposals/{c}/accept"),
                &t,
                Some(format!("sat-accept-{i}")),
                Some(p7a_accept_body(&Some(head.clone()))),
            )
        })
        .collect();
    wait_until(
        async || runtime_sessions_waiting(&h.owner).await >= 3,
        "three accepts blocked inside PostgreSQL",
    )
    .await;
    let started = std::time::Instant::now();
    let read = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/refs?name=main"),
            Some(&t),
            None,
            None,
        )
        .await;
    let read_waited = started.elapsed();
    let ready = h.call("GET", "/ready", None, None, None).await;
    assert_error(
        &read,
        StatusCode::SERVICE_UNAVAILABLE,
        "DEPENDENCY_UNAVAILABLE",
    );
    assert_error(
        &ready,
        StatusCode::SERVICE_UNAVAILABLE,
        "DEPENDENCY_UNAVAILABLE",
    );
    assert!(
        read_waited >= Duration::from_secs(9) && read_waited < Duration::from_secs(20),
        "the read starved for the pool acquire timeout: {read_waited:?}"
    );
    blocker.rollback().await.unwrap();
    let mut statuses = Vec::new();
    for a in accepts {
        statuses.push(a.await.unwrap().0);
    }
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::CONFLICT)
            .count(),
        2,
        "{statuses:?}"
    );
    println!("saturation: read starved {read_waited:?}; accepts {statuses:?}");
}

/// FUTURE ACCEPTANCE (Plan 0013 M3, ADR-0026 §5 admission model). Pool N = 3, reserved cheap
/// headroom 2, so one DB-work permit (`expensive` and `validations` are 1 each, as the start-up
/// invariants require for N = 3). Sequence, order-sensitive by construction:
/// 1. one accept is admitted and blocks inside PostgreSQL on `main`'s row lock;
/// 2. `graphs` is then locked exclusively, so *any* pooled query touching it would block;
/// 3. three more heavy requests of different classes (accept, prepare, state read) must be
///    refused at once with 503 `RESOURCE_LIMIT` — before `authorized_graph`, hence without
///    touching `graphs`; a permit taken after the graph lookup would block here instead;
/// 4. with `graphs` released and the first accept still blocked, the cheap read and `/ready`
///    answer 200 promptly on the reserved connections;
/// 5. with the row lock released, the admitted accept lands; nothing else was written.
/// Today accept takes no permit and the followers block on `graphs` until `lock_timeout`
/// (6 s here) → `DEPENDENCY_TIMEOUT`, so this fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FUTURE ACCEPTANCE (Plan 0013 M3): requires PostgreSQL and the admission model"]
async fn future_p7a_reserved_headroom_keeps_reads_and_readiness_answering_while_heavy_writes_saturate()
 {
    let limits = ApiLimits {
        request_timeout: Duration::from_secs(60),
        max_concurrent_expensive: 1,
        max_concurrent_validations: 1,
        ..ApiLimits::default()
    };
    let url = fresh_database("api_p7a_headroom").await;
    let h = harness_in(
        &url,
        dev(),
        limits,
        p7a_auth(),
        p7a_saturation_session(3, Duration::from_secs(6)),
    )
    .await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let head = commit(&h, &g, &t, "<urn:a> <urn:p> \"1\" .").await;
    let (_, c0) = p7a_prepare(&h, &g, &t, "<urn:a> <urn:p> \"2\" .", "hr-prep-0").await;
    let (_, c1) = p7a_prepare(&h, &g, &t, "<urn:a> <urn:p> \"3\" .", "hr-prep-1").await;
    let written_before = h.count("idempotency", &g).await;
    // 1. the admitted accept blocks on the row lock
    let blocker = p7a_hold_main(&h, &g).await;
    let admitted = p7a_spawn(
        &h,
        "POST",
        format!("/v1/graphs/{g}/proposals/{c0}/accept"),
        &t,
        Some("hr-accept-0".into()),
        Some(p7a_accept_body(&Some(head.clone()))),
    );
    wait_until(
        async || runtime_sessions_waiting(&h.owner).await >= 1,
        "the admitted accept blocked inside PostgreSQL",
    )
    .await;
    // 2. graphs locked: anything that looks a graph up now blocks
    let mut graphs_lock = h.owner.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE graphs IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *graphs_lock)
        .await
        .unwrap();
    // 3. followers of three heavy classes: refused by admission, never reaching the pool
    let started = std::time::Instant::now();
    let followers = [
        p7a_spawn(
            &h,
            "POST",
            format!("/v1/graphs/{g}/proposals/{c1}/accept"),
            &t,
            Some("hr-accept-1".into()),
            Some(p7a_accept_body(&Some(head.clone()))),
        ),
        p7a_spawn(
            &h,
            "POST",
            format!("/v1/graphs/{g}/proposals"),
            &t,
            Some("hr-prep-2".into()),
            Some(prepare_body(
                Some(&head),
                &[("add", "<urn:a> <urn:p> \"4\" .")],
                "m",
            )),
        ),
        p7a_spawn(
            &h,
            "GET",
            format!("/v1/graphs/{g}/commits/{head}/state"),
            &t,
            None,
            None,
        ),
    ];
    for f in followers {
        let r = f.await.unwrap();
        assert_error(&r, StatusCode::SERVICE_UNAVAILABLE, "RESOURCE_LIMIT");
    }
    let refused_in = started.elapsed();
    assert!(
        refused_in < Duration::from_millis(1500),
        "admission refuses without touching the database: {refused_in:?}"
    );
    graphs_lock.rollback().await.unwrap();
    // 4. cheap traffic answers on the reserved headroom while the heavy one is still blocked
    assert!(runtime_sessions_waiting(&h.owner).await >= 1);
    let started = std::time::Instant::now();
    let read = h
        .call(
            "GET",
            &format!("/v1/graphs/{g}/refs?name=main"),
            Some(&t),
            None,
            None,
        )
        .await;
    let ready = h.call("GET", "/ready", None, None, None).await;
    let cheap_took = started.elapsed();
    assert_eq!(read.0, StatusCode::OK, "{read:?}");
    assert_eq!(ready.0, StatusCode::OK, "{ready:?}");
    assert!(cheap_took < Duration::from_secs(2), "{cheap_took:?}");
    // 5. the admitted accept lands once the row lock is gone
    blocker.rollback().await.unwrap();
    let r = admitted.await.unwrap();
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    assert_eq!(
        h.count("idempotency", &g).await,
        written_before + 1,
        "only the admitted accept wrote"
    );
}

/// PRESERVATION (premise of the Plan 0013 admission model, HTTP level). Every heavy route
/// completes on a served store with a one-connection pool, including the handler-level steps
/// (`authorized_graph`, membership lookups) around the store calls: prepare, accept, state
/// read, branch creation from a historical commit, delete, restore, merge preview / propose /
/// apply, refs, branch status, history and log. (Validations need a configured validator; the
/// validate route's two transactions are sequential by code and are listed for M2.)
#[tokio::test]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn p7a_every_heavy_route_completes_on_a_one_connection_pool() {
    let session = DbSessionLimits {
        max_connections: 1,
        ..DbSessionLimits::default()
    };
    let h = harness_full(dev(), ApiLimits::default(), p7a_auth(), session).await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let admin = token(
        "tenant-a",
        "actor",
        &[
            "ledger.read",
            "ledger.propose",
            "ledger.review",
            "ledger.admin",
        ],
    );
    let c1 = commit(&h, &g, &t, "<urn:a> <urn:p> \"1\" .").await;
    let c2 = commit(&h, &g, &t, "<urn:b> <urn:p> \"1\" .").await;
    let ok = |r: &Reply, what: &str| assert!(r.0.is_success(), "{what}: {r:?}");
    ok(
        &h.call(
            "GET",
            &format!("/v1/graphs/{g}/refs?name=main"),
            Some(&t),
            None,
            None,
        )
        .await,
        "refs",
    );
    ok(
        &h.call(
            "GET",
            &format!("/v1/graphs/{g}/commits/{c2}/state"),
            Some(&t),
            None,
            None,
        )
        .await,
        "state",
    );
    ok(
        &h.call(
            "GET",
            &format!("/v1/graphs/{g}/branches/history?name=main&limit=10"),
            Some(&t),
            None,
            None,
        )
        .await,
        "history",
    );
    ok(
        &h.call(
            "GET",
            &format!("/v1/graphs/{g}/branches/log?name=main&limit=10"),
            Some(&t),
            None,
            None,
        )
        .await,
        "log",
    );
    // Branch from the historical genesis: the reachability walk precedes the transaction.
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/branches"),
            Some(&t),
            Some("one-branch"),
            Some(json!({"name": "agent/one", "source": "main", "from_commit": c1})),
        )
        .await;
    assert_eq!(r.0, StatusCode::CREATED, "{r:?}");
    ok(
        &h.call(
            "GET",
            &format!("/v1/graphs/{g}/branches/status?name=agent/one"),
            Some(&t),
            None,
            None,
        )
        .await,
        "status",
    );
    // Diverge the branch, then merge it back: preview, propose, apply.
    let body = json!({"ref": "agent/one", "expected_head": c1, "operations": [{"op": "add", "quad": "<urn:c> <urn:p> \"1\" ."}], "activity": "cognitive-correction", "message": "branch"});
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals"),
            Some(&t),
            Some("one-bp"),
            Some(body),
        )
        .await;
    assert_eq!(r.0, StatusCode::CREATED, "{r:?}");
    let bc = r.1["candidate"].as_str().unwrap().to_owned();
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/proposals/{bc}/accept"),
            Some(&t),
            Some("one-ba"),
            Some(json!({"ref": "agent/one", "expected_head": c1, "reason": "ok"})),
        )
        .await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/merges/preview"),
            Some(&t),
            None,
            Some(json!({"source": "agent/one", "target": "main"})),
        )
        .await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    let token_ = r.1["preview_token"].as_str().unwrap().to_owned();
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/merges/propose"),
            Some(&t),
            Some("one-mp"),
            Some(json!({"source": "agent/one", "target": "main", "preview_token": token_, "message": "integrate"})),
        )
        .await;
    assert_eq!(r.0, StatusCode::CREATED, "{r:?}");
    let proposal_id = r.1["proposal_id"].clone();
    let reviewer = token("tenant-a", "reviewer", &ALL);
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/merges/apply"),
            Some(&reviewer),
            Some("one-ma"),
            Some(json!({"proposal_id": proposal_id, "preview_token": token_, "reason": "ok"})),
        )
        .await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/branches/delete"),
            Some(&admin),
            Some("one-bd"),
            Some(json!({"name": "agent/one", "reason": "done"})),
        )
        .await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    let r = h
        .call(
            "POST",
            &format!("/v1/graphs/{g}/branches/restore"),
            Some(&admin),
            Some("one-br"),
            Some(json!({"name": "agent/one"})),
        )
        .await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    ok(&h.call("GET", "/ready", None, None, None).await, "ready");
}

/// A harness whose served store pauses every workflow transaction right after `COMMIT`
/// (the response has not been built), with a 1 s edge timeout: the edge fires while the
/// outcome is durable.
async fn p7a_after_commit_harness() -> (Harness, ledger_store::test_hooks::PauseHook) {
    use ledger_store::test_hooks::{HookPoint, PauseHook};
    let hook = PauseHook::new(HookPoint::AfterCommit);
    let limits = ApiLimits {
        request_timeout: Duration::from_secs(1),
        ..ApiLimits::default()
    };
    let h = harness_in_hooked(
        &database_url(),
        dev(),
        limits,
        p7a_auth(),
        DbSessionLimits::default(),
        Some(hook.clone()),
    )
    .await;
    (h, hook)
}

/// Genesis prepare over the after-COMMIT harness (resumed explicitly), then an accept that
/// pauses after its COMMIT and is dropped by the 1 s edge timeout. Returns the candidate, the
/// accept's path and the timeout reply. The accept is only sent once it has provably reached
/// COMMIT (`hook.reached()`), so a slow runner cannot turn this into a pre-COMMIT drop.
async fn p7a_lost_accept(
    h: &Harness,
    hook: &ledger_store::test_hooks::PauseHook,
    g: &GraphId,
    t: &str,
    prefix: &str,
) -> (String, String, Reply, Duration) {
    let prepare = p7a_spawn(
        h,
        "POST",
        format!("/v1/graphs/{g}/proposals"),
        t,
        Some(format!("{prefix}-prep")),
        Some(prepare_body(
            None,
            &[("add", "<urn:a> <urn:p> \"1\" .")],
            "m",
        )),
    );
    hook.reached().await;
    hook.resume();
    assert_eq!(prepare.await.unwrap().0, StatusCode::CREATED);
    let candidate: String =
        sqlx::query_scalar("SELECT candidate_commit FROM proposals WHERE graph_id = $1")
            .bind(g.as_str())
            .fetch_one(h.owner.pool())
            .await
            .unwrap();
    let accept_path = format!("/v1/graphs/{g}/proposals/{candidate}/accept");
    let started = std::time::Instant::now();
    let accept = p7a_spawn(
        h,
        "POST",
        accept_path.clone(),
        t,
        Some(format!("{prefix}-accept")),
        Some(p7a_accept_body(&None)),
    );
    // COMMIT has returned; the handler now waits at the hook until the edge timeout drops it.
    hook.reached().await;
    let reply = accept.await.unwrap();
    (candidate, accept_path, reply, started.elapsed())
}

/// CHARACTERIZATION (F4). An accept whose COMMIT succeeded but whose handler is dropped by
/// the edge timeout before responding is reported today as 503 `RESOURCE_LIMIT` "request
/// exceeded the configured time limit" — indistinguishable from an admission refusal where
/// nothing happened, and without any replay guidance — although the write is durable: the
/// same-key retry replays it. (M2 / ADR-0026 outcome model changes the envelope; the
/// durability and replay here are PRESERVATION and must not change.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL: run via scripts/test-integration.sh with LEDGER_TEST_DATABASE_URL"]
async fn p7a_an_edge_timeout_after_commit_is_reported_today_like_an_admission_refusal() {
    let (h, hook) = p7a_after_commit_harness().await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let (candidate, accept_path, r, took) = p7a_lost_accept(&h, &hook, &g, &t, "eto").await;
    assert_error(&r, StatusCode::SERVICE_UNAVAILABLE, "RESOURCE_LIMIT");
    assert!(
        r.1["message"].as_str().unwrap().contains("time limit"),
        "{r:?}"
    );
    assert!(
        !r.1["message"].as_str().unwrap().contains("idempotency"),
        "today: no replay guidance: {r:?}"
    );
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(5),
        "{took:?}"
    );
    println!(
        "edge timeout after COMMIT answered {} {:?} after {took:?}",
        r.0, r.1["code"]
    );
    // Durable regardless of the lost response.
    assert_eq!(h.count("ref_events", &g).await, 1);
    assert_eq!(h.count("idempotency", &g).await, 2);
    // The retry replays (a replay commits no write, so it never reaches the after-COMMIT hook).
    let r = h
        .call(
            "POST",
            &accept_path,
            Some(&t),
            Some("eto-accept"),
            Some(p7a_accept_body(&None)),
        )
        .await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    assert_eq!(r.1["replayed"], true, "{r:?}");
    assert_eq!(r.1["head"], candidate, "{r:?}");
    println!(
        "retry after the lost response: {} replayed={}",
        r.0, r.1["replayed"]
    );
}

/// FUTURE ACCEPTANCE (Plan 0013 M2, ADR-0026 outcome model, Design A). The same scenario must
/// answer with the request-timeout envelope for an idempotent write whose execution began:
/// 503 `REQUEST_TIMEOUT`, a message saying the outcome is unknown and that the same
/// idempotency key replays it, and the retry replays the durable result. Today the code is
/// `RESOURCE_LIMIT` and the message carries no guidance, so this fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FUTURE ACCEPTANCE (Plan 0013 M2): requires PostgreSQL and the outcome-classified timeout envelope"]
async fn future_p7a_an_edge_timeout_after_commit_reports_the_outcome_as_unknown_and_the_key_replays()
 {
    let (h, hook) = p7a_after_commit_harness().await;
    let g = h.graph("tenant-a").await;
    let t = token("tenant-a", "actor", &ALL);
    let (_, accept_path, r, _) = p7a_lost_accept(&h, &hook, &g, &t, "fto").await;
    assert_error(&r, StatusCode::SERVICE_UNAVAILABLE, "REQUEST_TIMEOUT");
    let message = r.1["message"].as_str().unwrap();
    assert!(
        message.contains("unknown") && message.contains("idempotency key"),
        "{r:?}"
    );
    let r = h
        .call(
            "POST",
            &accept_path,
            Some(&t),
            Some("fto-accept"),
            Some(p7a_accept_body(&None)),
        )
        .await;
    assert_eq!(r.0, StatusCode::OK, "{r:?}");
    assert_eq!(r.1["replayed"], true, "{r:?}");
}
