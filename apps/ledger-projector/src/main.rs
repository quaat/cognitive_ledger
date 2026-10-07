//! `ledger-projector`: the dedicated accepted-state projector process (ADR-0020/0021).
//!
//! ```text
//! ledger-projector [run]                         serve /health /ready /metrics and project
//! ledger-projector rebuild --graph <id> [--ref main]
//! ledger-projector verify  --graph <id> [--ref main]
//! ```
//! (Status is `ledger-admin projection status`, a database-only operator read.)
//! Configuration (environment; secrets only through files, never logged):
//! `LEDGER_PROJECTOR_DATABASE_URL` (the projector identity, ADR-0021), `LEDGER_PROJECTION_TARGET_ID`,
//! `LEDGER_PROJECTION_QUERY_URL`, `LEDGER_PROJECTION_UPDATE_URL`, either
//! `LEDGER_PROJECTION_USERNAME` + `LEDGER_PROJECTION_PASSWORD_FILE` or
//! `LEDGER_PROJECTION_TOKEN_FILE`, `LEDGER_PROJECTOR_ADDR` (default 127.0.0.1:9464),
//! `LEDGER_PROJECTOR_CONCURRENCY`, `LEDGER_PROJECTOR_LEASE_SECONDS`, `LEDGER_PROJECTOR_POLL_MS`,
//! `LEDGER_PROJECTOR_TARGET_TIMEOUT_SECONDS`, `LEDGER_PROJECTOR_MAX_UPDATE_BYTES`,
//! `LEDGER_PROJECTOR_MAX_RESPONSE_BYTES`, `LEDGER_PROJECTOR_MAX_QUADS`,
//! `LEDGER_PROJECTOR_MAX_STATE_BYTES`, `LEDGER_PROJECTOR_MAX_DEPTH`,
//! `LEDGER_PROJECTOR_RECONCILE_SECONDS` (idle-stream re-check, default 300),
//! `LEDGER_PROJECTOR_PROBE_SECONDS` (transactional probe, default 300).
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

/// Environment lookup (injectable so configuration refusals are unit-testable).
type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

fn process_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

fn required(env: Env, name: &str) -> Result<String, String> {
    env(name).ok_or_else(|| format!("{name} is required"))
}

fn number<T: std::str::FromStr>(env: Env, name: &str, default: T) -> Result<T, String> {
    match env(name) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| format!("{name} must be a number (value not shown)")),
    }
}

/// A number setting with a lower bound (refused below it).
fn at_least(env: Env, name: &str, default: u64, minimum: u64) -> Result<u64, String> {
    let value = number(env, name, default)?;
    if value < minimum {
        return Err(format!("{name} must be at least {minimum}"));
    }
    Ok(value)
}

