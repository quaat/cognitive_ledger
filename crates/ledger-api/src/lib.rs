//! Authenticated, graph-scoped HTTP adapter over the atomic workflow (ADR-0011/0013).
//! It exposes no query language. Every public mutation requires a verified identity,
//! an `Idempotency-Key`, and is routed through `WorkflowRepository` with a
//! server-computed canonical request digest; every public read is graph-scoped and
//! bounded. Errors are a stable, redacted envelope carrying the request's correlation id.

pub mod auth;
pub mod lifecycle;
pub mod request_identity;
pub mod validator;

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
    AcceptRequest, DbSessionLimits, Ledger, MAX_BRANCH_BYTES, MAX_CORRELATION_BYTES,
    MAX_IDEMPOTENCY_KEY_BYTES, MAX_REASON_BYTES, PostgresLedgerStore, PrepareRequest,
    ReconstructionLimits, RejectRequest, RequestScope, ValidateRequest, ValidationBegin,
    ValidationPolicy, ValidationTrustPolicy, ValidatorOutcome,
};
use ledger_validation_protocol::{
    CandidateDescriptor, RequestedContext, SemanticContextId, SemanticEnvironmentId,
    SemanticExecutionContext, ValidationClient, ValidationId, ValidationInvocation,
    ValidationRecord, ValidationRequest,
};
use request_identity::CanonicalRequest;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
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
    /// Concurrent outbound validations (a dedicated budget beside the expensive slots: the
    /// reconstruction before the call takes an expensive slot, the call itself one of these).
    pub max_concurrent_validations: usize,
    /// Total time allowed for one validator call; must be below `request_timeout`.
    pub validator_timeout: Duration,
    /// Maximum bytes of a validator response body (streamed and capped).
    pub validator_response_bytes: usize,
    /// Maximum bytes of candidate N-Quads shipped inline to the validator.
    pub max_validation_state_bytes: usize,
    /// Maximum bytes of the encoded context hints of a validate request.
    pub max_validation_metadata_bytes: usize,
    /// Byte budget of a merge preview's detailed conflict report (the `conflicts` array as
    /// JSON, ADR-0024). Diagnostic only: the classification, merged state, conflict count
    /// and preview token never depend on it. Within
    /// `ledger_merge::{MIN,MAX}_CONFLICT_REPORT_BYTES`.
    pub max_merge_conflict_report_bytes: usize,
}

impl ApiLimits {
    /// Refuse values that would make a limit ambiguous rather than clamping them: a merge
    /// conflict report budget outside `MIN..=MAX_CONFLICT_REPORT_BYTES` (zero included).
    pub fn validate(&self) -> Result<(), String> {
        self.merge_report_limits().map(|_| ())
    }

    fn merge_report_limits(&self) -> Result<ledger_store::MergeReportLimits, String> {
        ledger_store::MergeReportLimits::with_max_bytes(self.max_merge_conflict_report_bytes)
            .map_err(|e| format!("max_merge_conflict_report_bytes: {e}"))
    }
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
            max_concurrent_validations: 4,
            // ADR-0026 §2: validator_timeout + statement_timeout ≤ request_timeout (15 + 10 ≤ 30).
            validator_timeout: Duration::from_secs(15),
            validator_response_bytes: 1024 * 1024,
            max_validation_state_bytes: 8 * 1024 * 1024,
            max_validation_metadata_bytes: 16 * 1024,
            max_merge_conflict_report_bytes: ledger_store::MergeReportLimits::DEFAULT.max_bytes(),
        }
    }
}

/// The configured semantic validation service (ADR-0014): the client the ledger calls and
/// the service identity recorded in every context (deployment configuration, never client
/// input).
#[derive(Clone)]
pub struct ValidationService {
    pub client: Arc<dyn ValidationClient>,
    pub service_id: String,
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
    /// Owned permits (`Arc`): a permit moves into the detached operation and is released when
    /// the operation ends, never when the edge stops waiting (ADR-0026 §3, §8).
    expensive: Arc<tokio::sync::Semaphore>,
    validations: Arc<tokio::sync::Semaphore>,
    /// Every database-bearing route operation runs through this tracker (ADR-0026 §8).
    detached: lifecycle::DetachedOperations,
    validation: Option<ValidationService>,
    /// The trusted validation service (ADR-0019); independent of `validation`, which is
    /// only the ability to call it.
    trust: Option<ValidationTrustPolicy>,
    correlation_counter: Arc<AtomicU64>,
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
        // Invalid limits are a configuration error, refused at startup (the server checks
        // them first and exits with a readable message).
        if let Err(e) = limits.validate() {
            panic!("invalid ApiLimits: {e}");
        }
        // One source of truth for reconstruction bounds: the workflow's base
        // reconstruction and public state reads use the same limits.
        let store = store.with_limits(limits.reconstruction);
        // A trust anchor already on the store is the state's: it can then only be confirmed,
        // never silently replaced.
        let trust = store.validation_trust().cloned();
        Self(Arc::new(Shared {
            expensive: Arc::new(tokio::sync::Semaphore::new(limits.max_concurrent_expensive)),
            validations: Arc::new(tokio::sync::Semaphore::new(
                limits.max_concurrent_validations,
            )),
            detached: lifecycle::DetachedOperations::new(),
            validation: None,
            trust,
            store,
            authenticator,
            limits,
            acceptance,
            correlation_counter: Arc::new(AtomicU64::new(0)),
        }))
    }

    /// Trust records of exactly this validation service for validated acceptance
    /// (`LEDGER_VALIDATOR_SERVICE_ID`). Independent of an endpoint: with trust but no client,
    /// `validate` answers `VALIDATOR_UNAVAILABLE` while earlier records of the trusted
    /// service still satisfy acceptance. Without trust, validated acceptance fails closed.
    /// Construction-time only (before the state is shared).
    pub fn with_validation_trust(self, trust: ValidationTrustPolicy) -> Self {
        let mut shared = Arc::try_unwrap(self.0).unwrap_or_else(|_| {
            panic!("with_validation_trust must be called before the state is shared")
        });
        if shared.trust.as_ref().is_some_and(|t| t != &trust) {
            panic!("the validation trust policy is set once");
        }
        shared.store = shared.store.clone().with_validation_trust(trust.clone());
        shared.trust = Some(trust);
        Self(Arc::new(shared))
    }

    /// Attach the client for the validation service. The service must be the trusted one:
    /// without an explicit trust policy it becomes the trust anchor; a client for any other
    /// service is a configuration error. Construction-time only.
    pub fn with_validation(self, service: ValidationService) -> Self {
        let trust = ValidationTrustPolicy::single(service.service_id.clone())
            .unwrap_or_else(|_| panic!("validator service id must be a bounded token"));
        let state = match &self.0.trust {
            Some(existing) if existing != &trust => {
                panic!("the validation client must be for the trusted validation service")
            }
            Some(_) => self,
            None => self.with_validation_trust(trust),
        };
        let mut shared = Arc::try_unwrap(state.0).unwrap_or_else(|_| {
            panic!("with_validation must be called before the state is shared")
        });
        shared.validation = Some(service);
        Self(Arc::new(shared))
    }

    fn edge(&self) -> EdgeConfig {
        EdgeConfig {
            request_timeout: self.0.limits.request_timeout,
            correlation_counter: self.0.correlation_counter.clone(),
        }
    }

    /// The tracker of detached database-bearing operations (ADR-0026 §8): the server closes
    /// it at shutdown and waits for it; tests observe it.
    pub fn detached(&self) -> &lifecycle::DetachedOperations {
        &self.0.detached
    }

    /// Shutdown step 4 (ADR-0026 §8): refuse new database-bearing operations. Running ones
    /// continue to their bounded end; `detached().wait_idle()` resolves when they are done.
    pub fn begin_shutdown(&self) {
        self.0.detached.close();
    }

    /// The lifecycle limits of the served store (for start-up validation).
    pub fn session_limits(&self) -> DbSessionLimits {
        self.0.store.session_limits()
    }

    /// Run the database-bearing part of a route as one tracked, detached operation (ADR-0026
    /// §3, §8): everything from the graph lookup to the last pooled query happens inside
    /// `operation`, under the request's deadline for every connection acquisition. The edge
    /// waits until `deadline`; past it the client gets `REQUEST_TIMEOUT` for its class and
    /// the operation continues to its own bounded end (transaction bound + statement tail).
    /// Nothing inside is ever dropped by a timeout.
    async fn run_db<T, F>(
        &self,
        deadline: Instant,
        class: lifecycle::OperationClass,
        correlation: &str,
        operation: F,
    ) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, ApiError>> + Send + 'static,
    {
        if Instant::now() >= deadline {
            // The budget was spent before any database work (slow authentication or body):
            // nothing to start, nothing done.
            return Err(ApiError::request_timeout(class, correlation));
        }
        let handle = self
            .0
            .detached
            .spawn(ledger_store::lifecycle::with_request_deadline(
                deadline, operation,
            ))
            .map_err(|_| ApiError::shutting_down(class, correlation))?;
        match tokio::time::timeout_at(deadline.into(), handle).await {
            Ok(Ok(result)) => result.map_err(|e| e.for_class(class)),
            Ok(Err(join)) => {
                // A panic inside the operation: the registration is released by its guard. Only
                // the failure kind is logged (a panic payload could carry request data).
                warn!(
                    correlation,
                    panicked = join.is_panic(),
                    cancelled = join.is_cancelled(),
                    "database operation task failed"
                );
                Err(ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL",
                    "internal ledger error",
                    correlation,
                ))
            }
            Err(_) => Err(ApiError::request_timeout(class, correlation)),
        }
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
    (
        "POST",
        "/v1/graphs/{graph}/proposals/{candidate}/validations",
    ),
    (
        "GET",
        "/v1/graphs/{graph}/proposals/{candidate}/validations/{validation}",
    ),
    ("GET", "/v1/graphs/{graph}/refs"),
    ("GET", "/v1/graphs/{graph}/commits/{commit}/state"),
    ("POST", "/v1/graphs/{graph}/branches"),
    ("GET", "/v1/graphs/{graph}/branches"),
    ("GET", "/v1/graphs/{graph}/branches/status"),
    ("GET", "/v1/graphs/{graph}/branches/history"),
    ("GET", "/v1/graphs/{graph}/branches/log"),
    ("POST", "/v1/graphs/{graph}/branches/delete"),
    ("POST", "/v1/graphs/{graph}/branches/restore"),
    ("POST", "/v1/graphs/{graph}/merges/preview"),
    ("POST", "/v1/graphs/{graph}/merges/propose"),
    ("POST", "/v1/graphs/{graph}/merges/apply"),
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
        .route(
            "/v1/graphs/{graph}/proposals/{candidate}/validations",
            post(validate),
        )
        .route(
            "/v1/graphs/{graph}/proposals/{candidate}/validations/{validation}",
            get(read_validation),
        )
        .route("/v1/graphs/{graph}/refs", get(read_ref))
        .route("/v1/graphs/{graph}/commits/{commit}/state", get(read_state))
        // Branch names travel in bodies and query parameters only (ADR-0022): `/` inside a
        // name (`agent/task-17`) never becomes path structure.
        .route(
            "/v1/graphs/{graph}/branches",
            post(create_branch).get(list_branches),
        )
        .route("/v1/graphs/{graph}/branches/status", get(branch_status))
        .route("/v1/graphs/{graph}/branches/history", get(branch_history))
        .route("/v1/graphs/{graph}/branches/log", get(branch_log))
        .route("/v1/graphs/{graph}/branches/delete", post(delete_branch))
        .route("/v1/graphs/{graph}/branches/restore", post(restore_branch))
        .route("/v1/graphs/{graph}/merges/preview", post(merge_preview))
        .route("/v1/graphs/{graph}/merges/propose", post(merge_propose))
        .route("/v1/graphs/{graph}/merges/apply", post(merge_apply))
        .fallback(unknown_route)
        .method_not_allowed_fallback(unknown_route)
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(middleware::from_fn_with_state(
            state.edge(),
            correlation_and_timeout,
        ))
        .with_state(state)
}

