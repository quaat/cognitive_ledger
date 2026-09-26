use ledger_api::{
    AcceptancePolicy, ApiLimits, AppState,
    auth::{
        Capability, ClaimsPolicy, DevHs256Authenticator, OidcAuthenticator, SharedAuthenticator,
    },
};
use ledger_store::{Ledger, PostgresLedgerStore, ReconstructionLimits, V1Binding};
use std::{
    collections::BTreeSet,
    env,
    future::Future,
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};
use tracing::{info, warn};

/// How long open connections may drain after a shutdown signal before the process exits
/// anyway. An unfinished publication transaction is rolled back by PostgreSQL, so exiting
/// never leaves partial state.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// The only value of `LEDGER_UNVALIDATED_ACCEPTANCE` that enables acceptance without
/// semantic validation. Deliberately long and self-describing so it cannot be set by
/// accident or mistaken for a production knob.
const UNVALIDATED_ACCEPTANCE_SWITCH: &str = "allow-unvalidated-acceptance-development-only";
/// The only value of `LEDGER_ALLOW_INSECURE_NON_LOOPBACK` that permits a non-loopback bind
/// with a non-production authenticator (containerised CI on an isolated network).
const INSECURE_BIND_SWITCH: &str = "allow-insecure-non-loopback-development-only";

/// Which persistence topology the environment selects (ADR-0012).
#[derive(Clone, Debug, Eq, PartialEq)]
enum Backend {
    /// No database: filesystem refs and objects, read-only inspection (development only).
    FilesystemOnly,
    /// PostgreSQL refs and PostgreSQL immutable objects — the shared, multi-replica-safe
    /// topology whenever a database URL is present (and, since migration 0006, the only
    /// one PostgreSQL refs permit).
    SharedPostgres,
}

/// Pure selection so the defaulting rules are unit-testable:
///
/// ```text
/// url absent,  backend absent|filesystem → filesystem development mode
/// url absent,  backend postgres          → configuration error (no silent fallback)
/// url present, backend absent|postgres   → shared PostgreSQL
/// url present, backend filesystem        → configuration error since migration 0006
/// url ""                                 → configuration error (set it or unset it)
/// ```
fn select_backend(database_url: Option<&str>, backend: Option<&str>) -> Result<Backend, String> {
    if database_url == Some("") {
        return Err(
            "LEDGER_DATABASE_URL is set but empty; unset it for filesystem development mode or \
             provide a database URL"
                .into(),
        );
    }
    match (database_url, backend) {
        (None, None | Some("filesystem")) => Ok(Backend::FilesystemOnly),
        (None, Some("postgres")) => Err(
            "LEDGER_IMMUTABLE_BACKEND=postgres requires LEDGER_DATABASE_URL; refusing to fall back \
             to node-local filesystem objects"
                .into(),
        ),
        (Some(_), None | Some("postgres")) => Ok(Backend::SharedPostgres),
        // Since migration 0006 a PostgreSQL ref must point at an indexed commit of its
        // graph (refs → commit_index FK), so node-local objects behind shared refs cannot
        // work at all: refuse instead of failing on the first commit.
        (Some(_), Some("filesystem")) => Err(
            "LEDGER_IMMUTABLE_BACKEND=filesystem with LEDGER_DATABASE_URL is no longer supported \
             (migration 0006 requires every PostgreSQL ref to target an indexed commit); use the \
             shared PostgreSQL backend, or unset LEDGER_DATABASE_URL for filesystem-only \
             development mode"
                .into(),
        ),
        (_, Some(other)) => Err(format!(
            "LEDGER_IMMUTABLE_BACKEND must be 'postgres' (default with a database URL) or \
             'filesystem', got {other:?}"
        )),
    }
}

/// An optional environment variable that must be valid Unicode when present: a value the
/// process cannot read is a configuration error, never "unset".
fn env_optional(name: &str) -> Result<Option<String>, String> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid Unicode")),
    }
}

fn env_usize(name: &str, default: usize) -> Result<usize, String> {
    match env_optional(name)? {
        None => Ok(default),
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("{name} must be a positive integer, got {v:?}")),
    }
}

