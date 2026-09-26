use ledger_store::{FileStore, Ledger, PgRefStore};
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

    // Immutable objects and commits always live on the filesystem. The mutable ref
    // head is delegated to PostgreSQL when LEDGER_DATABASE_URL is set (the horizontally
    // safe CAS point, ADR-0004/0007); otherwise the filesystem ref is used.
    let ledger = match env::var("LEDGER_DATABASE_URL") {
        Ok(url) if !url.is_empty() => {
            let store = Arc::new(FileStore::open(&data)?);
            let refs = Arc::new(PgRefStore::connect(&url).await?);
            info!("ref coordination: postgresql");
            Ledger::with_ref_store(store, refs)
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