/// Filesystem development mode: read-only inspection of the single bootstrap ref. No
/// mutation surface exists here; it is loopback-only by the server's binding rules. The
/// same read guards as the shared router apply: reconstruction and export limits, request
/// timeout, concurrency cap, correlation ids and the error envelope.
pub fn filesystem_readonly_router(ledger: Arc<Ledger>, limits: ApiLimits) -> Router {
    #[derive(Clone)]
    struct FsState {
        ledger: Arc<Ledger>,
        limits: ApiLimits,
        expensive: Arc<tokio::sync::Semaphore>,
    }
    fn correlation(request: &Request<Body>) -> String {
        request
            .extensions()
            .get::<Correlation>()
            .map(|c| c.0.clone())
            .unwrap_or_default()
    }
    async fn fs_head(
        State(s): State<FsState>,
        request: Request<Body>,
    ) -> Result<Json<RefResponse>, ApiError> {
        let correlation = correlation(&request);
        let head = s
            .ledger
            .head()
            .await
            .map_err(|e| ApiError::from_ledger(e, &correlation))?;
        Ok(Json(RefResponse {
            name: "main".into(),
            head,
            version: None,
        }))
    }
    async fn fs_state(
        State(s): State<FsState>,
        Path(id): Path<String>,
        request: Request<Body>,
    ) -> Result<Json<StateResponse>, ApiError> {
        let correlation = correlation(&request);
        let id = CommitId::from_str(&id).map_err(|_| ApiError::not_found(&correlation))?;
        let _permit = s.expensive.try_acquire().map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "RESOURCE_LIMIT",
                "too many concurrent expensive operations; retry later",
                &correlation,
            )
        })?;
        let mut bounds = s.limits.reconstruction;
        bounds.max_bytes = bounds.max_bytes.min(s.limits.max_state_export_bytes);
        let quads = s
            .ledger
            .state_at_bounded(&id, &bounds)
            .await
            .map_err(|e| ApiError::from_ledger(e, &correlation))?;
        let quads = export_quads(quads, s.limits.max_state_export_bytes, &correlation)?;
        Ok(Json(StateResponse { commit: id, quads }))
    }
    let edge = EdgeConfig {
        request_timeout: limits.request_timeout,
        correlation_counter: Arc::new(AtomicU64::new(0)),
    };
    let state = FsState {
        expensive: Arc::new(tokio::sync::Semaphore::new(limits.max_concurrent_expensive)),
        ledger,
        limits,
    };
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(|| async { Json(Health { status: "ready" }) }))
        .route("/v1/refs/main", get(fs_head))
        .route("/v1/states/{id}", get(fs_state))
        .fallback(unknown_route)
        .method_not_allowed_fallback(unknown_route)
        .layer(middleware::from_fn_with_state(
            edge,
            correlation_and_timeout,
        ))
        .with_state(state)
}

