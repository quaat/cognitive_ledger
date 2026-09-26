//! Authenticated, graph-scoped HTTP adapter over the atomic workflow (ADR-0011/0013).
//! It exposes no query language. Every public mutation requires a verified identity,
//! an `Idempotency-Key`, and is routed through `WorkflowRepository` with a
//! server-computed canonical request digest; every public read is graph-scoped and
//! bounded. Errors are a stable, redacted envelope carrying the request's correlation id.

pub mod auth;
pub mod request_identity;

use auth::{AuthError, Capability, SharedAuthenticator, VerifiedIdentity};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ledger_core::{CommitId, ContentId, GraphId, LedgerError, LedgerTimestamp};
use ledger_rdf::{Operation, OperationKind, Patch, Quad};
use ledger_store::{
    AcceptRequest, Ledger, MAX_BRANCH_BYTES, MAX_CORRELATION_BYTES, MAX_IDEMPOTENCY_KEY_BYTES,
    MAX_REASON_BYTES, PostgresLedgerStore, PrepareRequest, ReconstructionLimits, RejectRequest,
    RequestScope, ValidationPolicy,
};
use request_identity::CanonicalRequest;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tracing::{info, warn};

/// Operational limits below the untrusted boundary. Configurable per deployment.
#[derive(Clone, Copy, Debug)]
pub struct ApiLimits {
    pub body_bytes: usize,
    pub max_operations: usize,
    pub max_term_bytes: usize,
    /// activity + message + evidence refs + source system, in bytes.
    pub max_metadata_bytes: usize,
    pub reconstruction: ReconstructionLimits,
    /// Maximum bytes of N-Quads a state read may return.
    pub max_state_export_bytes: usize,
    pub request_timeout: Duration,
    pub max_concurrent_expensive: usize,
}

impl Default for ApiLimits {
    fn default() -> Self {
        Self {
            body_bytes: 2 * 1024 * 1024,
            max_operations: 10_000,
            max_term_bytes: 8 * 1024,
            max_metadata_bytes: 64 * 1024,
            reconstruction: ReconstructionLimits::DEVELOPMENT,
            max_state_export_bytes: 64 * 1024 * 1024,
            request_timeout: Duration::from_secs(30),
            // Below the store's 16-connection pool so cheap paths (auth, ref reads,
            // readiness) always find a connection while expensive work is saturated.
            max_concurrent_expensive: 12,
        }
    }
}

/// Whether accepting onto protected state without semantic validation is permitted.
/// Phase 2 supplies validation; until then the default is to refuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcceptancePolicy {
    /// Production default: `accept` fails with `VALIDATION_REQUIRED`.
    RequireValidation,
    /// Development/CI only (`LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only`).
    AllowUnvalidatedDevelopmentOnly,
}

struct Shared {
    store: PostgresLedgerStore,
    authenticator: SharedAuthenticator,
    limits: ApiLimits,
    acceptance: AcceptancePolicy,
    expensive: tokio::sync::Semaphore,
    correlation_counter: AtomicU64,
}

#[derive(Clone)]
pub struct AppState(Arc<Shared>);

impl AppState {
    pub fn new(
        store: PostgresLedgerStore,
        authenticator: SharedAuthenticator,
        limits: ApiLimits,
        acceptance: AcceptancePolicy,
    ) -> Self {
        if acceptance == AcceptancePolicy::AllowUnvalidatedDevelopmentOnly {
            warn!(
                "UNVALIDATED ACCEPTANCE ENABLED: proposals are accepted without semantic \
                 validation; this is a development/CI setting, never a production one"
            );
        }
        // One source of truth for reconstruction bounds: the workflow's base
        // reconstruction and public state reads use the same limits.
        let store = store.with_limits(limits.reconstruction);
        Self(Arc::new(Shared {
            expensive: tokio::sync::Semaphore::new(limits.max_concurrent_expensive),
            store,
            authenticator,
            limits,
            acceptance,
            correlation_counter: AtomicU64::new(0),
        }))
    }
}

/// The routes this adapter serves; the checked-in OpenAPI document is verified against it.
pub const ROUTES: &[(&str, &str)] = &[
    ("GET", "/health"),
    ("GET", "/ready"),
    ("GET", "/openapi.json"),
    ("POST", "/v1/graphs/{graph}/proposals"),
    ("POST", "/v1/graphs/{graph}/proposals/{candidate}/accept"),
    ("POST", "/v1/graphs/{graph}/proposals/{candidate}/reject"),
    ("GET", "/v1/graphs/{graph}/refs"),
    ("GET", "/v1/graphs/{graph}/commits/{commit}/state"),
];

pub const OPENAPI_JSON: &str = include_str!("../../../docs/api/openapi.json");