fn env_set(name: &str) -> Result<BTreeSet<String>, String> {
    Ok(env_optional(name)?
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Whether every address the bind string resolves to is loopback. Unresolvable strings
/// are treated as non-loopback (fail closed).
fn is_loopback_bind(address: &str) -> bool {
    match address.to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<SocketAddr> = addrs.collect();
            !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback())
        }
        Err(_) => false,
    }
}

/// Pure binding policy: a non-loopback listener requires production-grade authentication
/// unless the conspicuous development switch is set.
fn check_binding(
    address: &str,
    production_auth: bool,
    insecure_switch: Option<&str>,
) -> Result<(), String> {
    if is_loopback_bind(address) || production_auth {
        return Ok(());
    }
    if insecure_switch == Some(INSECURE_BIND_SWITCH) {
        warn!(
            %address,
            "INSECURE NON-LOOPBACK BIND with non-production authentication permitted by \
             LEDGER_ALLOW_INSECURE_NON_LOOPBACK; development/CI networks only"
        );
        return Ok(());
    }
    Err(format!(
        "refusing to bind {address}: non-loopback binding requires production authentication \
         (LEDGER_AUTH_MODE=oidc); bind 127.0.0.1 instead, or for an isolated development \
         network set LEDGER_ALLOW_INSECURE_NON_LOOPBACK={INSECURE_BIND_SWITCH}"
    ))
}

/// The unvalidated-acceptance switch is a development setting: it is refused together
/// with production-grade authentication so a copied development environment cannot turn
/// a real deployment into one that publishes unvalidated changes.
fn check_acceptance(acceptance: AcceptancePolicy, production_auth: bool) -> Result<(), String> {
    if acceptance == AcceptancePolicy::AllowUnvalidatedDevelopmentOnly && production_auth {
        return Err(format!(
            "LEDGER_UNVALIDATED_ACCEPTANCE={UNVALIDATED_ACCEPTANCE_SWITCH} is a development \
             setting and cannot be combined with LEDGER_AUTH_MODE=oidc"
        ));
    }
    Ok(())
}

/// Filesystem development mode has no authentication at all: loopback only, no override.
fn check_filesystem_binding(address: &str) -> Result<(), String> {
    if is_loopback_bind(address) {
        Ok(())
    } else {
        Err(format!(
            "refusing to bind {address}: filesystem development mode has no authentication \
             and is loopback-only"
        ))
    }
}

fn acceptance_policy(value: Option<&str>) -> Result<AcceptancePolicy, String> {
    match value {
        None | Some("") => Ok(AcceptancePolicy::RequireValidation),
        Some(v) if v == UNVALIDATED_ACCEPTANCE_SWITCH => {
            Ok(AcceptancePolicy::AllowUnvalidatedDevelopmentOnly)
        }
        Some(_) => Err(format!(
            "LEDGER_UNVALIDATED_ACCEPTANCE has an unrecognised value; the only accepted value \
             is {UNVALIDATED_ACCEPTANCE_SWITCH} (value not shown)"
        )),
    }
}

fn claims_policy() -> Result<ClaimsPolicy, String> {
    let mut policy = ClaimsPolicy::default();
    if let Some(v) = env_optional("LEDGER_AUTH_TENANT_CLAIM")? {
        policy.tenant_claim = v;
    }
    if let Some(v) = env_optional("LEDGER_AUTH_PRINCIPAL_CLAIM")? {
        policy.principal_claim = v;
    }
    if let Some(v) = env_optional("LEDGER_AUTH_PRINCIPAL_TYPE_CLAIM")? {
        policy.principal_type_claim = Some(v).filter(|s| !s.is_empty());
    }
    if let Some(v) = env_optional("LEDGER_AUTH_ROLES_CLAIM")? {
        policy.roles_claim = v;
    }
    if let Some(v) = env_optional("LEDGER_AUTH_ON_BEHALF_OF_CLAIM")? {
        policy.on_behalf_of_claim = Some(v).filter(|s| !s.is_empty());
    }
    policy.agent_client_ids = env_set("LEDGER_AUTH_AGENT_CLIENT_IDS")?;
    policy.service_client_ids = env_set("LEDGER_AUTH_SERVICE_CLIENT_IDS")?;
    // Optional role renames: LEDGER_AUTH_ROLE_MAP="Ledger.Reader=read,Ledger.Writer=propose,..."
    if let Some(map) = env_optional("LEDGER_AUTH_ROLE_MAP")? {
        policy.role_map = parse_role_map(&map)?;
    }
    Ok(policy)
}

