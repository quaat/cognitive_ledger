//! `ledger-projector`: the dedicated accepted-state projector process (ADR-0020/0021).
//!
//! ```text
//! ledger-projector [run]                         serve /health /ready /metrics and project
//! ledger-projector rebuild --graph <id> [--ref main]
//! ledger-projector verify  --graph <id> [--ref main]
//! ledger-projector status  [--json]
//! ```
//! Configuration (environment; secrets only through files, never logged):
//! `LEDGER_PROJECTOR_DATABASE_URL` (the projector identity, ADR-0021), `LEDGER_PROJECTION_TARGET_ID`,
//! `LEDGER_PROJECTION_QUERY_URL`, `LEDGER_PROJECTION_UPDATE_URL`, either
//! `LEDGER_PROJECTION_USERNAME` + `LEDGER_PROJECTION_PASSWORD_FILE` or
//! `LEDGER_PROJECTION_TOKEN_FILE`, `LEDGER_PROJECTOR_ADDR` (default 127.0.0.1:9464),
//! `LEDGER_PROJECTOR_CONCURRENCY`, `LEDGER_PROJECTOR_LEASE_SECONDS`, `LEDGER_PROJECTOR_POLL_MS`,
//! `LEDGER_PROJECTOR_TARGET_TIMEOUT_SECONDS`, `LEDGER_PROJECTOR_MAX_UPDATE_BYTES`,
//! `LEDGER_PROJECTOR_MAX_RESPONSE_BYTES`, `LEDGER_PROJECTOR_MAX_QUADS`,
//! `LEDGER_PROJECTOR_MAX_STATE_BYTES`, `LEDGER_PROJECTOR_MAX_DEPTH`.
//! `LEDGER_PROJECTOR_DEVELOPMENT=allow-insecure-development-only` permits plain http to a
//! loopback target and a target without credentials; it is refused together with https-less
//! non-loopback endpoints in every case.

use ledger_core::GraphId;
use ledger_projection::ProjectionClient;
use ledger_projection_fuseki::{FusekiClient, FusekiConfig, TargetCredentials};
use ledger_projector::{Projector, ProjectorConfig, metrics::Metrics};
use ledger_store::{DbSessionLimits, ProjectionRepository, ReconstructionLimits, StreamKey};
use std::{env, process::ExitCode, sync::Arc, time::Duration};

const DEVELOPMENT_SWITCH: &str = "allow-insecure-development-only";
const MAX_SECRET_FILE_BYTES: u64 = 64 * 1024;

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

fn required(name: &str) -> Result<String, String> {
    var(name).ok_or_else(|| format!("{name} is required"))
}

fn number<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match var(name) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| format!("{name} must be a number (value not shown)")),
    }
}

fn secret_file(name: &str) -> Result<Option<String>, String> {
    let Some(path) = var(name) else {
        return Ok(None);
    };
    let unreadable = || format!("{name} cannot be read (path not shown)");
    let size = std::fs::metadata(&path).map_err(|_| unreadable())?.len();
    if size > MAX_SECRET_FILE_BYTES {
        return Err(format!("{name} exceeds {MAX_SECRET_FILE_BYTES} bytes"));
    }
    Ok(Some(
        std::fs::read_to_string(&path)
            .map_err(|_| unreadable())?
            .trim()
            .to_owned(),
    ))
}

struct Settings {
    database_url: String,
    target: FusekiConfig,
    projector: ProjectorConfig,
    address: String,
}

