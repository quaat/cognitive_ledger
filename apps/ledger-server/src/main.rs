use ledger_core::{GraphId, ImmutableStore, RefStore};
use ledger_store::{Ledger, PgRefStore, PostgresLedgerStore, V1Binding};
use std::{env, future::Future, sync::Arc, time::Duration};
use tracing::{info, warn};

/// How long open connections may drain after a shutdown signal before the process exits
/// anyway. An unfinished publication transaction is rolled back by PostgreSQL, so exiting
/// never leaves partial state.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Which persistence topology the environment selects (ADR-0012).
#[derive(Clone, Debug, Eq, PartialEq)]
enum Backend {
    /// No database: filesystem refs and objects (development only).
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    // Loopback by default until the authenticated API exists (P1.4); a container or
    // operator opts into 0.0.0.0 explicitly (the Dockerfile does).
    let address = env_optional("LEDGER_ADDR")?.unwrap_or_else(|| "127.0.0.1:8080".into());
    let data = env_optional("LEDGER_DATA_DIR")?.unwrap_or_else(|| "./data".into());
    let database_url = env_optional("LEDGER_DATABASE_URL")?;
    let backend_choice = env_optional("LEDGER_IMMUTABLE_BACKEND")?;
    let backend = select_backend(database_url.as_deref(), backend_choice.as_deref())?;
    let listener = tokio::net::TcpListener::bind(&address).await?;

    // The bootstrap write path still emits v1 envelopes, so the shared store binds them to
    // the bootstrap `default` graph (ADR-0010 policy); production flips this to `Reject`
    // with the v2 write path.
    let ledger = match backend {
        Backend::FilesystemOnly => {
            info!("ref coordination: filesystem; immutable objects: filesystem (development)");
            Ledger::open(&data)?
        }
        Backend::SharedPostgres => {
            let url = database_url
                .as_deref()
                .expect("selected only with a database url");
            let bootstrap = GraphId::new("default")?;
            // One pool for immutable content, graph authority and the workflow repository
            // (P1.3). The HTTP surface still uses the bootstrap v1 write path through the
            // raw ref primitive until the authenticated API (P1.4) routes prepare/accept.
            let store = PostgresLedgerStore::connect(url, V1Binding::BindTo(bootstrap)).await?;
            let refs: Arc<dyn RefStore> = Arc::new(PgRefStore::with_ref_migrated(
                store.pool().clone(),
                "default",
                "main",
            ));
            let immutable: Arc<dyn ImmutableStore> = Arc::new(store.immutable().clone());
            info!("ref coordination: postgresql; immutable objects: postgresql (shared)");
            warn!(
                "bootstrap topology: v1 commits are bound to the non-production graph 'default' \
                 (ADR-0010) through the raw ref primitive (no ref events); production accepted \
                 transitions use WorkflowRepository once the authenticated API lands (P1.4)"
            );
            Ledger::with_stores(immutable, refs)
        }
    };
    // Refuse to serve a HEAD whose content (commit and patch) is not valid in the
    // configured store (e.g. an unmigrated filesystem history behind a shared ref).
    match ledger.verify_head().await {
        Ok(head) => {
            info!(head = ?head.map(|h| h.to_string()), "HEAD resolves in the configured store")
        }
        Err(e) => {
            return Err(format!(
                "startup refused: current HEAD does not resolve in the configured immutable \
                 store ({e}); migrate content first (ledger-admin migrate-fs-to-pg) or fix the \
                 backend selection"
            )
            .into());
        }
    }
    let ledger = Arc::new(ledger);
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
    let server =
        axum::serve(listener, ledger_api::router(ledger)).with_graceful_shutdown(async move {
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