fn parse_role_map(map: &str) -> Result<std::collections::BTreeMap<String, Capability>, String> {
    let mut out = std::collections::BTreeMap::new();
    for entry in map.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (role, capability) = entry.split_once('=').ok_or_else(|| {
            format!("LEDGER_AUTH_ROLE_MAP entry {entry:?} must be role=capability")
        })?;
        let capability = match capability.trim() {
            "read" => Capability::Read,
            "propose" => Capability::Propose,
            "review" => Capability::Review,
            "admin" => Capability::Admin,
            other => {
                return Err(format!(
                    "LEDGER_AUTH_ROLE_MAP capability must be read|propose|review|admin, got {other:?}"
                ));
            }
        };
        out.insert(role.trim().to_owned(), capability);
    }
    if out.is_empty() {
        return Err("LEDGER_AUTH_ROLE_MAP is set but maps no roles".into());
    }
    Ok(out)
}

/// Build the authenticator from the environment. There is no unauthenticated mode and no
/// header-trusting mode: `oidc` (production) or `dev-hs256` (development/CI only).
fn authenticator() -> Result<SharedAuthenticator, String> {
    authenticator_from(
        env_optional("LEDGER_AUTH_MODE")?.as_deref(),
        env_optional("LEDGER_AUTH_ISSUER")?,
        env_optional("LEDGER_AUTH_AUDIENCE")?,
        env_optional("LEDGER_AUTH_JWKS_URL")?,
        env_optional("LEDGER_AUTH_DEV_HS256_SECRET")?,
        claims_policy()?,
    )
}

fn required(name: &str, value: Option<String>) -> Result<String, String> {
    value
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{name} is required for the selected LEDGER_AUTH_MODE"))
}

/// Pure authenticator selection so the refusal rules are unit-testable.
fn authenticator_from(
    mode: Option<&str>,
    issuer: Option<String>,
    audience: Option<String>,
    jwks_url: Option<String>,
    dev_secret: Option<String>,
    policy: ClaimsPolicy,
) -> Result<SharedAuthenticator, String> {
    match mode {
        Some("oidc") => {
            let issuer = required("LEDGER_AUTH_ISSUER", issuer)?;
            let audience = required("LEDGER_AUTH_AUDIENCE", audience)?;
            let jwks = required("LEDGER_AUTH_JWKS_URL", jwks_url)?;
            if !jwks.starts_with("https://") {
                return Err("LEDGER_AUTH_JWKS_URL must be an https:// URL".into());
            }
            Ok(Arc::new(OidcAuthenticator::new(
                issuer, audience, jwks, policy,
            )))
        }
        Some("dev-hs256") => {
            let issuer = required("LEDGER_AUTH_ISSUER", issuer)?;
            let audience = required("LEDGER_AUTH_AUDIENCE", audience)?;
            let secret = required("LEDGER_AUTH_DEV_HS256_SECRET", dev_secret)?;
            warn!(
                "DEVELOPMENT AUTHENTICATION (dev-hs256) selected: tokens are verified against a \
                 shared secret; this is never production authentication"
            );
            Ok(Arc::new(DevHs256Authenticator::new(
                issuer,
                audience,
                secret.as_bytes(),
                policy,
            )?))
        }
        None | Some("") => Err(
            "LEDGER_AUTH_MODE is required for the shared PostgreSQL server: 'oidc' (production) \
             or 'dev-hs256' (development/CI only)"
                .into(),
        ),
        Some(other) => Err(format!(
            "LEDGER_AUTH_MODE must be 'oidc' or 'dev-hs256', got {other:?}"
        )),
    }
}