/// Serialize a reconstructed state as canonical N-Quads lines under the export byte limit.
fn export_quads(
    quads: BTreeSet<Quad>,
    max_bytes: usize,
    correlation: &str,
) -> Result<Vec<String>, ApiError> {
    let mut total = 0usize;
    let mut out = Vec::with_capacity(quads.len());
    for quad in quads {
        let line = quad.to_string();
        total += line.len() + 1;
        if total > max_bytes {
            return Err(ApiError::resource_limit(
                format!("state export exceeds {max_bytes} bytes"),
                correlation,
            ));
        }
        out.push(line);
    }
    Ok(out)
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

/// What the correlation/timeout middleware needs from either router.
#[derive(Clone)]
struct EdgeConfig {
    request_timeout: Duration,
    correlation_counter: Arc<AtomicU64>,
}

/// How much longer than the request deadline the edge lets a handler run before answering
/// for it. A handler that has reached its database operation answers `REQUEST_TIMEOUT` itself
/// exactly at the deadline (`run_db`); the grace only catches work before that point (slow
/// bodies, authentication), which holds no pooled connection and may be dropped.
const EDGE_GRACE: Duration = Duration::from_secs(1);

async fn correlation_and_timeout(
    State(edge): State<EdgeConfig>,
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
        let n = edge.correlation_counter.fetch_add(1, Ordering::Relaxed);
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
    let deadline = Instant::now() + edge.request_timeout;
    request
        .extensions_mut()
        .insert(lifecycle::RequestDeadline(deadline));
    // The class is decided from the request alone (ADR-0026 §4): an idempotent write is a
    // mutating method carrying an Idempotency-Key; everything else is a read (merge preview
    // is a POST without a key and so a read).
    let class = if request.method() != axum::http::Method::GET
        && request.headers().contains_key("idempotency-key")
    {
        lifecycle::OperationClass::IdempotentWrite
    } else {
        lifecycle::OperationClass::Read
    };
    let mut response =
        match tokio::time::timeout_at((deadline + EDGE_GRACE).into(), next.run(request)).await {
            Ok(response) => response,
            Err(_) => ApiError::request_timeout(class, &correlation).into_response(),
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
    /// The request exceeded its time budget at the edge (ADR-0026 §4). For an idempotent write
    /// the detached operation may still commit, so the outcome is unknown and the same key
    /// resolves it; a read wrote nothing and has no key.
    fn request_timeout(class: lifecycle::OperationClass, correlation: &str) -> Self {
        let message = match class {
            lifecycle::OperationClass::Read => {
                "the request exceeded its time limit; nothing was written; retry"
            }
            lifecycle::OperationClass::IdempotentWrite => {
                "the request exceeded its time limit; the outcome is unknown: retry with the \
                 same idempotency key to replay a committed result or execute it afresh"
            }
        };
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "REQUEST_TIMEOUT",
            message,
            correlation,
        )
    }

    /// The server is draining (ADR-0026 §8): no new database-bearing operation starts, so
    /// nothing was done.
    fn shutting_down(class: lifecycle::OperationClass, correlation: &str) -> Self {
        let message = match class {
            lifecycle::OperationClass::Read => {
                "the ledger is shutting down; nothing was done; retry against another replica"
            }
            lifecycle::OperationClass::IdempotentWrite => {
                "the ledger is shutting down; nothing was done; retry with the same idempotency \
                 key against another replica"
            }
        };
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "DEPENDENCY_UNAVAILABLE",
            message,
            correlation,
        )
    }

    /// Dependency guidance by class (ADR-0026 §4): the store's messages are written for
    /// idempotent writes ("retry with the same idempotency key"); a key-less read gets read
    /// guidance instead. Everything else passes through unchanged.
    fn for_class(mut self, class: lifecycle::OperationClass) -> Self {
        if class == lifecycle::OperationClass::Read {
            match self.code {
                "DEPENDENCY_UNAVAILABLE" => {
                    self.message =
                        "the ledger database is not available; nothing was written; retry".into();
                }
                "DEPENDENCY_TIMEOUT" => {
                    self.message = "the database operation exceeded its time limit or lost a lock \
                                    race; nothing was written; retry"
                        .into();
                }
                _ => {}
            }
        }
        self
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
            E::BranchNotFound(_) => (
                StatusCode::NOT_FOUND,
                "BRANCH_NOT_FOUND",
                "branch not found".into(),
            ),
            E::BranchExists(_) => (
                StatusCode::CONFLICT,
                "BRANCH_EXISTS",
                "a branch of that name already exists in this graph".into(),
            ),
            E::BranchDeleted(_) => (
                StatusCode::CONFLICT,
                "BRANCH_DELETED",
                "the branch is deleted; restore it before proposing or accepting on it".into(),
            ),
            E::BranchPointUnreachable => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "BRANCH_POINT_UNREACHABLE",
                "the branch point is not the source branch head nor reachable from it".into(),
            ),
            E::BranchPolicyViolation(message) => (
                StatusCode::CONFLICT,
                "BRANCH_POLICY_VIOLATION",
                message.clone(),
            ),
            E::BranchStateConflict(message) => (
                StatusCode::CONFLICT,
                "BRANCH_STATE_CONFLICT",
                message.clone(),
            ),
            E::MergeStale(message) => (StatusCode::CONFLICT, "MERGE_STALE", message.clone()),
            E::MergeConflict(count) => (
                StatusCode::CONFLICT,
                "MERGE_CONFLICT",
                format!("{count} structurally conflicting key(s); choose a resolving strategy"),
            ),
            E::MergeNothingToDo(message) => {
                (StatusCode::CONFLICT, "MERGE_NOTHING_TO_DO", message.clone())
            }
            E::InvalidMergeBase(message) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "INVALID_MERGE_BASE",
                message.clone(),
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
                "acceptance requires a conforming validation record: name validation_id and \
                 semantic_environment_id"
                    .into(),
            ),
            E::ValidationRejected => (
                StatusCode::CONFLICT,
                "VALIDATION_REJECTED",
                "the named validation reported violations; the candidate cannot be accepted on it"
                    .into(),
            ),
            E::ValidationStale(_) => (
                StatusCode::CONFLICT,
                "VALIDATION_STALE",
                "the named validation does not satisfy this deployment's acceptance policy: it \
                 ran in another semantic environment than the one named, or was not produced by \
                 the validation service this deployment trusts"
                    .into(),
            ),
            E::ValidationNotFound => (
                StatusCode::NOT_FOUND,
                "VALIDATION_NOT_FOUND",
                "validation record not found".into(),
            ),
            E::ValidatorUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "VALIDATOR_UNAVAILABLE",
                "the semantic validation service is not available; nothing was recorded; retry \
                 with the same idempotency key"
                    .into(),
            ),
            E::ValidatorError(_) => (
                StatusCode::BAD_GATEWAY,
                "VALIDATOR_ERROR",
                "the semantic validation service returned a response the ledger cannot record; \
                 nothing was recorded"
                    .into(),
            ),
            E::InvalidValidation(_) => (
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "the request does not form a valid validation request".into(),
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
            E::DependencyTimeout(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "DEPENDENCY_TIMEOUT",
                "the database operation exceeded its time limit or lost a lock race and was \
                 rolled back; retry with the same idempotency key"
                    .into(),
            ),
            // COMMIT itself failed (ADR-0026 §4): the transaction may be durable, so never
            // "rolled back"; the code follows the driver failure's class, the guidance is
            // identical.
            E::CommitOutcomeUnknown(inner) => (
                StatusCode::SERVICE_UNAVAILABLE,
                match **inner {
                    E::DependencyTimeout(_) => "DEPENDENCY_TIMEOUT",
                    _ => "DEPENDENCY_UNAVAILABLE",
                },
                "the database did not confirm the commit; the outcome is unknown: retry with \
                 the same idempotency key (a committed result replays, a rolled-back one \
                 executes afresh)"
                    .into(),
            ),
            E::SchemaIncompatible(_) | E::RuntimeIdentity(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "DEPENDENCY_UNAVAILABLE",
                "the ledger database is not compatible with this build's configuration".into(),
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
    /// When the request's time budget ends (set by the edge middleware).
    pub deadline: Instant,
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
            .and_then(bearer_credential)
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
        let deadline = parts
            .extensions
            .get::<lifecycle::RequestDeadline>()
            .map(|d| d.0)
            .unwrap_or_else(|| Instant::now() + state.0.limits.request_timeout);
        Ok(Self {
            identity,
            correlation_id: correlation,
            deadline,
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

/// The credential of an `Authorization` header using the Bearer scheme. Scheme names are
/// case-insensitive (RFC 9110 §11.1); the credential itself is returned verbatim and must
/// be non-empty. Any other scheme is not a bearer token.
fn bearer_credential(header: &str) -> Option<&str> {
    let (scheme, credential) = header.trim_start().split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let credential = credential.trim();
    (!credential.is_empty() && !credential.contains(char::is_whitespace)).then_some(credential)
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
    let deadline = request
        .extensions()
        .get::<lifecycle::RequestDeadline>()
        .map(|d| d.0)
        .unwrap_or_else(|| Instant::now() + state.0.limits.request_timeout);
    let detached = state.clone();
    let inner_correlation = correlation.clone();
    let outcome = state
        .run_db(
            deadline,
            lifecycle::OperationClass::Read,
            &correlation,
            async move {
                detached.0.store.ready().await.map_err(|e| {
                    warn!(correlation = %inner_correlation, error = %e, "readiness check failed");
                    ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "DEPENDENCY_UNAVAILABLE",
                        "the ledger database is not available",
                        &inner_correlation,
                    )
                })
            },
        )
        .await;
    match outcome {
        Ok(()) => Json(Health { status: "ready" }).into_response(),
        Err(e) => e.into_response(),
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
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::IdempotentWrite,
            &corr,
            async move {
                let state = detached;
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
                let _permit = Arc::clone(&state.0.expensive)
                    .try_acquire_owned()
                    .map_err(|_| {
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
            },
        )
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptBody {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub expected_head: Option<CommitId>,
    pub reason: Option<String>,
    /// The validation record acceptance is bound to (ADR-0019); with
    /// `semantic_environment_id`, both or neither.
    #[serde(default)]
    pub validation_id: Option<ValidationId>,
    /// The semantic environment the reviewer accepts under (candidate-independent; the one
    /// Sculpin declares current).
    #[serde(default)]
    pub semantic_environment_id: Option<SemanticEnvironmentId>,
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
    match (&body.validation_id, &body.semantic_environment_id) {
        (Some(validation_id), Some(semantic_environment_id)) => {
            Ok(CanonicalRequest::AcceptValidated {
                graph: graph.clone(),
                branch: body.ref_name.clone(),
                expected_head: body.expected_head.clone(),
                candidate: candidate.clone(),
                reason: body.reason.clone().filter(|r| !r.is_empty()),
                validation_id: validation_id.clone(),
                semantic_environment_id: semantic_environment_id.clone(),
            })
        }
        (None, None) => Ok(CanonicalRequest::Accept {
            graph: graph.clone(),
            branch: body.ref_name.clone(),
            expected_head: body.expected_head.clone(),
            candidate: candidate.clone(),
            reason: body.reason.clone().filter(|r| !r.is_empty()),
            validation_policy: "no-validation".into(),
        }),
        _ => Err(ApiError::invalid(
            "validation_id and semantic_environment_id are given together or not at all",
            correlation,
        )),
    }
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
    Ok(match &body.validation_id {
        Some(validation_id) => CanonicalRequest::RejectValidated {
            graph: graph.clone(),
            branch: body.ref_name.clone(),
            candidate: candidate.clone(),
            reason: body.reason.clone(),
            validation_id: validation_id.clone(),
        },
        None => CanonicalRequest::Reject {
            graph: graph.clone(),
            branch: body.ref_name.clone(),
            candidate: candidate.clone(),
            reason: body.reason.clone(),
        },
    })
}

/// Validate a validate body (context hints) and compute its canonical identity (request
/// v2). Bounds come before any lock, slot or reconstruction.
pub fn canonical_validate(
    graph: &GraphId,
    candidate: &CommitId,
    body: &ValidateBody,
    limits: &ApiLimits,
    correlation: &str,
) -> Result<CanonicalRequest, ApiError> {
    body.requested
        .validate()
        .map_err(|e| ApiError::invalid(format!("invalid context hints: {e}"), correlation))?;
    let mut encoded = Vec::new();
    body.requested
        .encode_into(&mut encoded)
        .map_err(|e| ApiError::invalid(format!("invalid context hints: {e}"), correlation))?;
    if encoded.len() > limits.max_validation_metadata_bytes {
        return Err(ApiError::resource_limit(
            format!(
                "context hints exceed {} bytes",
                limits.max_validation_metadata_bytes
            ),
            correlation,
        ));
    }
    Ok(CanonicalRequest::Validate {
        graph: graph.clone(),
        candidate: candidate.clone(),
        requested: body.requested.clone(),
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
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::IdempotentWrite,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let candidate = CommitId::from_str(&candidate)
                    .map_err(|_| ApiError::invalid("invalid candidate commit id", &correlation))?;
                let canonical = canonical_accept(&graph, &candidate, &body, &correlation)?;
                // A named validation binds acceptance to that record and context (ADR-0019) under
                // every deployment policy. Without one, the deployment's policy travels with the
                // request; the store enforces it after the idempotent-replay lookup, so a durable
                // earlier acceptance still replays.
                let validation = match (&body.validation_id, &body.semantic_environment_id) {
                    (Some(validation_id), Some(semantic_environment_id)) => {
                        ValidationPolicy::Validated {
                            validation_id: validation_id.clone(),
                            semantic_environment_id: semantic_environment_id.clone(),
                        }
                    }
                    _ => match state.0.acceptance {
                        AcceptancePolicy::RequireValidation => ValidationPolicy::Required,
                        AcceptancePolicy::AllowUnvalidatedDevelopmentOnly => {
                            ValidationPolicy::NoValidation
                        }
                    },
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
            },
        )
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectBody {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub reason: String,
    /// A validation record the rejection cites (auditable; ADR-0019).
    #[serde(default)]
    pub validation_id: Option<ValidationId>,
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
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::IdempotentWrite,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let candidate = CommitId::from_str(&candidate)
                    .map_err(|_| ApiError::invalid("invalid candidate commit id", &correlation))?;
                let canonical = canonical_reject(&graph, &candidate, &body, &correlation)?;
                let request = RejectRequest {
                    scope: scope(&ctx, &graph, key, canonical.digest()),
                    branch: body.ref_name,
                    candidate,
                    reason: body.reason,
                    validation_id: body.validation_id,
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
            },
        )
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateBody {
    /// Context hints for the validator (all optional; request identity).
    #[serde(default)]
    pub requested: RequestedContext,
}

#[derive(Serialize)]
pub struct ValidationResponse {
    pub validation_id: ValidationId,
    pub semantic_context_id: SemanticContextId,
    /// What an accepting party names (ADR-0019).
    pub semantic_environment_id: SemanticEnvironmentId,
    pub candidate: CommitId,
    pub conforms: bool,
    pub record: ValidationRecord,
    pub context: SemanticExecutionContext,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replayed: Option<bool>,
    pub correlation_id: String,
}

fn busy(correlation: &str, what: &str) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "RESOURCE_LIMIT",
        format!("too many concurrent {what}; retry later"),
        correlation,
    )
}

/// Request a semantic validation of a prepared candidate (ADR-0014/0019). The ledger
/// reconstructs the candidate state (expensive slot), calls the configured validation
/// service (validation slot, bounded time and response), and records the immutable context
/// and record atomically with the idempotency result. It never moves a ref.
async fn validate(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path((graph, candidate)): Path<(String, String)>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<ValidateBody>,
) -> Result<(StatusCode, Json<ValidationResponse>), ApiError> {
    ctx.require(Capability::Validate)?;
    let correlation = ctx.correlation_id.clone();
    let key = idempotency_key(&headers, &correlation)?;
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::IdempotentWrite,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let candidate = CommitId::from_str(&candidate)
                    .map_err(|_| ApiError::invalid("invalid candidate commit id", &correlation))?;
                let limits = &state.0.limits;
                let canonical =
                    canonical_validate(&graph, &candidate, &body, limits, &correlation)?;
                let request = ValidateRequest {
                    scope: scope(&ctx, &graph, key, canonical.digest()),
                    candidate: candidate.clone(),
                    requested: body.requested,
                };
                // One logical invocation per (authenticated scope, key, canonical request): concurrent
                // duplicates and retries after a lost response or crash carry the same id, so the
                // validator resolves them to one validation in one environment (ADR-0019 amendment).
                let invocation_id = ValidationInvocation::for_request(
                    &request.scope.principal,
                    &request.scope.graph,
                    &request.scope.idempotency_key,
                    &request.scope.request_digest,
                )
                .id()
                .map_err(|e| ApiError::from_ledger(e.into(), &correlation))?;
                let store = state.0.store.validations();
                // A completed identical request replays before any admission slot or validator check.
                let replayed = store
                    .replayed(&request)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?;
                let begun = match replayed {
                    Some(recorded) => ValidationBegin::Replayed(Box::new(recorded)),
                    None => {
                        if state.0.validation.is_none() {
                            return Err(ApiError::from_ledger(
                                LedgerError::ValidatorUnavailable(
                                    "no validation service is configured".into(),
                                ),
                                &correlation,
                            ));
                        }
                        let _expensive = Arc::clone(&state.0.expensive)
                            .try_acquire_owned()
                            .map_err(|_| busy(&correlation, "expensive operations"))?;
                        // Never reconstruct more than may be shipped to the validator.
                        let mut bounds = limits.reconstruction;
                        bounds.max_bytes = bounds.max_bytes.min(limits.max_validation_state_bytes);
                        store
                            .begin_with_limits(&request, &bounds)
                            .await
                            .map_err(|e| ApiError::from_ledger(e, &correlation))?
                    }
                };
                let _validation_slot = match &begun {
                    ValidationBegin::Replayed(_) => None,
                    ValidationBegin::Fresh(_) => Some(
                        Arc::clone(&state.0.validations)
                            .try_acquire_owned()
                            .map_err(|_| busy(&correlation, "validations"))?,
                    ),
                };
                let ticket = match begun {
                    ValidationBegin::Replayed(recorded) => {
                        let recorded = *recorded;
                        let context = load_context(
                            &state,
                            &ctx,
                            &graph,
                            &recorded.validation_id,
                            &correlation,
                        )
                        .await?;
                        return Ok((
                            StatusCode::OK,
                            Json(ValidationResponse {
                                validation_id: recorded.validation_id,
                                semantic_context_id: recorded.context_id,
                                semantic_environment_id: recorded.environment_id,
                                candidate,
                                conforms: recorded.record.outcome.is_conforming(),
                                record: recorded.record,
                                context,
                                replayed: Some(true),
                                correlation_id: correlation,
                            }),
                        ));
                    }
                    ValidationBegin::Fresh(ticket) => ticket,
                };
                let Some(service) = state.0.validation.clone() else {
                    return Err(ApiError::from_ledger(
                        LedgerError::ValidatorUnavailable(
                            "no validation service is configured".into(),
                        ),
                        &correlation,
                    ));
                };
                let mut total = 0usize;
                let mut quads = Vec::with_capacity(ticket.state().len());
                for quad in ticket.state() {
                    let line = quad.to_string();
                    total += line.len() + 1;
                    if total > limits.max_validation_state_bytes {
                        return Err(ApiError::resource_limit(
                            format!(
                                "the candidate state exceeds the {} bytes shipped to the validator",
                                limits.max_validation_state_bytes
                            ),
                            &correlation,
                        ));
                    }
                    quads.push(line);
                }
                let mut outbound = ValidationRequest::new(
                    invocation_id,
                    CandidateDescriptor {
                        graph_id: graph.clone(),
                        knowledge_base_id: ticket.knowledge_base_id().map(str::to_owned),
                        commit: candidate.clone(),
                        state_digest: ticket.state_digest().clone(),
                        state_href: Some(format!("/v1/graphs/{graph}/commits/{candidate}/state")),
                        quads,
                    },
                    request.requested.clone(),
                );
                outbound.correlation_id = Some(correlation.clone());
                // The validator gets its own timeout, never more than the request's remaining budget
                // (ADR-0026 §8: the detached operation ends by request + one statement tail).
                let validator_deadline =
                    (Instant::now() + limits.validator_timeout).min(ctx.deadline);
                let answer = match tokio::time::timeout_at(
                    validator_deadline.into(),
                    service.client.validate(&outbound),
                )
                .await
                {
                    Ok(result) => result.map_err(LedgerError::from),
                    Err(_) => Err(LedgerError::ValidatorUnavailable(
                        "validator call exceeded the configured timeout".into(),
                    )),
                }
                .map_err(|e| ApiError::from_ledger(e, &correlation))?
                .with_bounded_summary();
                let context = answer
                    .into_context(
                        &graph,
                        &candidate,
                        ticket.state_digest(),
                        &service.service_id,
                        &request.requested,
                    )
                    .map_err(|e| {
                        ApiError::from_ledger(
                            LedgerError::ValidatorError(e.to_string()),
                            &correlation,
                        )
                    })?;
                let recorded = store
                    .record(
                        &request,
                        &ticket,
                        ValidatorOutcome {
                            context: context.clone(),
                            outcome: answer.outcome,
                            report_digest: answer.report.digest,
                            report_reference: answer.report.reference,
                        },
                    )
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?;
                let context = if recorded.replayed {
                    load_context(&state, &ctx, &graph, &recorded.validation_id, &correlation)
                        .await?
                } else {
                    context
                };
                Ok((
                    if recorded.replayed {
                        StatusCode::OK
                    } else {
                        StatusCode::CREATED
                    },
                    Json(ValidationResponse {
                        validation_id: recorded.validation_id,
                        semantic_context_id: recorded.context_id,
                        semantic_environment_id: recorded.environment_id,
                        candidate,
                        conforms: recorded.record.outcome.is_conforming(),
                        record: recorded.record,
                        context,
                        replayed: Some(recorded.replayed),
                        correlation_id: correlation,
                    }),
                ))
            },
        )
        .await
}

async fn load_context(
    state: &AppState,
    ctx: &RequestContext,
    graph: &GraphId,
    validation_id: &ValidationId,
    correlation: &str,
) -> Result<SemanticExecutionContext, ApiError> {
    state
        .0
        .store
        .validations()
        .load(&ctx.identity.principal.tenant_id, graph, validation_id)
        .await
        .map_err(|e| ApiError::from_ledger(e, correlation))?
        .map(|(_, context)| context)
        .ok_or_else(|| ApiError::from_ledger(LedgerError::ValidationNotFound, correlation))
}

/// Read one validation record of a candidate (read capability; tenant/graph scoped). A
/// record of another graph, tenant or candidate is VALIDATION_NOT_FOUND alike.
async fn read_validation(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path((graph, candidate, validation)): Path<(String, String, String)>,
) -> Result<Json<ValidationResponse>, ApiError> {
    ctx.require(Capability::Read)?;
    let correlation = ctx.correlation_id.clone();
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::Read,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let not_found =
                    || ApiError::from_ledger(LedgerError::ValidationNotFound, &correlation);
                let candidate = CommitId::from_str(&candidate).map_err(|_| not_found())?;
                let validation_id = ValidationId::from_str(&validation).map_err(|_| not_found())?;
                let (record, context) = state
                    .0
                    .store
                    .validations()
                    .load(&ctx.identity.principal.tenant_id, &graph, &validation_id)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?
                    .ok_or_else(not_found)?;
                if record.candidate_commit != candidate {
                    return Err(not_found());
                }
                let semantic_environment_id = context
                    .environment_id()
                    .map_err(|e| ApiError::from_ledger(e.into(), &correlation))?;
                Ok(Json(ValidationResponse {
                    semantic_context_id: record.semantic_execution_context_id.clone(),
                    semantic_environment_id,
                    validation_id,
                    candidate,
                    conforms: record.outcome.is_conforming(),
                    record,
                    context,
                    replayed: None,
                    correlation_id: correlation,
                }))
            },
        )
        .await
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
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::Read,
            &corr,
            async move {
                let state = detached;
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
            },
        )
        .await
}

// ---- branches (ADR-0022) ------------------------------------------------------------------

/// Visit bound and time budget for the branch-point reachability proof.
const BRANCH_POINT_MAX_VISITED: usize = 100_000;
const BRANCH_POINT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
/// Upper bound for history and log pages.
const MAX_HISTORY_PAGE: i64 = 1_000;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchPolicyBody {
    #[serde(default)]
    pub protected: bool,
    #[serde(default)]
    pub require_validation: bool,
    #[serde(default)]
    pub require_distinct_reviewer: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBranchBody {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub from_commit: Option<CommitId>,
    #[serde(default)]
    pub policy: BranchPolicyBody,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchLifecycleBody {
    pub name: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Validate a branch creation body and compute its canonical identity (branch request v1).
pub fn canonical_branch_create(
    graph: &GraphId,
    body: &CreateBranchBody,
    correlation: &str,
) -> Result<CanonicalRequest, ApiError> {
    check_branch(&body.name, correlation)?;
    check_branch(&body.source, correlation)?;
    Ok(CanonicalRequest::BranchCreate {
        graph: graph.clone(),
        name: body.name.clone(),
        source: body.source.clone(),
        from_commit: body.from_commit.clone(),
        protected: body.policy.protected,
        require_validation: body.policy.require_validation,
        require_distinct_reviewer: body.policy.require_distinct_reviewer,
    })
}

/// Validate a delete/restore body and compute its canonical identity (branch request v1).
pub fn canonical_branch_lifecycle(
    graph: &GraphId,
    delete: bool,
    body: &BranchLifecycleBody,
    correlation: &str,
) -> Result<CanonicalRequest, ApiError> {
    check_branch(&body.name, correlation)?;
    // An empty reason carries no meaning and is normalized to absence (ADR-0015).
    let reason = body.reason.clone().filter(|r| !r.is_empty());
    check_reason(reason.as_deref(), correlation)?;
    Ok(if delete {
        CanonicalRequest::BranchDelete {
            graph: graph.clone(),
            name: body.name.clone(),
            reason,
        }
    } else {
        CanonicalRequest::BranchRestore {
            graph: graph.clone(),
            name: body.name.clone(),
            reason,
        }
    })
}

#[derive(Serialize)]
pub struct BranchPolicyResponse {
    pub protected: bool,
    pub require_validation: bool,
    pub require_distinct_reviewer: bool,
}

impl From<ledger_store::BranchPolicy> for BranchPolicyResponse {
    fn from(p: ledger_store::BranchPolicy) -> Self {
        Self {
            protected: p.protected,
            require_validation: p.require_validation,
            require_distinct_reviewer: p.require_distinct_reviewer,
        }
    }
}

#[derive(Serialize)]
pub struct BranchEventResponse {
    pub event_id: i64,
    pub name: String,
    pub lifecycle_version: i64,
    pub operation: String,
    pub status: String,
    pub head: CommitId,
    pub version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<CommitId>,
    pub principal_id: String,
    pub principal_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub recorded_at: String,
}

impl From<ledger_store::BranchEvent> for BranchEventResponse {
    fn from(e: ledger_store::BranchEvent) -> Self {
        Self {
            event_id: e.event_id,
            name: e.branch,
            lifecycle_version: e.lifecycle_version,
            operation: e.operation,
            status: e.status_after,
            head: e.head,
            version: e.ref_version,
            source: e.source_branch,
            source_commit: e.source_commit,
            principal_id: e.principal_id,
            principal_type: e.principal_type,
            on_behalf_of: e.on_behalf_of,
            reason: e.reason,
            recorded_at: e.recorded_at,
        }
    }
}

#[derive(Serialize)]
pub struct BranchLifecycleResponse {
    pub event: BranchEventResponse,
    pub replayed: bool,
    pub correlation_id: String,
}

#[derive(Serialize)]
pub struct BranchResponse {
    pub name: String,
    pub status: String,
    pub lifecycle_version: i64,
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<CommitId>,
    pub head: CommitId,
    pub version: i64,
    pub policy: BranchPolicyResponse,
    pub created_at: String,
}

impl From<ledger_store::BranchInfo> for BranchResponse {
    fn from(b: ledger_store::BranchInfo) -> Self {
        Self {
            name: b.name,
            status: b.status,
            lifecycle_version: b.lifecycle_version,
            origin: b.origin,
            source: b.source_branch,
            source_commit: b.source_commit,
            head: b.head,
            version: b.version,
            policy: b.policy.into(),
            created_at: b.created_at,
        }
    }
}

#[derive(Serialize)]
pub struct BranchListResponse {
    pub branches: Vec<BranchResponse>,
}

#[derive(Serialize)]
pub struct RefMovementResponse {
    pub event_id: i64,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_head: Option<CommitId>,
    pub new_head: CommitId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_version: Option<i64>,
    pub new_version: i64,
    pub principal_id: String,
    pub principal_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub recorded_at: String,
}

#[derive(Serialize)]
pub struct BranchHistoryResponse {
    pub name: String,
    /// The latest `limit` lifecycle events (created / deleted / restored …), oldest first.
    pub lifecycle: Vec<BranchEventResponse>,
    /// Head movements (ref events), newest first.
    pub movements: Vec<RefMovementResponse>,
}

#[derive(Serialize)]
pub struct BranchLogResponse {
    pub name: String,
    pub head: CommitId,
    /// First-parent commit history from the head, head first.
    pub commits: Vec<CommitId>,
}

// ---------------------------------------------------------------------------------------
// Merges (ADR-0023 / ADR-0024): preview (read), propose (persist), apply (accept)

/// Ancestry walks of a merge are bounded like a historical branch point.
const MERGE_MAX_VISITED: usize = 100_000;
/// At most this many best common ancestors are listed in a preview (the count is exact).
pub const MAX_LISTED_MERGE_BASE_CANDIDATES: usize = 64;
const MERGE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
/// Expensive-operation slots one merge preview or propose takes (three to four state
/// reconstructions instead of one).
const MERGE_PERMITS: u32 = 3;

/// [`MERGE_PERMITS`], never more than the configured total (a deployment with fewer slots
/// still runs merges, one at a time).
fn merge_permits(state: &AppState) -> u32 {
    let total = u32::try_from(state.0.limits.max_concurrent_expensive).unwrap_or(u32::MAX);
    MERGE_PERMITS.min(total).max(1)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergePreviewBody {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub base: Option<CommitId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeProposeBody {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub base: Option<CommitId>,
    pub preview_token: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeApplyBody {
    pub proposal_id: i64,
    pub preview_token: String,
    #[serde(default)]
    pub validation_id: Option<ValidationId>,
    #[serde(default)]
    pub semantic_environment_id: Option<SemanticEnvironmentId>,
    #[serde(default)]
    pub reason: Option<String>,
}

fn merge_strategy(
    value: Option<&str>,
    correlation: &str,
) -> Result<ledger_store::MergeStrategy, ApiError> {
    match value {
        None => Ok(ledger_store::MergeStrategy::Abort),
        Some(v) => ledger_store::MergeStrategy::parse(v).ok_or_else(|| {
            ApiError::invalid(
                "`strategy` must be abort, take-target, take-source or union",
                correlation,
            )
        }),
    }
}

fn merge_spec(
    source: &str,
    target: &str,
    strategy: Option<&str>,
    base: Option<&CommitId>,
    correlation: &str,
) -> Result<ledger_store::MergeSpec, ApiError> {
    check_branch(source, correlation)?;
    check_branch(target, correlation)?;
    if source == target {
        return Err(ApiError::invalid(
            "`source` and `target` must be different branches",
            correlation,
        ));
    }
    Ok(ledger_store::MergeSpec {
        source: source.to_owned(),
        target: target.to_owned(),
        strategy: merge_strategy(strategy, correlation)?,
        base: base.cloned(),
    })
}

/// Validate a propose body and compute its canonical identity (merge request v1).
pub fn canonical_merge_propose(
    graph: &GraphId,
    body: &MergeProposeBody,
    correlation: &str,
) -> Result<CanonicalRequest, ApiError> {
    let spec = merge_spec(
        &body.source,
        &body.target,
        body.strategy.as_deref(),
        body.base.as_ref(),
        correlation,
    )?;
    check_text("preview_token", &body.preview_token, 71, true, correlation)?;
    let message = body.message.clone().filter(|m| !m.is_empty());
    if let Some(m) = &message {
        check_text(
            "message",
            m,
            ledger_core::MAX_MESSAGE_BYTES,
            true,
            correlation,
        )?;
    }
    let distinct: std::collections::BTreeSet<&str> =
        body.evidence_refs.iter().map(String::as_str).collect();
    if distinct.len() > ledger_core::MAX_EVIDENCE_REFS {
        return Err(ApiError::invalid(
            format!(
                "at most {} distinct evidence references",
                ledger_core::MAX_EVIDENCE_REFS
            ),
            correlation,
        ));
    }
    for e in &body.evidence_refs {
        check_text(
            "evidence_refs",
            e,
            ledger_core::MAX_IDENTIFIER_BYTES,
            true,
            correlation,
        )?;
    }
    Ok(CanonicalRequest::MergePropose {
        graph: graph.clone(),
        source: spec.source,
        target: spec.target,
        strategy: spec.strategy.as_str().to_owned(),
        base: spec.base,
        preview_token: body.preview_token.clone(),
        message,
        evidence_refs: body.evidence_refs.clone(),
    })
}

/// Validate an apply body and compute its canonical identity (merge request v1).
pub fn canonical_merge_apply(
    graph: &GraphId,
    body: &MergeApplyBody,
    correlation: &str,
) -> Result<CanonicalRequest, ApiError> {
    if body.proposal_id < 1 {
        return Err(ApiError::invalid(
            "`proposal_id` must be positive",
            correlation,
        ));
    }
    check_text("preview_token", &body.preview_token, 71, true, correlation)?;
    if body.validation_id.is_some() != body.semantic_environment_id.is_some() {
        return Err(ApiError::invalid(
            "`validation_id` and `semantic_environment_id` are given together or not at all",
            correlation,
        ));
    }
    let reason = body.reason.clone().filter(|r| !r.is_empty());
    check_reason(reason.as_deref(), correlation)?;
    Ok(CanonicalRequest::MergeApply {
        graph: graph.clone(),
        proposal_id: body.proposal_id,
        preview_token: body.preview_token.clone(),
        validation_id: body.validation_id.clone(),
        semantic_environment_id: body.semantic_environment_id.clone(),
        reason,
    })
}

#[derive(Serialize)]
pub struct DeltaSummaryResponse {
    pub adds: usize,
    pub deletes: usize,
    pub affected_keys: usize,
}

impl From<ledger_store::DeltaSummary> for DeltaSummaryResponse {
    fn from(d: ledger_store::DeltaSummary) -> Self {
        Self {
            adds: d.adds,
            deletes: d.deletes,
            affected_keys: d.affected_keys,
        }
    }
}

#[derive(Serialize)]
pub struct ConflictSideResponse {
    pub quads: Vec<String>,
    pub truncated: bool,
}

#[derive(Serialize)]
pub struct ConflictResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<String>,
    pub subject: String,
    pub predicate: String,
    pub base: ConflictSideResponse,
    pub target: ConflictSideResponse,
    pub source: ConflictSideResponse,
}

fn side(s: &ledger_merge_side::Side) -> ConflictSideResponse {
    ConflictSideResponse {
        quads: s.quads.iter().map(ToString::to_string).collect(),
        truncated: s.truncated,
    }
}

mod ledger_merge_side {
    pub use ledger_store::MergeSide as Side;
}

#[derive(Serialize)]
pub struct MergePreviewResponse {
    pub classification: &'static str,
    pub source_head: CommitId,
    pub target_head: CommitId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_base: Option<CommitId>,
    /// The first [`MAX_LISTED_MERGE_BASE_CANDIDATES`] best common ancestors of an
    /// ambiguous history, ascending.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub merge_base_candidates: Vec<CommitId>,
    /// Exact number of best common ancestors (ambiguous histories only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_base_candidate_count: Option<usize>,
    pub ahead: usize,
    pub behind: usize,
    pub target_delta: DeltaSummaryResponse,
    pub source_delta: DeltaSummaryResponse,
    pub strategy: &'static str,
    /// Exact total of conflicting keys.
    pub conflict_count: usize,
    /// A deterministic prefix of the conflicts, bounded by count and by
    /// `max_merge_conflict_report_bytes`.
    pub conflicts: Vec<ConflictResponse>,
    /// `conflicts` is incomplete: a limit (entries, quads per side, bytes) left a conflict
    /// or a quad out. `false` means every conflicting key is listed in full.
    pub conflicts_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_state_digest: Option<ContentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_token: Option<String>,
    pub correlation_id: String,
}

async fn merge_preview(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    ValidJson(body): ValidJson<MergePreviewBody>,
) -> Result<Json<MergePreviewResponse>, ApiError> {
    ctx.require(Capability::Read)?;
    let correlation = ctx.correlation_id.clone();
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::Read,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let spec = merge_spec(
                    &body.source,
                    &body.target,
                    body.strategy.as_deref(),
                    body.base.as_ref(),
                    &correlation,
                )?;
                // A merge preview reconstructs three states (and a propose four): it takes
                // `MERGE_PERMITS` expensive slots, so merges cannot crowd out single-state reads.
                let _permit = state
                    .0
                    .expensive
                    .clone()
                    .try_acquire_many_owned(merge_permits(&state))
                    .map_err(|_| {
                        ApiError::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "RESOURCE_LIMIT",
                            "too many concurrent expensive operations; retry later",
                            &correlation,
                        )
                    })?;
                let limits = ledger_store::TraversalLimits {
                    max_visited: MERGE_MAX_VISITED,
                    deadline: Some(std::time::Instant::now() + MERGE_DEADLINE),
                };
                let report = state
                    .0
                    .limits
                    .merge_report_limits()
                    .expect("validated in AppState::new");
                let p = state
                    .0
                    .store
                    .workflows()
                    .merge_preview_reported(
                        &ctx.identity.principal.tenant_id,
                        &graph,
                        &spec,
                        limits,
                        report,
                    )
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?;
                // A deliberately built criss-cross can have many best common ancestors: list a bounded
                // prefix (the order is ascending, so any listed one can be named as `base`) and the count.
                let (candidates, candidate_count) = match &p.class {
                    ledger_store::MergeClass::AmbiguousMergeBase(c) => (
                        c.iter()
                            .take(MAX_LISTED_MERGE_BASE_CANDIDATES)
                            .cloned()
                            .collect(),
                        Some(c.len()),
                    ),
                    _ => (Vec::new(), None),
                };
                Ok(Json(MergePreviewResponse {
                    classification: p.class.as_str(),
                    source_head: p.source_head,
                    target_head: p.target_head,
                    merge_base: p.merge_base,
                    merge_base_candidates: candidates,
                    merge_base_candidate_count: candidate_count,
                    ahead: p.ahead,
                    behind: p.behind,
                    target_delta: p.target_delta.into(),
                    source_delta: p.source_delta.into(),
                    strategy: p.strategy.as_str(),
                    conflict_count: p.conflict_count,
                    conflicts: p
                        .conflicts
                        .iter()
                        .map(|c| ConflictResponse {
                            graph: c.key.graph.clone(),
                            subject: c.key.subject.clone(),
                            predicate: c.key.predicate.clone(),
                            base: side(&c.base),
                            target: side(&c.target),
                            source: side(&c.source),
                        })
                        .collect(),
                    conflicts_truncated: p.conflicts_truncated,
                    merged_state_digest: p.merged_state_digest,
                    preview_token: p.preview_token,
                    correlation_id: correlation,
                }))
            },
        )
        .await
}

#[derive(Serialize)]
pub struct MergeProposeResponse {
    pub proposal_id: i64,
    pub candidate: CommitId,
    pub target_head: CommitId,
    pub source_head: CommitId,
    pub merge_base: CommitId,
    pub classification: String,
    pub strategy: String,
    pub conflict_count: i32,
    pub merged_state_digest: ContentId,
    pub preview_token: String,
    pub replayed: bool,
    pub correlation_id: String,
}

async fn merge_propose(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<MergeProposeBody>,
) -> Result<(StatusCode, Json<MergeProposeResponse>), ApiError> {
    ctx.require(Capability::Propose)?;
    let correlation = ctx.correlation_id.clone();
    let key = idempotency_key(&headers, &correlation)?;
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::IdempotentWrite,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let canonical = canonical_merge_propose(&graph, &body, &correlation)?;
                let spec = merge_spec(
                    &body.source,
                    &body.target,
                    body.strategy.as_deref(),
                    body.base.as_ref(),
                    &correlation,
                )?;
                let request = ledger_store::ProposeMergeRequest {
                    scope: scope(&ctx, &graph, key, canonical.digest()),
                    spec,
                    preview_token: body.preview_token,
                    message: body.message.unwrap_or_default(),
                    evidence_refs: body.evidence_refs,
                };
                // A completed propose replays before any admission permit is taken.
                if let Some(p) = state
                    .0
                    .store
                    .workflows()
                    .stored_merge_proposal(&request.scope)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?
                {
                    return Ok((StatusCode::OK, Json(propose_response(p, correlation))));
                }
                let _permit = state
                    .0
                    .expensive
                    .clone()
                    .try_acquire_many_owned(merge_permits(&state))
                    .map_err(|_| {
                        ApiError::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "RESOURCE_LIMIT",
                            "too many concurrent expensive operations; retry later",
                            &correlation,
                        )
                    })?;
                let limits = ledger_store::TraversalLimits {
                    max_visited: MERGE_MAX_VISITED,
                    deadline: Some(std::time::Instant::now() + MERGE_DEADLINE),
                };
                let p = state
                    .0
                    .store
                    .workflows()
                    .merge_propose(&request, limits)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?;
                let status = if p.replayed {
                    StatusCode::OK
                } else {
                    StatusCode::CREATED
                };
                Ok((status, Json(propose_response(p, correlation))))
            },
        )
        .await
}