fn settings() -> Result<Settings, String> {
    let development = match var("LEDGER_PROJECTOR_DEVELOPMENT") {
        None => false,
        Some(v) if v == DEVELOPMENT_SWITCH => true,
        Some(_) => {
            return Err(format!(
                "LEDGER_PROJECTOR_DEVELOPMENT has an unrecognised value; the only accepted value is \
                 {DEVELOPMENT_SWITCH} (value not shown)"
            ));
        }
    };
    let credentials = match (
        var("LEDGER_PROJECTION_USERNAME"),
        secret_file("LEDGER_PROJECTION_PASSWORD_FILE")?,
        secret_file("LEDGER_PROJECTION_TOKEN_FILE")?,
    ) {
        (Some(username), Some(password), None) => TargetCredentials::Basic { username, password },
        (None, None, Some(token)) => TargetCredentials::Bearer(token),
        (None, None, None) if development => TargetCredentials::None,
        (None, None, None) => {
            return Err(
                "projection target credentials are required (LEDGER_PROJECTION_USERNAME + \
                 LEDGER_PROJECTION_PASSWORD_FILE, or LEDGER_PROJECTION_TOKEN_FILE)"
                    .into(),
            );
        }
        _ => {
            return Err(
                "configure either LEDGER_PROJECTION_USERNAME + LEDGER_PROJECTION_PASSWORD_FILE or \
                 LEDGER_PROJECTION_TOKEN_FILE, not a mix"
                    .into(),
            );
        }
    };
    let timeout = Duration::from_secs(number("LEDGER_PROJECTOR_TARGET_TIMEOUT_SECONDS", 30u64)?);
    let lease = Duration::from_secs(number("LEDGER_PROJECTOR_LEASE_SECONDS", 120u64)?);
    if lease <= timeout * 2 {
        return Err(
            "LEDGER_PROJECTOR_LEASE_SECONDS must exceed twice LEDGER_PROJECTOR_TARGET_TIMEOUT_SECONDS"
                .into(),
        );
    }
    let target_id = required("LEDGER_PROJECTION_TARGET_ID")?;
    let instance = format!(
        "{}:{}:{}",
        var("HOSTNAME").unwrap_or_else(|| "projector".into()),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let d = ReconstructionLimits::DEVELOPMENT;
    Ok(Settings {
        database_url: required("LEDGER_PROJECTOR_DATABASE_URL")?,
        target: FusekiConfig {
            query_endpoint: required("LEDGER_PROJECTION_QUERY_URL")?,
            update_endpoint: required("LEDGER_PROJECTION_UPDATE_URL")?,
            credentials,
            connect_timeout: timeout.min(Duration::from_secs(10)),
            request_timeout: timeout,
            max_update_bytes: number("LEDGER_PROJECTOR_MAX_UPDATE_BYTES", 64 * 1024 * 1024)?,
            max_response_bytes: number("LEDGER_PROJECTOR_MAX_RESPONSE_BYTES", 64 * 1024 * 1024)?,
            allow_insecure_loopback: development,
        },
        projector: ProjectorConfig {
            target_id,
            owner: instance.chars().take(256).collect(),
            lease_ttl: lease,
            reconstruction: ReconstructionLimits {
                max_depth: number("LEDGER_PROJECTOR_MAX_DEPTH", d.max_depth)?,
                max_quads: number("LEDGER_PROJECTOR_MAX_QUADS", d.max_quads)?,
                max_bytes: number("LEDGER_PROJECTOR_MAX_STATE_BYTES", d.max_bytes)?,
            },
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(300),
            poll_interval: Duration::from_millis(number("LEDGER_PROJECTOR_POLL_MS", 1000u64)?),
            concurrency: number("LEDGER_PROJECTOR_CONCURRENCY", 2usize)?,
        },
        address: var("LEDGER_PROJECTOR_ADDR").unwrap_or_else(|| "127.0.0.1:9464".into()),
    })
}

enum Command {
    Run,
    Rebuild(StreamKey),
    Verify(StreamKey),
    Status { json: bool },
}

fn parse(target_id: &str, mut argv: impl Iterator<Item = String>) -> Result<Command, String> {
    let sub = argv.next();
    let (mut graph, mut branch, mut json) = (None, "main".to_owned(), false);
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--graph" => graph = argv.next(),
            "--ref" => branch = argv.next().ok_or("--ref needs a value")?,
            "--json" => json = true,
            _ => return Err("unknown argument (value not shown)".into()),
        }
    }
    let key = |graph: Option<String>| -> Result<StreamKey, String> {
        Ok(StreamKey {
            graph_id: GraphId::new(graph.ok_or("--graph is required")?)
                .map_err(|e| e.to_string())?,
            branch: branch.clone(),
            target_id: target_id.to_owned(),
        })
    };
    match sub.as_deref() {
        None | Some("run") => Ok(Command::Run),
        Some("rebuild") => key(graph).map(Command::Rebuild),
        Some("verify") => key(graph).map(Command::Verify),
        Some("status") => Ok(Command::Status { json }),
        _ => Err("usage: ledger-projector [run|rebuild|verify|status] …".into()),
    }
}

