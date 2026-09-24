use ledger_store::Ledger;
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
    let ledger = Arc::new(Ledger::open(data)?);
    info!(%address,"ledger server listening");
    axum::serve(listener, ledger_api::router(ledger))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}
async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