fn propose_response(p: ledger_store::MergeProposed, correlation: String) -> MergeProposeResponse {
    MergeProposeResponse {
        proposal_id: p.proposal_id,
        candidate: p.candidate,
        target_head: p.target_head,
        source_head: p.source_head,
        merge_base: p.merge_base,
        classification: p.classification,
        strategy: p.strategy,
        conflict_count: p.conflict_count,
        merged_state_digest: p.merged_state_digest,
        preview_token: p.preview_token,
        replayed: p.replayed,
        correlation_id: correlation,
    }
}

async fn merge_apply(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<MergeApplyBody>,
) -> Result<Json<AcceptResponse>, ApiError> {
    ctx.require(Capability::Review)?;
    let correlation = ctx.correlation_id.clone();
    let key = idempotency_key(&headers, &correlation)?;
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::IdempotentWrite,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let canonical = canonical_merge_apply(&graph, &body, &correlation)?;
                // As for accept: a named validation binds the merge to that record (ADR-0019);
                // otherwise the deployment policy travels with the request and the store enforces it
                // after the replay lookup.
                let validation = match (&body.validation_id, &body.semantic_environment_id) {
                    (Some(validation_id), Some(semantic_environment_id)) => {
                        ValidationPolicy::Validated {
                            validation_id: validation_id.clone(),
                            semantic_environment_id: semantic_environment_id.clone(),
                        }
                    }
                    _ => match state.0.acceptance {
                        AcceptancePolicy::RequireValidation => ValidationPolicy::Required,
                        AcceptancePolicy::AllowUnvalidatedDevelopmentOnly => {
                            ValidationPolicy::NoValidation
                        }
                    },
                };
                let request = ledger_store::ApplyMergeRequest {
                    scope: scope(&ctx, &graph, key, canonical.digest()),
                    proposal_id: body.proposal_id,
                    preview_token: body.preview_token,
                    reason: body.reason.filter(|r| !r.is_empty()),
                    validation,
                };
                let applied = state
                    .0
                    .store
                    .workflows()
                    .merge_apply(&request)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?;
                Ok(Json(AcceptResponse {
                    decision_id: applied.decision_id,
                    ref_event_id: applied.ref_event_id,
                    outbox_id: applied.outbox_id,
                    ref_version: applied.ref_version,
                    head: applied.head,
                    replayed: applied.replayed,
                    correlation_id: correlation,
                }))
            },
        )
        .await
}