fn limits() -> Result<ApiLimits, String> {
    let d = ApiLimits::default();
    Ok(ApiLimits {
        body_bytes: env_usize("LEDGER_LIMIT_BODY_BYTES", d.body_bytes)?,
        max_operations: env_usize("LEDGER_LIMIT_PATCH_OPERATIONS", d.max_operations)?,
        max_term_bytes: env_usize("LEDGER_LIMIT_TERM_BYTES", d.max_term_bytes)?,
        max_metadata_bytes: env_usize("LEDGER_LIMIT_METADATA_BYTES", d.max_metadata_bytes)?,
        reconstruction: ReconstructionLimits {
            max_depth: env_usize(
                "LEDGER_LIMIT_RECONSTRUCTION_DEPTH",
                d.reconstruction.max_depth,
            )?,
            max_quads: env_usize(
                "LEDGER_LIMIT_RECONSTRUCTION_QUADS",
                d.reconstruction.max_quads,
            )?,
            max_bytes: env_usize(
                "LEDGER_LIMIT_RECONSTRUCTION_BYTES",
                d.reconstruction.max_bytes,
            )?,
        },
        max_state_export_bytes: env_usize("LEDGER_LIMIT_EXPORT_BYTES", d.max_state_export_bytes)?,
        request_timeout: Duration::from_secs(env_usize(
            "LEDGER_LIMIT_REQUEST_SECONDS",
            d.request_timeout.as_secs() as usize,
        )? as u64),
        max_concurrent_expensive: env_usize(
            "LEDGER_LIMIT_CONCURRENT_EXPENSIVE",
            d.max_concurrent_expensive,
        )?,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let address = env_optional("LEDGER_ADDR")?.unwrap_or_else(|| "127.0.0.1:8080".into());
    let data = env_optional("LEDGER_DATA_DIR")?.unwrap_or_else(|| "./data".into());
    let database_url = env_optional("LEDGER_DATABASE_URL")?;
    let backend_choice = env_optional("LEDGER_IMMUTABLE_BACKEND")?;
    let backend = select_backend(database_url.as_deref(), backend_choice.as_deref())?;
    let limits = limits()?;

    let app = match backend {
        Backend::FilesystemOnly => {
            // Read-only inspection of a local development store: no authentication exists in
            // this mode, so it must never be reachable beyond loopback.
            check_filesystem_binding(&address)?;
            let ledger = Ledger::open(&data)?;
            match ledger.verify_head().await {
                Ok(head) => info!(head = ?head.map(|h| h.to_string()), "HEAD resolves"),
                Err(e) => {
                    return Err(format!("startup refused: HEAD does not resolve ({e})").into());
                }
            }
            info!("filesystem development mode: READ-ONLY inspection, no write surface");
            ledger_api::filesystem_readonly_router(Arc::new(ledger), limits)
        }
        Backend::SharedPostgres => {
            let url = database_url
                .as_deref()
                .expect("selected only with a database url");
            let authenticator = authenticator()?;
            check_binding(
                &address,
                authenticator.is_production_grade(),
                env_optional("LEDGER_ALLOW_INSECURE_NON_LOOPBACK")?.as_deref(),
            )?;
            let acceptance =
                acceptance_policy(env_optional("LEDGER_UNVALIDATED_ACCEPTANCE")?.as_deref())?;
            check_acceptance(acceptance, authenticator.is_production_grade())?;
            // Public writes are CommitV2 through the workflow only: no v1 envelope may be
            // published by this process (ADR-0010 policy `Reject`), and no route reaches the
            // raw ref primitive or `Ledger::commit`.
            let store = PostgresLedgerStore::connect(url, V1Binding::Reject).await?;
            info!(
                auth = %authenticator.describe(),
                "shared PostgreSQL topology: refs, objects and workflow in one database; v1 \
                 writes rejected"
            );
            ledger_api::router(AppState::new(store, authenticator, limits, acceptance))
        }
    };

    let listener = tokio::net::TcpListener::bind(&address).await?;
    info!(%address, "ledger server listening");

    // Graceful shutdown: SIGINT or (Unix) SIGTERM stops accepting connections, open ones
    // drain, and a bounded deadline guarantees the process exits.
    let (signalled_tx, signalled_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let why = shutdown_on(ctrl_c(), terminate()).await;
        info!(
            signal = why,
            "shutdown signal received; draining connections"
        );
        let _ = signalled_tx.send(true);
    });
    let mut drain_rx = signalled_rx.clone();
    let mut deadline_rx = signalled_rx;
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = drain_rx.wait_for(|signalled| *signalled).await;
    });
    let deadline = async move {
        let _ = deadline_rx.wait_for(|signalled| *signalled).await;
        tokio::time::sleep(DRAIN_TIMEOUT).await;
    };
    tokio::select! {
        result = server => result?,
        () = deadline => warn!(
            timeout_secs = DRAIN_TIMEOUT.as_secs(),
            "drain deadline reached with connections still open; exiting"
        ),
    }
    Ok(())
}