/// The authenticated, graph-scoped production router.
pub fn router(state: AppState) -> Router {
    let body_limit = state.0.limits.body_bytes;
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/openapi.json", get(openapi))
        .route("/v1/graphs/{graph}/proposals", post(prepare))
        .route(
            "/v1/graphs/{graph}/proposals/{candidate}/accept",
            post(accept),
        )
        .route(
            "/v1/graphs/{graph}/proposals/{candidate}/reject",
            post(reject),
        )
        .route("/v1/graphs/{graph}/refs", get(read_ref))
        .route("/v1/graphs/{graph}/commits/{commit}/state", get(read_state))
        .fallback(unknown_route)
        .method_not_allowed_fallback(unknown_route)
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            correlation_and_timeout,
        ))
        .with_state(state)
}

/// Filesystem development mode: read-only inspection of the single bootstrap ref. No
/// mutation surface exists here; it is loopback-only by the server's binding rules.
pub fn filesystem_readonly_router(ledger: Arc<Ledger>, limits: ReconstructionLimits) -> Router {
    #[derive(Clone)]
    struct FsState {
        ledger: Arc<Ledger>,
        limits: ReconstructionLimits,
    }
    async fn fs_head(State(s): State<FsState>) -> Result<Json<RefResponse>, ApiError> {
        let head = s
            .ledger
            .head()
            .await
            .map_err(|e| ApiError::from_ledger(e, ""))?;
        Ok(Json(RefResponse {
            name: "main".into(),
            head,
            version: None,
        }))
    }
    async fn fs_state(
        State(s): State<FsState>,
        Path(id): Path<String>,
    ) -> Result<Json<StateResponse>, ApiError> {
        let id = CommitId::from_str(&id).map_err(|_| ApiError::not_found(""))?;
        let quads = s
            .ledger
            .state_at_bounded(&id, &s.limits)
            .await
            .map_err(|e| ApiError::from_ledger(e, ""))?;
        Ok(Json(StateResponse {
            commit: id,
            quads: quads.into_iter().map(|q| q.to_string()).collect(),
        }))
    }
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(|| async { Json(Health { status: "ready" }) }))
        .route("/v1/refs/main", get(fs_head))
        .route("/v1/states/{id}", get(fs_state))
        .with_state(FsState { ledger, limits })
}

// ---------------------------------------------------------------------------------------
// Correlation, timeout, errors

const CORRELATION_HEADER: &str = "x-correlation-id";

#[derive(Clone)]
struct Correlation(String);

fn valid_correlation(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CORRELATION_BYTES
        && value.bytes().all(|b| b.is_ascii_graphic())
}

async fn correlation_and_timeout(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let supplied = request
        .headers()
        .get(CORRELATION_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| valid_correlation(v))
        .map(str::to_owned);
    let correlation = supplied.unwrap_or_else(|| {
        let n = state.0.correlation_counter.fetch_add(1, Ordering::Relaxed);
        let seed = format!(
            "{}:{n}:{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        );
        ContentId::for_bytes(seed.as_bytes()).digest_hex()[..32].to_owned()
    });
    request
        .extensions_mut()
        .insert(Correlation(correlation.clone()));
    let timeout = state.0.limits.request_timeout;
    let mut response = match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "RESOURCE_LIMIT",
            "request exceeded the configured time limit",
            &correlation,
        )
        .into_response(),
    };
    if let Ok(value) = HeaderValue::from_str(&correlation) {
        response.headers_mut().insert(CORRELATION_HEADER, value);
    }
    response
}

/// Unknown paths and methods answer with the envelope too (no framework default bodies).
async fn unknown_route(request: Request<Body>) -> ApiError {
    let correlation = request
        .extensions()
        .get::<Correlation>()
        .map(|c| c.0.clone())
        .unwrap_or_default();
    ApiError::not_found(&correlation)
}

/// The public error envelope. Codes are stable; messages are safe text only.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    correlation_id: String,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
    correlation_id: &'a str,
}