fn query_name(query: &BTreeMap<String, String>, correlation: &str) -> Result<String, ApiError> {
    let name = query.get("name").cloned().ok_or_else(|| {
        ApiError::invalid(
            "query parameter `name` (branch name) is required",
            correlation,
        )
    })?;
    check_branch(&name, correlation)?;
    Ok(name)
}

fn query_limit(query: &BTreeMap<String, String>, correlation: &str) -> Result<i64, ApiError> {
    match query.get("limit") {
        None => Ok(100),
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=MAX_HISTORY_PAGE).contains(n))
            .ok_or_else(|| {
                ApiError::invalid(
                    format!("`limit` must be an integer between 1 and {MAX_HISTORY_PAGE}"),
                    correlation,
                )
            }),
    }
}

async fn create_branch(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<CreateBranchBody>,
) -> Result<(StatusCode, Json<BranchLifecycleResponse>), ApiError> {
    ctx.require(Capability::Propose)?;
    // A protected branch is an administrative decision (ADR-0022).
    if body.policy.protected {
        ctx.require(Capability::Admin)?;
    }
    let correlation = ctx.correlation_id.clone();
    let key = idempotency_key(&headers, &correlation)?;
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::IdempotentWrite,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let canonical = canonical_branch_create(&graph, &body, &correlation)?;
                let request = ledger_store::CreateBranchRequest {
                    scope: scope(&ctx, &graph, key, canonical.digest()),
                    name: body.name,
                    source: body.source,
                    from_commit: body.from_commit,
                    policy: ledger_store::BranchPolicy {
                        protected: body.policy.protected,
                        require_validation: body.policy.require_validation,
                        require_distinct_reviewer: body.policy.require_distinct_reviewer,
                    },
                };
                let limits = ledger_store::TraversalLimits {
                    max_visited: BRANCH_POINT_MAX_VISITED,
                    deadline: Some(std::time::Instant::now() + BRANCH_POINT_DEADLINE),
                };
                let outcome = state
                    .0
                    .store
                    .workflows()
                    .create_branch(&request, limits)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?;
                let status = if outcome.replayed {
                    StatusCode::OK
                } else {
                    StatusCode::CREATED
                };
                Ok((
                    status,
                    Json(BranchLifecycleResponse {
                        event: outcome.event.into(),
                        replayed: outcome.replayed,
                        correlation_id: correlation,
                    }),
                ))
            },
        )
        .await
}