async fn serve(
    projector: Arc<Projector<FusekiClient>>,
    metrics: Arc<Metrics>,
    address: String,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    use axum::{Router, http::StatusCode, routing::get};
    let ready_projector = projector.clone();
    let metrics_projector = projector.clone();
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/ready",
            get(move || {
                let p = ready_projector.clone();
                async move {
                    match p.repository().ready().await {
                        Ok(()) => (StatusCode::OK, "ready"),
                        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "not ready"),
                    }
                }
            }),
        )
        .route(
            "/metrics",
            get(move || {
                let p = metrics_projector.clone();
                let m = metrics.clone();
                async move {
                    let repo = p.repository();
                    match (
                        repo.status(Some(&p.config().target_id)).await,
                        repo.unconfigured_pending().await,
                    ) {
                        (Ok(streams), Ok(unconfigured)) => {
                            (StatusCode::OK, m.render(&streams, unconfigured))
                        }
                        _ => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            "database unavailable\n".into(),
                        ),
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|_| "cannot bind LEDGER_PROJECTOR_ADDR (value not shown)".to_owned())?;
    let mut stop = shutdown;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = stop.changed().await;
        })
        .await
        .map_err(|e| e.to_string())
}

async fn real_main() -> Result<(), String> {
    let settings = settings()?;
    let command = parse(&settings.projector.target_id, env::args().skip(1))?;
    let repo = ProjectionRepository::connect(
        &settings.database_url,
        DbSessionLimits {
            max_connections: 8,
            ..DbSessionLimits::default()
        },
    )
    .await
    .map_err(|e| format!("startup refused: {e}"))?;
    let client = Arc::new(FusekiClient::new(settings.target).map_err(|e| e.to_string())?);
    let metrics = Arc::new(Metrics::default());
    let projector = Arc::new(Projector::new(
        repo,
        client.clone(),
        settings.projector,
        metrics.clone(),
    ));
    match command {
        Command::Status { json } => {
            let streams = projector
                .repository()
                .status(Some(&projector.config().target_id))
                .await
                .map_err(|e| e.to_string())?;
            for s in &streams {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "graph_id": s.key.graph_id.as_str(), "ref": s.key.branch,
                            "status": s.status, "projected_ref_version": s.projected_ref_version,
                            "head_version": s.head_version, "lag_versions": s.lag_versions(),
                            "pending_events": s.pending_events,
                            "oldest_pending_seconds": s.oldest_pending_seconds,
                            "last_error_code": s.last_error_code,
                        })
                    );
                } else {
                    println!(
                        "{} {} [{}] projected v{} of v{} (lag {}){}",
                        s.key.graph_id,
                        s.key.branch,
                        s.status,
                        s.projected_ref_version.unwrap_or(0),
                        s.head_version.unwrap_or(0),
                        s.lag_versions(),
                        s.last_error_code
                            .as_ref()
                            .map(|c| format!(" last error {c}"))
                            .unwrap_or_default()
                    );
                }
            }
            return Ok(());
        }
        Command::Verify(key) => {
            let report = projector.verify(&key).await.map_err(|e| e.to_string())?;
            println!("{}", report.detail);
            return if report.consistent {
                println!("PROJECTION CONSISTENT");
                Ok(())
            } else {
                Err("PROJECTION INCONSISTENT".into())
            };
        }
        Command::Rebuild(_) | Command::Run => {}
    }
    // Both remaining commands write the target: prove it is transactional first.
    client
        .probe_transactional()
        .await
        .map_err(|e| format!("startup refused: {e}"))?;
    if let Command::Rebuild(key) = command {
        return match projector.rebuild(&key).await.map_err(|e| e.to_string())? {
            Some(ledger_projector::StepOutcome::Projected { version, .. }) => {
                println!("REBUILD OK: {} {} at v{version}", key.graph_id, key.branch);
                Ok(())
            }
            Some(other) => Err(format!("rebuild did not complete: {other:?}")),
            None => Err("the stream is leased by a running projector or disabled".into()),
        };
    }
    tracing::info!(
        target = %projector.config().target_id,
        client = %client.describe(),
        concurrency = projector.config().concurrency,
        "projector started"
    );
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(serve(
        projector.clone(),
        metrics,
        settings.address,
        stop_rx.clone(),
    ));
    let workers = tokio::spawn(projector.clone().run(stop_rx));
    shutdown_signal().await;
    tracing::info!("shutting down: finishing current steps");
    let _ = stop_tx.send(true);
    let _ = workers.await;
    server.await.map_err(|e| e.to_string())?
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match real_main().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ledger-projector: {e}");
            ExitCode::FAILURE
        }
    }
}