impl ApiError {
    fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
        correlation: &str,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            correlation_id: correlation.to_owned(),
        }
    }
    fn invalid(message: impl Into<String>, correlation: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            message,
            correlation,
        )
    }
    fn not_found(correlation: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "resource not found",
            correlation,
        )
    }
    fn resource_limit(message: impl Into<String>, correlation: &str) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "RESOURCE_LIMIT",
            message,
            correlation,
        )
    }
    fn from_auth(error: AuthError, correlation: &str) -> Self {
        match error {
            AuthError::Unauthenticated(reason) => {
                info!(correlation, reason, "authentication refused");
                Self::new(
                    StatusCode::UNAUTHORIZED,
                    "UNAUTHENTICATED",
                    "authentication required",
                    correlation,
                )
            }
            AuthError::KeySourceUnavailable(reason) => {
                warn!(correlation, reason, "authentication key source unavailable");
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "DEPENDENCY_UNAVAILABLE",
                    "authentication is temporarily unavailable",
                    correlation,
                )
            }
        }
    }
    /// Map a ledger error to the public envelope. Anything that could disclose foreign
    /// graphs, tenants, commit membership or storage internals is redacted; the full
    /// error is logged under the correlation id.
    fn from_ledger(error: LedgerError, correlation: &str) -> Self {
        use LedgerError as E;
        let (status, code, message): (StatusCode, &'static str, String) = match &error {
            E::HeadChanged { expected, actual } => (
                StatusCode::CONFLICT,
                "HEAD_CHANGED",
                format!(
                    "expected head {} but the ref is at {}",
                    expected
                        .as_ref()
                        .map_or("none".to_owned(), ToString::to_string),
                    actual
                        .as_ref()
                        .map_or("none".to_owned(), ToString::to_string)
                ),
            ),
            E::IdempotencyConflict => (
                StatusCode::CONFLICT,
                "IDEMPOTENCY_CONFLICT",
                "this idempotency key was already used with a different request".into(),
            ),
            E::LineageMismatch(_) => (
                StatusCode::CONFLICT,
                "LINEAGE_MISMATCH",
                "the candidate cannot be decided onto this ref from the given expected head".into(),
            ),
            E::BaseMismatch(quad) => (
                StatusCode::PRECONDITION_FAILED,
                "BASE_MISMATCH",
                format!("the request deletes a quad absent from the base state: {quad}"),
            ),
            E::NoEffectiveChange => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "NO_EFFECTIVE_CHANGE",
                "every requested operation is a no-op against the base state".into(),
            ),
            E::GraphNotActive { status, .. } => (
                StatusCode::CONFLICT,
                "GRAPH_NOT_ACTIVE",
                format!("the graph is {status}; normal acceptance requires an active graph"),
            ),
            E::ValidationRequired => (
                StatusCode::CONFLICT,
                "VALIDATION_REQUIRED",
                "acceptance requires semantic validation, which is not available yet".into(),
            ),
            E::ResourceLimit(message) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "RESOURCE_LIMIT",
                message.clone(),
            ),
            E::DependencyUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "DEPENDENCY_UNAVAILABLE",
                "the ledger database is not available; retry with the same idempotency key".into(),
            ),
            E::UnknownGraph(_) | E::NotFound(_) | E::MissingTarget(_) => (
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                "resource not found".into(),
            ),
            E::InvalidIdentifier { field, reason } => (
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                format!("invalid {field}: {reason}"),
            ),
            E::InvalidTimestamp(_) => (
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "invalid timestamp".into(),
            ),
            E::InvalidCommit(_) => (
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "the request does not form a valid commit".into(),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
                "internal ledger error".into(),
            ),
        };
        if status.is_server_error() {
            warn!(correlation, error = %error, "ledger error mapped to {code}");
        } else {
            info!(correlation, error = %error, "ledger error mapped to {code}");
        }
        Self::new(status, code, message, correlation)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                code: self.code,
                message: &self.message,
                correlation_id: &self.correlation_id,
            }),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------------------
// Request context

/// What every authenticated handler receives: the verified identity, its capabilities and
/// the request's correlation id. Never constructed from a request body.
pub struct RequestContext {
    pub identity: VerifiedIdentity,
    pub correlation_id: String,
}

impl RequestContext {
    fn require(&self, capability: Capability) -> Result<(), ApiError> {
        if self.identity.capabilities.has(capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "the authenticated identity lacks the required capability",
                &self.correlation_id,
            ))
        }
    }
}

fn correlation_of(parts: &Parts) -> String {
    parts
        .extensions
        .get::<Correlation>()
        .map(|c| c.0.clone())
        .unwrap_or_default()
}

impl FromRequestParts<AppState> for RequestContext {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let correlation = correlation_of(parts);
        let bearer = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "UNAUTHENTICATED",
                    "authentication required",
                    &correlation,
                )
            })?;
        let identity = state
            .0
            .authenticator
            .authenticate(bearer)
            .await
            .map_err(|e| ApiError::from_auth(e, &correlation))?;
        Ok(Self {
            identity,
            correlation_id: correlation,
        })
    }
}

/// JSON body extractor whose rejections use the public error envelope (a body that is too
/// large is `RESOURCE_LIMIT`; anything malformed or with unknown fields is
/// `INVALID_REQUEST`).
pub struct ValidJson<T>(pub T);

impl<T: serde::de::DeserializeOwned> axum::extract::FromRequest<AppState> for ValidJson<T> {
    type Rejection = ApiError;
    async fn from_request(request: Request<Body>, state: &AppState) -> Result<Self, ApiError> {
        let correlation = request
            .extensions()
            .get::<Correlation>()
            .map(|c| c.0.clone())
            .unwrap_or_default();
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(axum::extract::rejection::JsonRejection::BytesRejection(_)) => Err(
                ApiError::resource_limit("request body exceeds the configured limit", &correlation),
            ),
            Err(axum::extract::rejection::JsonRejection::MissingJsonContentType(_)) => Err(
                ApiError::invalid("content-type must be application/json", &correlation),
            ),
            Err(_) => Err(ApiError::invalid(
                "request body is not a valid request document (unknown fields are rejected)",
                &correlation,
            )),
        }
    }
}