async fn change_branch(
    state: AppState,
    ctx: RequestContext,
    graph: String,
    headers: HeaderMap,
    body: BranchLifecycleBody,
    delete: bool,
) -> Result<Json<BranchLifecycleResponse>, ApiError> {
    // Deletion and restore are administrative (ADR-0022).
    ctx.require(Capability::Admin)?;
    let correlation = ctx.correlation_id.clone();
    let key = idempotency_key(&headers, &correlation)?;
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::IdempotentWrite,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let canonical = canonical_branch_lifecycle(&graph, delete, &body, &correlation)?;
                let request = ledger_store::BranchLifecycleRequest {
                    scope: scope(&ctx, &graph, key, canonical.digest()),
                    name: body.name,
                    reason: body.reason.filter(|r| !r.is_empty()),
                };
                let repo = state.0.store.workflows();
                let outcome = if delete {
                    repo.delete_branch(&request).await
                } else {
                    repo.restore_branch(&request).await
                }
                .map_err(|e| ApiError::from_ledger(e, &correlation))?;
                Ok(Json(BranchLifecycleResponse {
                    event: outcome.event.into(),
                    replayed: outcome.replayed,
                    correlation_id: correlation,
                }))
            },
        )
        .await
}

async fn delete_branch(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<BranchLifecycleBody>,
) -> Result<Json<BranchLifecycleResponse>, ApiError> {
    change_branch(state, ctx, graph, headers, body, true).await
}