/// A lease owner: at most `max` bytes (never splitting a character), control characters
/// replaced, so a hostname can never make every claim fail validation.
fn truncate_bytes(value: &str, max: usize) -> String {
    let mut out = String::new();
    for c in value.chars().map(|c| if c.is_control() { '_' } else { c }) {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

/// Read a secret from a regular file, bounded (a FIFO or device cannot bypass the cap).
fn secret_file(env: Env, name: &str) -> Result<Option<String>, String> {
    use std::io::Read as _;
    let Some(path) = env(name) else {
        return Ok(None);
    };
    let unreadable = || format!("{name} cannot be read (path not shown)");
    let meta = std::fs::metadata(&path).map_err(|_| unreadable())?;
    if !meta.is_file() {
        return Err(format!("{name} must name a regular file"));
    }
    let file = std::fs::File::open(&path).map_err(|_| unreadable())?;
    // Re-check what was actually opened (the path may have been swapped since).
    if !file.metadata().map_err(|_| unreadable())?.is_file() {
        return Err(format!("{name} must name a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SECRET_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unreadable())?;
    if bytes.len() as u64 > MAX_SECRET_FILE_BYTES {
        return Err(format!("{name} exceeds {MAX_SECRET_FILE_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{name} is not UTF-8"))?;
    // Only the one line ending a file editor adds is not part of the secret; any other
    // whitespace is (a password may legitimately start or end with a space).
    let secret = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    if secret.is_empty() {
        return Err(format!("{name} is empty"));
    }
    Ok(Some(secret.to_owned()))
}

struct Settings {
    database_url: String,
    target: FusekiConfig,
    projector: ProjectorConfig,
    address: String,
}

fn settings(env: Env) -> Result<Settings, String> {
    let development = match env("LEDGER_PROJECTOR_DEVELOPMENT") {
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
        env("LEDGER_PROJECTION_USERNAME"),
        secret_file(env, "LEDGER_PROJECTION_PASSWORD_FILE")?,
        secret_file(env, "LEDGER_PROJECTION_TOKEN_FILE")?,
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
    let timeout = Duration::from_secs(number(
        env,
        "LEDGER_PROJECTOR_TARGET_TIMEOUT_SECONDS",
        30u64,
    )?);
    let lease = Duration::from_secs(number(env, "LEDGER_PROJECTOR_LEASE_SECONDS", 180u64)?);
    // A step makes up to four target requests (observe, write, observe, containment) plus
    // reconstruction; the lease must cover them with margin (a write outliving its lease is
    // harmless, ADR-0020, but wasted).
    let minimum = timeout
        .checked_mul(4)
        .and_then(|t| t.checked_add(Duration::from_secs(30)))
        .ok_or("LEDGER_PROJECTOR_TARGET_TIMEOUT_SECONDS is out of range")?;
    if lease < minimum {
        return Err(
            "LEDGER_PROJECTOR_LEASE_SECONDS must be at least 4 × LEDGER_PROJECTOR_TARGET_TIMEOUT_SECONDS + 30"
                .into(),
        );
    }
    let target_id = required(env, "LEDGER_PROJECTION_TARGET_ID")?;
    let instance = format!(
        "{}:{}:{}",
        env("HOSTNAME").unwrap_or_else(|| "projector".into()),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let d = ReconstructionLimits::DEVELOPMENT;
    let max_update_bytes: usize =
        number(env, "LEDGER_PROJECTOR_MAX_UPDATE_BYTES", 64 * 1024 * 1024)?;
    let max_state_bytes: usize = number(env, "LEDGER_PROJECTOR_MAX_STATE_BYTES", 48 * 1024 * 1024)?;
    if max_state_bytes >= max_update_bytes {
        return Err(
            "LEDGER_PROJECTOR_MAX_STATE_BYTES must be below LEDGER_PROJECTOR_MAX_UPDATE_BYTES"
                .into(),
        );
    }
    Ok(Settings {
        database_url: required(env, "LEDGER_PROJECTOR_DATABASE_URL")?,
        target: FusekiConfig {
            query_endpoint: required(env, "LEDGER_PROJECTION_QUERY_URL")?,
            update_endpoint: required(env, "LEDGER_PROJECTION_UPDATE_URL")?,
            credentials,
            connect_timeout: timeout.min(Duration::from_secs(10)),
            request_timeout: timeout,
            max_update_bytes,
            max_response_bytes: number(
                env,
                "LEDGER_PROJECTOR_MAX_RESPONSE_BYTES",
                64 * 1024 * 1024,
            )?,
            allow_insecure_loopback: development,
        },
        projector: ProjectorConfig {
            target_id,
            owner: truncate_bytes(&instance, 256),
            lease_ttl: lease,
            reconstruction: ReconstructionLimits {
                max_depth: number(env, "LEDGER_PROJECTOR_MAX_DEPTH", d.max_depth)?,
                max_quads: number(env, "LEDGER_PROJECTOR_MAX_QUADS", d.max_quads)?,
                max_bytes: max_state_bytes,
            },
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(300),
            poll_interval: Duration::from_millis(at_least(
                env,
                "LEDGER_PROJECTOR_POLL_MS",
                1000,
                10,
            )?),
            concurrency: at_least(env, "LEDGER_PROJECTOR_CONCURRENCY", 2, 1)? as usize,
            // Never zero: an idle stream re-checked continuously, or a probe loop without
            // delay, would hammer the database and the target.
            reconcile_interval: Duration::from_secs(at_least(
                env,
                "LEDGER_PROJECTOR_RECONCILE_SECONDS",
                300,
                1,
            )?),
            probe_interval: Duration::from_secs(at_least(
                env,
                "LEDGER_PROJECTOR_PROBE_SECONDS",
                300,
                1,
            )?),
        },
        address: env("LEDGER_PROJECTOR_ADDR").unwrap_or_else(|| "127.0.0.1:9464".into()),
    })
}

enum Command {
    Run,
    Rebuild(StreamKey),
    Verify(StreamKey),
}

fn parse(target_id: &str, mut argv: impl Iterator<Item = String>) -> Result<Command, String> {
    let sub = argv.next();
    let (mut graph, mut branch) = (None, "main".to_owned());
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--graph" => graph = argv.next(),
            "--ref" => branch = argv.next().ok_or("--ref needs a value")?,
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
        _ => Err("usage: ledger-projector [run|rebuild|verify] …".into()),
    }
}

async fn serve(
    projector: Arc<Projector<FusekiClient>>,
    metrics: Arc<Metrics>,
    listener: tokio::net::TcpListener,
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
                    if p.is_paused() {
                        return (StatusCode::SERVICE_UNAVAILABLE, "target failed the probe");
                    }
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

    let mut stop = shutdown;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = stop.changed().await;
        })
        .await
        .map_err(|e| e.to_string())
}

async fn real_main() -> Result<(), String> {
    let settings = settings(&process_env)?;
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
    if settings.target.allow_insecure_loopback {
        tracing::warn!(
            "LEDGER_PROJECTOR_DEVELOPMENT is set: plain http to a loopback target is allowed and \
             target credentials are optional — development only, never in production"
        );
    }
    let client = Arc::new(FusekiClient::new(settings.target).map_err(|e| e.to_string())?);
    let metrics = Arc::new(Metrics::default());
    let projector = Arc::new(Projector::new(
        repo,
        client.clone(),
        settings.projector,
        metrics.clone(),
    ));
    match command {
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
    // Both remaining commands write the target: prove it is transactional and bound to this
    // target id (one dataset never answers to two target ids, ADR-0020).
    client
        .probe_transactional()
        .await
        .map_err(|e| format!("startup refused: {e}"))?;
    client
        .bind_target(&projector.config().target_id)
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
    // Bind before any worker starts: a projector without health, readiness and metrics
    // endpoints must not run.
    let listener = tokio::net::TcpListener::bind(&settings.address)
        .await
        .map_err(|_| "cannot bind LEDGER_PROJECTOR_ADDR (value not shown)".to_owned())?;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(serve(projector.clone(), metrics, listener, stop_rx.clone()));
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
    if ledger_store::TEST_HOOKS_COMPILED || ledger_projector::TEST_HOOKS_COMPILED {
        eprintln!(
            "ledger-projector: this build contains test-only fault injection (`test-hooks`); \
             refusing to run"
        );
        return ExitCode::FAILURE;
    }
    match real_main().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ledger-projector: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn secret_files_keep_everything_but_one_trailing_line_ending() {
        let dir = std::env::temp_dir().join(format!("lp-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (content, expected) in [
            (" pass word \n", Ok(" pass word ".to_owned())),
            ("secret\r\n", Ok("secret".to_owned())),
            ("secret", Ok("secret".to_owned())),
            ("two\n\n", Ok("two\n".to_owned())),
            ("\n", Err(())),
        ] {
            let path = dir.join("pw");
            std::fs::write(&path, content).unwrap();
            let path = path.to_string_lossy().into_owned();
            let env = |name: &str| (name == "SECRET").then(|| path.clone());
            let got = secret_file(&env, "SECRET")
                .map(|v| v.unwrap())
                .map_err(|_| ());
            assert_eq!(got, expected, "{content:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lease_owners_are_bounded_by_bytes_and_free_of_control_characters() {
        let owner = truncate_bytes(&"é".repeat(300), 256);
        assert_eq!((owner.len(), owner.chars().count()), (256, 128));
        assert_eq!(truncate_bytes("host\n1:2", 256), "host_1:2");
        assert!(
            ledger_core::validate_token("lease_owner", &truncate_bytes(&"ü".repeat(999), 256), 256)
                .is_ok()
        );
    }

    fn with(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![
            ("LEDGER_PROJECTOR_DATABASE_URL", "postgres://p@db/ledger"),
            ("LEDGER_PROJECTION_TARGET_ID", "fuseki-main"),
            ("LEDGER_PROJECTION_QUERY_URL", "https://f/ledger/query"),
            ("LEDGER_PROJECTION_UPDATE_URL", "https://f/ledger/update"),
        ]
    }

    #[test]
    fn configuration_refusals_are_explicit() {
        let dir = std::env::temp_dir().join(format!("ledger-projector-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("pw");
        std::fs::write(&secret, "s3cret\n").unwrap();
        let secret = secret.to_str().unwrap().to_owned();
        let err = |extra: &[(&str, &str)]| {
            let mut pairs = base();
            pairs.extend_from_slice(extra);
            settings(&with(&pairs)).err()
        };
        // Credentials are required outside development…
        assert!(err(&[]).unwrap().contains("credentials are required"));
        // …never mixed…
        assert!(
            err(&[
                ("LEDGER_PROJECTION_USERNAME", "p"),
                ("LEDGER_PROJECTION_PASSWORD_FILE", &secret),
                ("LEDGER_PROJECTION_TOKEN_FILE", &secret),
            ])
            .unwrap()
            .contains("not a mix")
        );
        // …and read from regular files only.
        assert!(
            err(&[
                ("LEDGER_PROJECTION_USERNAME", "p"),
                ("LEDGER_PROJECTION_PASSWORD_FILE", dir.to_str().unwrap()),
            ])
            .unwrap()
            .contains("regular file")
        );
        // The development switch needs its exact value.
        assert!(
            err(&[("LEDGER_PROJECTOR_DEVELOPMENT", "yes")])
                .unwrap()
                .contains("unrecognised")
        );
        // The lease must cover a step.
        assert!(
            err(&[
                ("LEDGER_PROJECTION_TOKEN_FILE", &secret),
                ("LEDGER_PROJECTOR_LEASE_SECONDS", "60"),
            ])
            .unwrap()
            .contains("LEASE_SECONDS")
        );
        // State limit below the request limit.
        assert!(
            err(&[
                ("LEDGER_PROJECTION_TOKEN_FILE", &secret),
                ("LEDGER_PROJECTOR_MAX_STATE_BYTES", "999999999"),
            ])
            .unwrap()
            .contains("MAX_STATE_BYTES")
        );
        // A complete production configuration is accepted, credentials from the file.
        let mut pairs = base();
        pairs.push(("LEDGER_PROJECTION_USERNAME", "projector"));
        pairs.push(("LEDGER_PROJECTION_PASSWORD_FILE", &secret));
        let ok = settings(&with(&pairs)).unwrap();
        assert!(
            matches!(ok.target.credentials, TargetCredentials::Basic { ref password, .. } if password == "s3cret")
        );
        assert!(!ok.target.allow_insecure_loopback);
        // Development allows no credentials.
        let mut pairs = base();
        pairs.push(("LEDGER_PROJECTOR_DEVELOPMENT", DEVELOPMENT_SWITCH));
        assert!(matches!(
            settings(&with(&pairs)).unwrap().target.credentials,
            TargetCredentials::None
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