fn idempotency_key(headers: &HeaderMap, correlation: &str) -> Result<String, ApiError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::invalid("Idempotency-Key header is required", correlation))?;
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES || key.chars().any(char::is_control)
    {
        return Err(ApiError::invalid(
            format!("Idempotency-Key must be 1..={MAX_IDEMPOTENCY_KEY_BYTES} bytes"),
            correlation,
        ));
    }
    Ok(key.to_owned())
}

/// Resolve and authorize the graph in the path: a graph that is missing, malformed or
/// owned by another tenant is `NOT_FOUND` alike.
async fn authorized_graph(
    state: &AppState,
    ctx: &RequestContext,
    graph: &str,
) -> Result<GraphId, ApiError> {
    let graph = GraphId::new(graph).map_err(|_| ApiError::not_found(&ctx.correlation_id))?;
    let record = state
        .0
        .store
        .graphs()
        .get(&graph)
        .await
        .map_err(|e| ApiError::from_ledger(e, &ctx.correlation_id))?;
    match record {
        Some(record) if record.tenant_id == ctx.identity.principal.tenant_id => Ok(graph),
        _ => Err(ApiError::not_found(&ctx.correlation_id)),
    }
}

fn scope(ctx: &RequestContext, graph: &GraphId, key: String, digest: ContentId) -> RequestScope {
    RequestScope {
        principal: ctx.identity.principal.clone(),
        graph: graph.clone(),
        idempotency_key: key,
        request_digest: digest,
        correlation_id: Some(ctx.correlation_id.clone()),
    }
}