async fn restore_branch(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    headers: HeaderMap,
    ValidJson(body): ValidJson<BranchLifecycleBody>,
) -> Result<Json<BranchLifecycleResponse>, ApiError> {
    change_branch(state, ctx, graph, headers, body, false).await
}

async fn list_branches(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Result<Json<BranchListResponse>, ApiError> {
    ctx.require(Capability::Read)?;
    let correlation = ctx.correlation_id.clone();
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::Read,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let limit = query_limit(&query, &correlation)?;
                let branches = state
                    .0
                    .store
                    .workflows()
                    .branches(&ctx.identity.principal.tenant_id, &graph, limit)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?;
                Ok(Json(BranchListResponse {
                    branches: branches.into_iter().map(Into::into).collect(),
                }))
            },
        )
        .await
}

async fn branch_status(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Result<Json<BranchResponse>, ApiError> {
    ctx.require(Capability::Read)?;
    let correlation = ctx.correlation_id.clone();
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::Read,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let name = query_name(&query, &correlation)?;
                state
                    .0
                    .store
                    .workflows()
                    .branch(&ctx.identity.principal.tenant_id, &graph, &name)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?
                    .map(|b| Json(b.into()))
                    .ok_or_else(|| {
                        ApiError::from_ledger(LedgerError::BranchNotFound(name), &correlation)
                    })
            },
        )
        .await
}

