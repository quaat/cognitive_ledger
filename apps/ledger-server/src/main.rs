use ledger_core::{GraphId, ImmutableStore};
use ledger_store::{FileStore, Ledger, PgRefStore, PostgresImmutableStore, V1Binding};
use std::{env, sync::Arc};
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let address = env::var("LEDGER_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let data = env::var("LEDGER_DATA_DIR").unwrap_or_else(|_| "./data".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;

    // The mutable ref head is delegated to PostgreSQL when LEDGER_DATABASE_URL is set
    // (the horizontally safe CAS point, ADR-0004/0007); otherwise the filesystem ref is
    // used. Immutable objects live on the filesystem unless LEDGER_IMMUTABLE_BACKEND is
    // `postgres`, in which case they share the database (ADR-0012). The bootstrap write
    // path still emits v1 envelopes, so the shared store binds them to the bootstrap
    // `default` graph (ADR-0010 policy); production flips this to `Reject` with the v2
    // write path. A shared ref MUST NOT reference node-local content, so only the
    // postgres immutable backend is a supported multi-replica topology.
    let ledger = match env::var("LEDGER_DATABASE_URL") {
        Ok(url) if !url.is_empty() => {
            let refs = Arc::new(PgRefStore::connect(&url).await?);
            let backend =
                env::var("LEDGER_IMMUTABLE_BACKEND").unwrap_or_else(|_| "filesystem".into());
            match backend.as_str() {
                "postgres" => {
                    let bootstrap = GraphId::new("default")?;
                    let store: Arc<dyn ImmutableStore> = Arc::new(
                        PostgresImmutableStore::connect(&url, V1Binding::BindTo(bootstrap)).await?,
                    );
                    info!("ref coordination: postgresql; immutable objects: postgresql");
                    Ledger::with_stores(store, refs)
                }
                "filesystem" => {
                    let store = Arc::new(FileStore::open(&data)?);
                    info!(
                        "ref coordination: postgresql; immutable objects: filesystem (single host only)"
                    );
                    Ledger::with_ref_store(store, refs)
                }
                other => {
                    return Err(format!(
                        "LEDGER_IMMUTABLE_BACKEND must be 'filesystem' or 'postgres', got {other:?}"
                    )
                    .into());
                }
            }
        }
        _ => {
            info!("ref coordination: filesystem");
            Ledger::open(&data)?
        }
    };
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