fn check_branch(branch: &str, correlation: &str) -> Result<(), ApiError> {
    if branch.is_empty() || branch.len() > MAX_BRANCH_BYTES {
        return Err(ApiError::invalid(
            format!("ref name must be 1..={MAX_BRANCH_BYTES} bytes"),
            correlation,
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Handlers

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn ready(State(state): State<AppState>, request: Request<Body>) -> Response {
    let correlation = request
        .extensions()
        .get::<Correlation>()
        .map(|c| c.0.clone())
        .unwrap_or_default();
    match state.0.store.ready().await {
        Ok(()) => Json(Health { status: "ready" }).into_response(),
        Err(e) => {
            warn!(correlation, error = %e, "readiness check failed");
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "DEPENDENCY_UNAVAILABLE",
                "the ledger database is not available",
                &correlation,
            )
            .into_response()
        }
    }
}

async fn openapi() -> Response {
    ([(header::CONTENT_TYPE, "application/json")], OPENAPI_JSON).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationBody {
    pub op: String,
    pub quad: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareBody {
    /// Ref (branch) name; may contain `/`, hence a body field.
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub expected_head: Option<CommitId>,
    pub operations: Vec<OperationBody>,
    pub activity: String,
    pub event_time: Option<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub source_system: Option<String>,
    pub message: String,
}

#[derive(Serialize)]
pub struct PrepareResponse {
    pub proposal_id: i64,
    pub candidate: CommitId,
    pub requested_patch: ledger_core::PatchId,
    pub effective_patch: ledger_core::PatchId,
    pub replayed: bool,
    pub correlation_id: String,
}

/// A prepare request after parsing and normalization: what the digest covers and what the
/// workflow receives. Building it is pure so the golden tests exercise the exact handler
/// path.
pub struct ParsedPrepare {
    pub canonical: CanonicalRequest,
    pub branch: String,
    pub expected_head: Option<CommitId>,
    pub requested: Patch,
    pub activity: String,
    pub event_time: Option<LedgerTimestamp>,
    pub evidence_refs: Vec<String>,
    pub source_system: Option<String>,
    pub message: String,
}

fn check_text(
    field: &str,
    value: &str,
    max: usize,
    required: bool,
    correlation: &str,
) -> Result<(), ApiError> {
    if (required && value.is_empty()) || value.len() > max {
        return Err(ApiError::invalid(
            format!("{field} must be 1..={max} bytes"),
            correlation,
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(ApiError::invalid(
            format!("{field} must not contain control characters"),
            correlation,
        ));
    }
    Ok(())
}

/// Validate, bound and normalize a prepare body; compute its canonical identity. Cheap
/// checks (shape, sizes, counts, token syntax) all happen here, before any lock, slot or
/// reconstruction; the store re-validates the commit it builds.
pub fn canonical_prepare(
    graph: &GraphId,
    body: PrepareBody,
    limits: &ApiLimits,
    correlation: &str,
) -> Result<ParsedPrepare, ApiError> {
    check_branch(&body.ref_name, correlation)?;
    if body.operations.len() > limits.max_operations {
        return Err(ApiError::resource_limit(
            format!("at most {} operations per request", limits.max_operations),
            correlation,
        ));
    }
    if body.operations.is_empty() {
        return Err(ApiError::invalid(
            "at least one operation is required",
            correlation,
        ));
    }
    let metadata_bytes = body.activity.len()
        + body.message.len()
        + body.evidence_refs.iter().map(String::len).sum::<usize>()
        + body.source_system.as_ref().map_or(0, String::len);
    if metadata_bytes > limits.max_metadata_bytes {
        return Err(ApiError::resource_limit(
            format!("metadata exceeds {} bytes", limits.max_metadata_bytes),
            correlation,
        ));
    }
    check_text(
        "activity",
        &body.activity,
        ledger_core::MAX_IDENTIFIER_BYTES,
        true,
        correlation,
    )?;
    check_text(
        "message",
        &body.message,
        ledger_core::MAX_MESSAGE_BYTES,
        true,
        correlation,
    )?;
    let source_system = body.source_system.filter(|s| !s.is_empty());
    if let Some(source) = &source_system {
        check_text(
            "source_system",
            source,
            ledger_core::MAX_IDENTIFIER_BYTES,
            true,
            correlation,
        )?;
    }
    let distinct_evidence: std::collections::BTreeSet<&str> =
        body.evidence_refs.iter().map(String::as_str).collect();
    if distinct_evidence.len() > ledger_core::MAX_EVIDENCE_REFS {
        return Err(ApiError::invalid(
            format!(
                "at most {} distinct evidence references",
                ledger_core::MAX_EVIDENCE_REFS
            ),
            correlation,
        ));
    }
    for evidence in &body.evidence_refs {
        check_text(
            "evidence_refs",
            evidence,
            ledger_core::MAX_IDENTIFIER_BYTES,
            true,
            correlation,
        )?;
    }
    let mut operations = Vec::with_capacity(body.operations.len());
    for op in &body.operations {
        if op.quad.len() > limits.max_term_bytes {
            return Err(ApiError::resource_limit(
                format!("a quad exceeds {} bytes", limits.max_term_bytes),
                correlation,
            ));
        }
        let kind = match op.op.as_str() {
            "add" => OperationKind::Add,
            "delete" => OperationKind::Delete,
            _ => {
                return Err(ApiError::invalid(
                    "operation must be add or delete",
                    correlation,
                ));
            }
        };
        let quad = Quad::from_str(&op.quad)
            .map_err(|e| ApiError::invalid(format!("invalid quad: {e}"), correlation))?;
        operations.push(Operation { kind, quad });
    }
    let requested = Patch::new(operations)
        .map_err(|e| ApiError::invalid(format!("invalid patch: {e}"), correlation))?;
    let event_time = body
        .event_time
        .as_deref()
        .map(LedgerTimestamp::parse_rfc3339)
        .transpose()
        .map_err(|e| ApiError::from_ledger(e, correlation))?;
    let canonical = CanonicalRequest::Prepare {
        graph: graph.clone(),
        branch: body.ref_name.clone(),
        expected_head: body.expected_head.clone(),
        requested_patch: requested.id(),
        activity: body.activity.clone(),
        event_time,
        evidence_refs: body.evidence_refs.clone(),
        source_system: source_system.clone(),
        message: body.message.clone(),
    };
    Ok(ParsedPrepare {
        canonical,
        branch: body.ref_name,
        expected_head: body.expected_head,
        requested,
        activity: body.activity,
        event_time,
        evidence_refs: body.evidence_refs,
        source_system,
        message: body.message,
    })
}

async fn prepare(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<PrepareBody>,
) -> Result<(StatusCode, Json<PrepareResponse>), ApiError> {
    ctx.require(Capability::Propose)?;
    let correlation = ctx.correlation_id.clone();
    let key = idempotency_key(&headers, &correlation)?;
    let graph = authorized_graph(&state, &ctx, &graph).await?;
    let parsed = canonical_prepare(&graph, body, &state.0.limits, &correlation)?;
    let request = PrepareRequest {
        scope: scope(&ctx, &graph, key, parsed.canonical.digest()),
        branch: parsed.branch,
        expected_head: parsed.expected_head,
        requested: parsed.requested,
        activity: parsed.activity,
        event_time: parsed.event_time,
        evidence_refs: parsed.evidence_refs,
        source_system: parsed.source_system,
        message: parsed.message,
    };
    let _permit = state.0.expensive.try_acquire().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "RESOURCE_LIMIT",
            "too many concurrent expensive operations; retry later",
            &correlation,
        )
    })?;
    let prepared = state
        .0
        .store
        .workflows()
        .prepare(&request)
        .await
        .map_err(|e| ApiError::from_ledger(e, &correlation))?;
    Ok((
        if prepared.replayed {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(PrepareResponse {
            proposal_id: prepared.proposal_id,
            candidate: prepared.candidate,
            requested_patch: prepared.requested_patch,
            effective_patch: prepared.effective_patch,
            replayed: prepared.replayed,
            correlation_id: correlation,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptBody {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub expected_head: Option<CommitId>,
    pub reason: Option<String>,
}

#[derive(Serialize)]
pub struct AcceptResponse {
    pub decision_id: i64,
    pub ref_event_id: i64,
    pub outbox_id: i64,
    pub ref_version: i64,
    pub head: CommitId,
    pub replayed: bool,
    pub correlation_id: String,
}

fn check_reason(reason: Option<&str>, correlation: &str) -> Result<(), ApiError> {
    match reason {
        Some(reason) => check_text("reason", reason, MAX_REASON_BYTES, true, correlation),
        None => Ok(()),
    }
}

/// Validate an accept body and compute its canonical identity. The `validation_policy`
/// field names what the client requests; in Phase 1 the only requestable value is
/// `no-validation` (whether the deployment permits it is server configuration, not
/// request identity, so a retry after a policy change replays rather than conflicts).
pub fn canonical_accept(
    graph: &GraphId,
    candidate: &CommitId,
    body: &AcceptBody,
    correlation: &str,
) -> Result<CanonicalRequest, ApiError> {
    check_branch(&body.ref_name, correlation)?;
    check_reason(body.reason.as_deref(), correlation)?;
    Ok(CanonicalRequest::Accept {
        graph: graph.clone(),
        branch: body.ref_name.clone(),
        expected_head: body.expected_head.clone(),
        candidate: candidate.clone(),
        reason: body.reason.clone().filter(|r| !r.is_empty()),
        validation_policy: "no-validation".into(),
    })
}

/// Validate a reject body and compute its canonical identity.
pub fn canonical_reject(
    graph: &GraphId,
    candidate: &CommitId,
    body: &RejectBody,
    correlation: &str,
) -> Result<CanonicalRequest, ApiError> {
    check_branch(&body.ref_name, correlation)?;
    check_reason(Some(&body.reason), correlation)?;
    Ok(CanonicalRequest::Reject {
        graph: graph.clone(),
        branch: body.ref_name.clone(),
        candidate: candidate.clone(),
        reason: body.reason.clone(),
    })
}

async fn accept(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path((graph, candidate)): Path<(String, String)>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<AcceptBody>,
) -> Result<Json<AcceptResponse>, ApiError> {
    ctx.require(Capability::Review)?;
    let correlation = ctx.correlation_id.clone();
    let key = idempotency_key(&headers, &correlation)?;
    let graph = authorized_graph(&state, &ctx, &graph).await?;
    let candidate = CommitId::from_str(&candidate)
        .map_err(|_| ApiError::invalid("invalid candidate commit id", &correlation))?;
    let canonical = canonical_accept(&graph, &candidate, &body, &correlation)?;
    // The deployment's policy travels with the request; the store enforces it after the
    // idempotent-replay lookup, so a durable earlier acceptance still replays.
    let validation = match state.0.acceptance {
        AcceptancePolicy::RequireValidation => ValidationPolicy::Required,
        AcceptancePolicy::AllowUnvalidatedDevelopmentOnly => ValidationPolicy::NoValidation,
    };
    let request = AcceptRequest {
        scope: scope(&ctx, &graph, key, canonical.digest()),
        branch: body.ref_name,
        expected_head: body.expected_head,
        candidate,
        reason: body.reason,
        validation,
    };
    let accepted = state
        .0
        .store
        .workflows()
        .accept(&request)
        .await
        .map_err(|e| ApiError::from_ledger(e, &correlation))?;
    Ok(Json(AcceptResponse {
        decision_id: accepted.decision_id,
        ref_event_id: accepted.ref_event_id,
        outbox_id: accepted.outbox_id,
        ref_version: accepted.ref_version,
        head: accepted.head,
        replayed: accepted.replayed,
        correlation_id: correlation,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectBody {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub reason: String,
}

#[derive(Serialize)]
pub struct RejectResponse {
    pub decision_id: i64,
    pub replayed: bool,
    pub correlation_id: String,
}

async fn reject(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path((graph, candidate)): Path<(String, String)>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<RejectBody>,
) -> Result<Json<RejectResponse>, ApiError> {
    ctx.require(Capability::Review)?;
    let correlation = ctx.correlation_id.clone();
    let key = idempotency_key(&headers, &correlation)?;
    let graph = authorized_graph(&state, &ctx, &graph).await?;
    let candidate = CommitId::from_str(&candidate)
        .map_err(|_| ApiError::invalid("invalid candidate commit id", &correlation))?;
    let canonical = canonical_reject(&graph, &candidate, &body, &correlation)?;
    let request = RejectRequest {
        scope: scope(&ctx, &graph, key, canonical.digest()),
        branch: body.ref_name,
        candidate,
        reason: body.reason,
    };
    let rejected = state
        .0
        .store
        .workflows()
        .reject(&request)
        .await
        .map_err(|e| ApiError::from_ledger(e, &correlation))?;
    Ok(Json(RejectResponse {
        decision_id: rejected.decision_id,
        replayed: rejected.replayed,
        correlation_id: correlation,
    }))
}

#[derive(Serialize)]
pub struct RefResponse {
    pub name: String,
    pub head: Option<CommitId>,
    pub version: Option<i64>,
}

async fn read_ref(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Result<Json<RefResponse>, ApiError> {
    ctx.require(Capability::Read)?;
    let correlation = ctx.correlation_id.clone();
    let graph = authorized_graph(&state, &ctx, &graph).await?;
    let name = query.get("name").cloned().ok_or_else(|| {
        ApiError::invalid(
            "query parameter `name` (ref name) is required",
            &correlation,
        )
    })?;
    check_branch(&name, &correlation)?;
    let head = state
        .0
        .store
        .ref_head(&graph, &name)
        .await
        .map_err(|e| ApiError::from_ledger(e, &correlation))?;
    match head {
        Some((head, version)) => Ok(Json(RefResponse {
            name,
            head: Some(head),
            version: Some(version),
        })),
        None => Err(ApiError::not_found(&correlation)),
    }
}

#[derive(Serialize)]
pub struct StateResponse {
    pub commit: CommitId,
    pub quads: Vec<String>,
}

async fn read_state(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path((graph, commit)): Path<(String, String)>,
) -> Result<Json<StateResponse>, ApiError> {
    ctx.require(Capability::Read)?;
    let correlation = ctx.correlation_id.clone();
    let graph = authorized_graph(&state, &ctx, &graph).await?;
    // Malformed ids, unknown commits and commits of other graphs are all NOT_FOUND.
    let commit = CommitId::from_str(&commit).map_err(|_| ApiError::not_found(&correlation))?;
    let member = state
        .0
        .store
        .commit_graph(&commit)
        .await
        .map_err(|e| ApiError::from_ledger(e, &correlation))?;
    if member.as_ref() != Some(&graph) {
        return Err(ApiError::not_found(&correlation));
    }
    let _permit = state.0.expensive.try_acquire().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "RESOURCE_LIMIT",
            "too many concurrent expensive operations; retry later",
            &correlation,
        )
    })?;
    // A read never reconstructs more than it may export.
    let mut bounds = state.0.limits.reconstruction;
    bounds.max_bytes = bounds.max_bytes.min(state.0.limits.max_state_export_bytes);
    let quads = state
        .0
        .store
        .workflows()
        .reconstruct(&commit, &bounds)
        .await
        .map_err(|e| ApiError::from_ledger(e, &correlation))?;
    let mut total = 0usize;
    let mut out = Vec::with_capacity(quads.len());
    for quad in quads {
        let line = quad.to_string();
        total += line.len() + 1;
        if total > state.0.limits.max_state_export_bytes {
            return Err(ApiError::resource_limit(
                format!(
                    "state export exceeds {} bytes",
                    state.0.limits.max_state_export_bytes
                ),
                &correlation,
            ));
        }
        out.push(line);
    }
    Ok(Json(StateResponse { commit, quads: out }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_document_matches_the_served_routes_and_error_codes() {
        let doc: serde_json::Value = serde_json::from_str(OPENAPI_JSON).expect("valid JSON");
        let paths = doc["paths"].as_object().expect("paths object");
        let mut documented = std::collections::BTreeSet::new();
        for (path, item) in paths {
            for method in item.as_object().expect("path item").keys() {
                documented.insert((method.to_uppercase(), path.clone()));
            }
        }
        let served: std::collections::BTreeSet<(String, String)> = ROUTES
            .iter()
            .map(|(m, p)| ((*m).to_owned(), (*p).to_owned()))
            .collect();
        assert_eq!(
            documented, served,
            "OpenAPI paths must equal the served routes"
        );
        let codes: std::collections::BTreeSet<&str> =
            doc["components"]["schemas"]["Error"]["properties"]["code"]["enum"]
                .as_array()
                .expect("error code enum")
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
        for code in [
            "INVALID_REQUEST",
            "UNAUTHENTICATED",
            "FORBIDDEN",
            "NOT_FOUND",
            "HEAD_CHANGED",
            "IDEMPOTENCY_CONFLICT",
            "LINEAGE_MISMATCH",
            "BASE_MISMATCH",
            "NO_EFFECTIVE_CHANGE",
            "GRAPH_NOT_ACTIVE",
            "VALIDATION_REQUIRED",
            "RESOURCE_LIMIT",
            "DEPENDENCY_UNAVAILABLE",
            "INTERNAL",
        ] {
            assert!(codes.contains(code), "OpenAPI error enum lacks {code}");
        }
        assert!(
            !OPENAPI_JSON.contains("/v1/commits"),
            "the bootstrap write endpoint must not be documented"
        );
    }

    fn lazy_state(auth: SharedAuthenticator, limits: ApiLimits) -> AppState {
        // No database listens on port 1: every store call fails as a dependency error.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_secs(2))
            .connect_lazy("postgres://ledger:x@127.0.0.1:1/ledger")
            .expect("lazy pool");
        let store = PostgresLedgerStore::from_pool_migrated(pool, ledger_store::V1Binding::Reject);
        AppState::new(store, auth, limits, AcceptancePolicy::RequireValidation)
    }

    fn dev_auth() -> (SharedAuthenticator, String) {
        let secret = b"unit-test-secret-that-is-at-least-32-bytes";
        let auth = Arc::new(
            auth::DevHs256Authenticator::new(
                "https://issuer.test/".into(),
                "api://ledger".into(),
                secret,
                auth::ClaimsPolicy::default(),
            )
            .unwrap(),
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &serde_json::json!({"iss": "https://issuer.test/", "aud": "api://ledger", "exp": now + 300,
                "tid": "t", "oid": "u", "sculpin_principal_type": "human", "roles": ["ledger.read"]}),
            &jsonwebtoken::EncodingKey::from_secret(secret),
        )
        .unwrap();
        (auth, token)
    }

    async fn send(
        app: &Router,
        method: &str,
        path: &str,
        bearer: Option<&str>,
    ) -> (StatusCode, serde_json::Value, Option<String>) {
        use tower::ServiceExt;
        let mut request = Request::builder().method(method).uri(path);
        if let Some(b) = bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {b}"));
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let correlation = response
            .headers()
            .get(CORRELATION_HEADER)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body, correlation)
    }

    #[tokio::test]
    async fn every_documented_route_is_served_and_unknown_routes_use_the_envelope() {
        let (auth, _) = dev_auth();
        let app = router(lazy_state(auth, ApiLimits::default()));
        let doc: serde_json::Value = serde_json::from_str(OPENAPI_JSON).unwrap();
        for (path, item) in doc["paths"].as_object().unwrap() {
            for method in item.as_object().unwrap().keys() {
                let concrete = path
                    .replace("{graph}", "g1")
                    .replace("{candidate}", &format!("sha256:{}", "a".repeat(64)))
                    .replace("{commit}", &format!("sha256:{}", "a".repeat(64)));
                let (status, body, correlation) =
                    send(&app, &method.to_uppercase(), &concrete, None).await;
                assert_ne!(
                    status,
                    StatusCode::NOT_FOUND,
                    "{method} {path} is not routed"
                );
                assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
                assert!(
                    correlation.is_some(),
                    "{method} {path} lacks a correlation id"
                );
                if path.starts_with("/v1/") {
                    assert_eq!(body["code"], "UNAUTHENTICATED", "{method} {path}: {body}");
                }
            }
        }
        // Unknown path and wrong method both answer with the envelope.
        for (method, path) in [
            ("GET", "/v1/commits"),
            ("DELETE", "/health"),
            ("POST", "/nope"),
        ] {
            let (status, body, _) = send(&app, method, path, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
            assert_eq!(body["code"], "NOT_FOUND", "{method} {path}: {body}");
        }
    }

    #[tokio::test]
    async fn database_outage_is_a_redacted_dependency_failure_on_every_path() {
        let (auth, token) = dev_auth();
        let app = router(lazy_state(auth, ApiLimits::default()));
        let (status, body, _) = send(&app, "GET", "/ready", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["code"], "DEPENDENCY_UNAVAILABLE");
        let (status, body, correlation) =
            send(&app, "GET", "/v1/graphs/g1/refs?name=main", Some(&token)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["code"], "DEPENDENCY_UNAVAILABLE");
        assert_eq!(body["correlation_id"], correlation.unwrap());
        let message = body["message"].as_str().unwrap();
        for leak in ["127.0.0.1", "postgres", "sqlx", "ledger:x"] {
            assert!(!message.contains(leak), "leak {leak:?} in {message}");
        }
        let (status, _, _) = send(&app, "GET", "/health", None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "liveness is independent of the database"
        );
    }

    struct NeverAnswers;
    #[async_trait::async_trait]
    impl auth::Authenticator for NeverAnswers {
        async fn authenticate(&self, _: &str) -> Result<auth::VerifiedIdentity, AuthError> {
            std::future::pending().await
        }
        fn is_production_grade(&self) -> bool {
            false
        }
        fn describe(&self) -> String {
            "never answers".into()
        }
    }

    #[tokio::test]
    async fn requests_exceeding_the_time_limit_get_a_resource_limit_envelope() {
        let limits = ApiLimits {
            request_timeout: Duration::from_millis(50),
            ..ApiLimits::default()
        };
        let app = router(lazy_state(Arc::new(NeverAnswers), limits));
        let (status, body, correlation) =
            send(&app, "GET", "/v1/graphs/g1/refs?name=main", Some("t")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["code"], "RESOURCE_LIMIT");
        assert_eq!(body["correlation_id"], correlation.unwrap());
    }

    #[test]
    fn correlation_ids_are_bounded_printable_ascii() {
        assert!(valid_correlation("req-123_ABC.x"));
        assert!(!valid_correlation(""));
        assert!(!valid_correlation("has space"));
        assert!(!valid_correlation(&"x".repeat(MAX_CORRELATION_BYTES + 1)));
        assert!(!valid_correlation("tab\there"));
    }
}