async fn branch_history(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Result<Json<BranchHistoryResponse>, ApiError> {
    ctx.require(Capability::Read)?;
    let correlation = ctx.correlation_id.clone();
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::Read,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let name = query_name(&query, &correlation)?;
                let limit = query_limit(&query, &correlation)?;
                let (lifecycle, movements) = state
                    .0
                    .store
                    .workflows()
                    .branch_history(&ctx.identity.principal.tenant_id, &graph, &name, limit)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?
                    .ok_or_else(|| {
                        ApiError::from_ledger(
                            LedgerError::BranchNotFound(name.clone()),
                            &correlation,
                        )
                    })?;
                Ok(Json(BranchHistoryResponse {
                    name,
                    lifecycle: lifecycle.into_iter().map(Into::into).collect(),
                    movements: movements
                        .into_iter()
                        .map(|m| RefMovementResponse {
                            event_id: m.event_id,
                            operation: m.operation,
                            old_head: m.old_head,
                            new_head: m.new_head,
                            old_version: m.old_version,
                            new_version: m.new_version,
                            principal_id: m.principal_id,
                            principal_type: m.principal_type,
                            on_behalf_of: m.on_behalf_of,
                            reason: m.reason,
                            recorded_at: m.recorded_at,
                        })
                        .collect(),
                }))
            },
        )
        .await
}

async fn branch_log(
    State(state): State<AppState>,
    ctx: RequestContext,
    Path(graph): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Result<Json<BranchLogResponse>, ApiError> {
    ctx.require(Capability::Read)?;
    let correlation = ctx.correlation_id.clone();
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::Read,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                let name = query_name(&query, &correlation)?;
                let limit = query_limit(&query, &correlation)?;
                let tenant = &ctx.identity.principal.tenant_id;
                let repo = state.0.store.workflows();
                let branch = repo
                    .branch(tenant, &graph, &name)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?
                    .ok_or_else(|| {
                        ApiError::from_ledger(
                            LedgerError::BranchNotFound(name.clone()),
                            &correlation,
                        )
                    })?;
                let commits = repo
                    .first_parent_history(
                        tenant,
                        &graph,
                        &branch.head,
                        limit as usize,
                        ledger_store::TraversalLimits {
                            max_visited: MAX_HISTORY_PAGE as usize,
                            deadline: Some(std::time::Instant::now() + BRANCH_POINT_DEADLINE),
                        },
                    )
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?;
                Ok(Json(BranchLogResponse {
                    name,
                    head: branch.head,
                    commits,
                }))
            },
        )
        .await
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
    // From the graph lookup on, every statement belongs to one tracked, detached
    // operation the edge may stop waiting for but never drops (ADR-0026 §8).
    let detached = state.clone();
    let corr = correlation.clone();
    state
        .run_db(
            ctx.deadline,
            lifecycle::OperationClass::Read,
            &corr,
            async move {
                let state = detached;
                let graph = authorized_graph(&state, &ctx, &graph).await?;
                // Malformed ids, unknown commits and commits of other graphs are all NOT_FOUND.
                let commit =
                    CommitId::from_str(&commit).map_err(|_| ApiError::not_found(&correlation))?;
                let member = state
                    .0
                    .store
                    .commit_graph(&commit)
                    .await
                    .map_err(|e| ApiError::from_ledger(e, &correlation))?;
                if member.as_ref() != Some(&graph) {
                    return Err(ApiError::not_found(&correlation));
                }
                let _permit = Arc::clone(&state.0.expensive)
                    .try_acquire_owned()
                    .map_err(|_| {
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
                let quads =
                    export_quads(quads, state.0.limits.max_state_export_bytes, &correlation)?;
                Ok(Json(StateResponse { commit, quads }))
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_conflict_report_budget_is_validated_never_clamped() {
        assert!(ApiLimits::default().validate().is_ok());
        assert_eq!(
            ApiLimits::default().max_merge_conflict_report_bytes,
            ledger_store::MergeReportLimits::DEFAULT.max_bytes()
        );
        for bad in [0, 1, 1023, 64 * 1024 * 1024 + 1, usize::MAX] {
            let limits = ApiLimits {
                max_merge_conflict_report_bytes: bad,
                ..ApiLimits::default()
            };
            assert!(limits.validate().is_err(), "{bad} accepted");
        }
        for good in [1024, 64 * 1024 * 1024] {
            let limits = ApiLimits {
                max_merge_conflict_report_bytes: good,
                ..ApiLimits::default()
            };
            assert!(limits.validate().is_ok(), "{good} refused");
        }
    }

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
            "VALIDATION_REJECTED",
            "VALIDATION_STALE",
            "VALIDATION_NOT_FOUND",
            "VALIDATOR_UNAVAILABLE",
            "VALIDATOR_ERROR",
            "RESOURCE_LIMIT",
            "DEPENDENCY_UNAVAILABLE",
            "DEPENDENCY_TIMEOUT",
            "REQUEST_TIMEOUT",
            "INTERNAL",
            "BRANCH_NOT_FOUND",
            "BRANCH_EXISTS",
            "BRANCH_DELETED",
            "BRANCH_POINT_UNREACHABLE",
            "BRANCH_POLICY_VIOLATION",
            "BRANCH_STATE_CONFLICT",
            "MERGE_STALE",
            "MERGE_CONFLICT",
            "MERGE_NOTHING_TO_DO",
            "INVALID_MERGE_BASE",
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
                    .replace("{validation}", &format!("sha256:{}", "b".repeat(64)))
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

    /// A request stuck before any database work (here: in authentication) is answered by
    /// the edge with `REQUEST_TIMEOUT` (ADR-0026 §4), classified by the request alone: a
    /// key-less read is told nothing was written and never to retry with a key.
    #[tokio::test]
    async fn requests_exceeding_the_time_limit_get_a_request_timeout_envelope_by_class() {
        let limits = ApiLimits {
            request_timeout: Duration::from_millis(50),
            ..ApiLimits::default()
        };
        let app = router(lazy_state(Arc::new(NeverAnswers), limits));
        let started = Instant::now();
        let (status, body, correlation) =
            send(&app, "GET", "/v1/graphs/g1/refs?name=main", Some("t")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["code"], "REQUEST_TIMEOUT");
        let message = body["message"].as_str().unwrap();
        assert!(
            message.contains("nothing was written") && !message.contains("idempotency"),
            "{message}"
        );
        assert_eq!(body["correlation_id"], correlation.unwrap());
        // The edge grace is the only budget before database work: ≈ request_timeout + 1 s.
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(3),
            "{elapsed:?}"
        );
    }

    /// An error from COMMIT itself (ADR-0026 §4) carries the outcome-unknown guidance under
    /// the inner class's code; a key-less read's dependency guidance never names a key.
    #[test]
    fn commit_outcome_unknown_and_read_guidance_map_as_the_adr_says() {
        let timeout = ApiError::from_ledger(
            LedgerError::CommitOutcomeUnknown(Box::new(LedgerError::DependencyTimeout("x".into()))),
            "c",
        );
        assert_eq!(
            (timeout.status, timeout.code),
            (StatusCode::SERVICE_UNAVAILABLE, "DEPENDENCY_TIMEOUT")
        );
        assert!(
            timeout.message.contains("outcome is unknown")
                && timeout.message.contains("same idempotency key"),
            "{}",
            timeout.message
        );
        let lost = ApiError::from_ledger(
            LedgerError::CommitOutcomeUnknown(Box::new(LedgerError::DependencyUnavailable(
                "x".into(),
            ))),
            "c",
        );
        assert_eq!(lost.code, "DEPENDENCY_UNAVAILABLE");
        assert!(lost.message.contains("outcome is unknown"));
        let read = ApiError::from_ledger(LedgerError::DependencyUnavailable("x".into()), "c")
            .for_class(lifecycle::OperationClass::Read);
        assert!(
            !read.message.contains("idempotency") && read.message.contains("nothing was written"),
            "{}",
            read.message
        );
        let write = ApiError::from_ledger(LedgerError::DependencyUnavailable("x".into()), "c")
            .for_class(lifecycle::OperationClass::IdempotentWrite);
        assert!(
            write.message.contains("same idempotency key"),
            "{}",
            write.message
        );
    }

    #[tokio::test]
    async fn filesystem_router_is_read_only_bounded_and_enveloped() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(Ledger::open(dir.path()).unwrap());
        let app = filesystem_readonly_router(
            ledger,
            ApiLimits {
                max_state_export_bytes: 10,
                ..ApiLimits::default()
            },
        );
        let (status, body, correlation) = send(&app, "GET", "/v1/refs/main", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["head"].is_null());
        assert!(correlation.is_some());
        let (status, body, _) = send(&app, "GET", "/v1/states/not-a-commit", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "NOT_FOUND");
        let unknown = format!("/v1/states/sha256:{}", "a".repeat(64));
        let (status, body, _) = send(&app, "GET", &unknown, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["code"], "NOT_FOUND");
        // No write surface at all, and unknown routes use the envelope.
        for (method, path) in [("POST", "/v1/commits"), ("POST", "/v1/graphs/g/proposals")] {
            let (status, body, _) = send(&app, method, path, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
            assert_eq!(body["code"], "NOT_FOUND");
        }
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_and_other_schemes_are_not_bearer() {
        assert_eq!(bearer_credential("Bearer abc.def"), Some("abc.def"));
        assert_eq!(bearer_credential("bearer abc.def"), Some("abc.def"));
        assert_eq!(bearer_credential("BEARER  abc.def "), Some("abc.def"));
        assert_eq!(bearer_credential("Basic abc"), None);
        assert_eq!(bearer_credential("Token abc"), None);
        assert_eq!(bearer_credential("Bearer"), None);
        assert_eq!(bearer_credential("Bearer "), None);
        assert_eq!(bearer_credential("Bearer a b"), None);
        assert_eq!(bearer_credential("Bearerabc"), None);
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