/// Resolve when either signal future resolves; the name of the winner is logged.
async fn shutdown_on(
    ctrl_c: impl Future<Output = ()>,
    terminate: impl Future<Output = ()>,
) -> &'static str {
    tokio::select! {
        () = ctrl_c => "SIGINT",
        () = terminate => "SIGTERM",
    }
}

/// Ctrl-C / SIGINT. A registration failure must not stop the server, so it waits forever
/// (and warns) instead of resolving immediately.
async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        warn!(error = %e, "SIGINT handler unavailable");
        std::future::pending::<()>().await;
    }
}

/// SIGTERM on Unix — the signal container runtimes send. Never resolves elsewhere.
async fn terminate() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(e) => {
                warn!(error = %e, "SIGTERM handler unavailable; only Ctrl-C stops the server");
                std::future::pending::<()>().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_url_defaults_to_the_shared_postgres_backend() {
        assert_eq!(
            select_backend(Some("postgres://x"), None),
            Ok(Backend::SharedPostgres)
        );
        assert_eq!(
            select_backend(Some("postgres://x"), Some("postgres")),
            Ok(Backend::SharedPostgres)
        );
    }

    #[test]
    fn filesystem_objects_with_shared_refs_is_refused_since_migration_0006() {
        let error = select_backend(Some("postgres://x"), Some("filesystem")).unwrap_err();
        assert!(error.contains("migration 0006"), "{error}");
    }

    #[test]
    fn no_database_means_filesystem_development_mode() {
        assert_eq!(select_backend(None, None), Ok(Backend::FilesystemOnly));
        assert_eq!(
            select_backend(None, Some("filesystem")),
            Ok(Backend::FilesystemOnly)
        );
    }

    #[test]
    fn explicit_postgres_without_a_database_url_is_a_configuration_error() {
        let error = select_backend(None, Some("postgres")).unwrap_err();
        assert!(error.contains("requires LEDGER_DATABASE_URL"), "{error}");
    }

    #[test]
    fn an_empty_database_url_is_a_configuration_error_in_every_mode() {
        for backend in [None, Some("postgres"), Some("filesystem")] {
            let error = select_backend(Some(""), backend).unwrap_err();
            assert!(error.contains("set but empty"), "{backend:?}: {error}");
        }
    }

    #[test]
    fn unknown_or_empty_backend_names_are_refused() {
        for url in [Some("postgres://x"), None] {
            for backend in [Some("bogus"), Some("")] {
                let error = select_backend(url, backend).unwrap_err();
                assert!(
                    error.contains("must be 'postgres'"),
                    "{url:?} {backend:?}: {error}"
                );
            }
        }
    }

    #[test]
    fn non_loopback_binding_requires_production_authentication() {
        assert!(check_binding("127.0.0.1:8080", false, None).is_ok());
        assert!(check_binding("[::1]:8080", false, None).is_ok());
        assert!(check_binding("0.0.0.0:8080", true, None).is_ok());
        let error = check_binding("0.0.0.0:8080", false, None).unwrap_err();
        assert!(
            error.contains("requires production authentication"),
            "{error}"
        );
        // A wrong switch value does not unlock anything.
        assert!(check_binding("0.0.0.0:8080", false, Some("yes")).is_err());
        assert!(check_binding("0.0.0.0:8080", false, Some(INSECURE_BIND_SWITCH)).is_ok());
        // Unresolvable binds fail closed.
        assert!(check_binding("not an address", false, None).is_err());
    }

    #[test]
    fn unvalidated_acceptance_needs_the_exact_conspicuous_value() {
        assert_eq!(
            acceptance_policy(None).unwrap(),
            AcceptancePolicy::RequireValidation
        );
        assert_eq!(
            acceptance_policy(Some("")).unwrap(),
            AcceptancePolicy::RequireValidation
        );
        assert!(acceptance_policy(Some("true")).is_err());
        assert!(acceptance_policy(Some("allow")).is_err());
        assert_eq!(
            acceptance_policy(Some(UNVALIDATED_ACCEPTANCE_SWITCH)).unwrap(),
            AcceptancePolicy::AllowUnvalidatedDevelopmentOnly
        );
    }

    #[test]
    fn unvalidated_acceptance_cannot_be_combined_with_production_authentication() {
        assert!(check_acceptance(AcceptancePolicy::RequireValidation, true).is_ok());
        assert!(check_acceptance(AcceptancePolicy::RequireValidation, false).is_ok());
        assert!(check_acceptance(AcceptancePolicy::AllowUnvalidatedDevelopmentOnly, false).is_ok());
        let error =
            check_acceptance(AcceptancePolicy::AllowUnvalidatedDevelopmentOnly, true).unwrap_err();
        assert!(error.contains("cannot be combined"), "{error}");
    }

    #[test]
    fn filesystem_mode_is_loopback_only_without_any_override() {
        assert!(check_filesystem_binding("127.0.0.1:8080").is_ok());
        assert!(check_filesystem_binding("0.0.0.0:8080").is_err());
        assert!(check_filesystem_binding("nonsense").is_err());
    }

    #[test]
    fn authenticator_selection_has_no_unauthenticated_or_header_mode() {
        let p = ClaimsPolicy::default;
        let s = |v: &str| Some(v.to_owned());
        for mode in [
            None,
            Some(""),
            Some("none"),
            Some("headers"),
            Some("trusted-headers"),
        ] {
            assert!(
                authenticator_from(mode, s("i"), s("a"), s("https://j"), s("x"), p()).is_err(),
                "{mode:?} must be refused"
            );
        }
        // oidc: https-only JWKS, all three settings required.
        assert!(
            authenticator_from(Some("oidc"), s("i"), s("a"), s("http://j"), None, p()).is_err()
        );
        assert!(authenticator_from(Some("oidc"), s("i"), None, s("https://j"), None, p()).is_err());
        let oidc =
            authenticator_from(Some("oidc"), s("i"), s("a"), s("https://j"), None, p()).unwrap();
        assert!(oidc.is_production_grade());
        // dev-hs256: secret required and at least 32 bytes; never production grade.
        assert!(
            authenticator_from(Some("dev-hs256"), s("i"), s("a"), None, s("short"), p()).is_err()
        );
        assert!(authenticator_from(Some("dev-hs256"), s("i"), s("a"), None, None, p()).is_err());
        let dev = authenticator_from(
            Some("dev-hs256"),
            s("i"),
            s("a"),
            None,
            s("a-development-secret-of-at-least-32-bytes"),
            p(),
        )
        .unwrap();
        assert!(!dev.is_production_grade());
    }

    #[test]
    fn role_map_parsing_is_strict() {
        let map =
            parse_role_map("Ledger.Reader=read, Ledger.Writer = propose,Ledger.Reviewer=review")
                .unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map["Ledger.Writer"], Capability::Propose);
        assert!(parse_role_map("Ledger.Reader=owner").is_err());
        assert!(parse_role_map("Ledger.Reader").is_err());
        assert!(parse_role_map(" , ").is_err());
    }

    #[tokio::test]
    async fn shutdown_resolves_on_either_signal_and_names_it() {
        let (int_tx, int_rx) = tokio::sync::oneshot::channel::<()>();
        let (term_tx, term_rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = shutdown_on(
            async {
                let _ = int_rx.await;
            },
            async {
                let _ = term_rx.await;
            },
        );
        let mut shutdown = Box::pin(shutdown);
        // Neither signal yet: the future stays pending.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut shutdown)
                .await
                .is_err()
        );
        term_tx.send(()).unwrap();
        assert_eq!(shutdown.await, "SIGTERM");
        drop(int_tx);

        let (int_tx, int_rx) = tokio::sync::oneshot::channel::<()>();
        let (_term_tx, term_rx) = tokio::sync::oneshot::channel::<()>();
        int_tx.send(()).unwrap();
        assert_eq!(
            shutdown_on(
                async {
                    let _ = int_rx.await;
                },
                async {
                    let _ = term_rx.await;
                }
            )
            .await,
            "SIGINT"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_stream_can_be_registered() {
        assert!(tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).is_ok());
    }
}
