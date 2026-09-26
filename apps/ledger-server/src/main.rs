use ledger_core::{GraphId, ImmutableStore};
use ledger_store::{FileStore, Ledger, PgRefStore, PostgresImmutableStore, V1Binding};
use std::{env, sync::Arc};
use tracing::{info, warn};

/// Which persistence topology the environment selects (ADR-0012).
#[derive(Clone, Debug, Eq, PartialEq)]
enum Backend {
    /// No database: filesystem refs and objects (development only).
    FilesystemOnly,
    /// PostgreSQL refs and PostgreSQL immutable objects — the shared, multi-replica-safe
    /// default whenever a database URL is present.
    SharedPostgres,
    /// PostgreSQL refs with node-local filesystem objects. Explicit single-host opt-in;
    /// a second replica would read refs whose content it does not have.
    SingleHostFilesystem,
}

/// Pure selection so the defaulting rules are unit-testable: a database URL means the
/// shared PostgreSQL backend unless `LEDGER_IMMUTABLE_BACKEND=filesystem` is set
/// explicitly; without a database URL the ledger is filesystem-only.
fn select_backend(database_url: Option<&str>, backend: Option<&str>) -> Result<Backend, String> {
    let database_url = database_url.filter(|u| !u.is_empty());
    match (database_url, backend) {
        (None, _) => Ok(Backend::FilesystemOnly),
        (Some(_), None | Some("postgres")) => Ok(Backend::SharedPostgres),
        (Some(_), Some("filesystem")) => Ok(Backend::SingleHostFilesystem),
        (Some(_), Some(other)) => Err(format!(
            "LEDGER_IMMUTABLE_BACKEND must be 'postgres' (default) or 'filesystem', got {other:?}"
        )),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let address = env::var("LEDGER_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let data = env::var("LEDGER_DATA_DIR").unwrap_or_else(|_| "./data".into());
    let database_url = env::var("LEDGER_DATABASE_URL").ok();
    let backend_choice = env::var("LEDGER_IMMUTABLE_BACKEND").ok();
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
            let refs = Arc::new(PgRefStore::connect(url).await?);
            let bootstrap = GraphId::new("default")?;
            let store: Arc<dyn ImmutableStore> =
                Arc::new(PostgresImmutableStore::connect(url, V1Binding::BindTo(bootstrap)).await?);
            info!("ref coordination: postgresql; immutable objects: postgresql (shared)");
            warn!(
                "bootstrap topology: v1 commits are bound to the non-production graph 'default' \
                 (ADR-0010); this is not a multi-tenant deployment"
            );
            Ledger::with_stores(store, refs)
        }
        Backend::SingleHostFilesystem => {
            let url = database_url
                .as_deref()
                .expect("selected only with a database url");
            let refs = Arc::new(PgRefStore::connect(url).await?);
            let store = Arc::new(FileStore::open(&data)?);
            warn!(
                data_dir = %data,
                "SINGLE-HOST MODE: shared PostgreSQL refs point at node-local filesystem \
                 objects; a second replica sharing this database would read refs whose \
                 content it does not have. Run exactly one ledger process against it."
            );
            Ledger::with_ref_store(store, refs)
        }
    };
    // Refuse to serve a HEAD whose content is not in the configured store (e.g. an
    // unmigrated filesystem history behind a shared ref).
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
    axum::serve(listener, ledger_api::router(ledger))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}
async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
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
    fn filesystem_objects_with_shared_refs_is_an_explicit_opt_in() {
        assert_eq!(
            select_backend(Some("postgres://x"), Some("filesystem")),
            Ok(Backend::SingleHostFilesystem)
        );
    }

    #[test]
    fn no_database_means_filesystem_only_regardless_of_backend_flag() {
        assert_eq!(select_backend(None, None), Ok(Backend::FilesystemOnly));
        assert_eq!(select_backend(Some(""), None), Ok(Backend::FilesystemOnly));
        assert_eq!(
            select_backend(None, Some("postgres")),
            Ok(Backend::FilesystemOnly)
        );
    }

    #[test]
    fn unknown_or_empty_backend_names_are_refused() {
        assert!(select_backend(Some("postgres://x"), Some("bogus")).is_err());
        assert!(select_backend(Some("postgres://x"), Some("")).is_err());
    }
}
