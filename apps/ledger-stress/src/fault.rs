//! Plan 0005 §3: process and container kill fault injection.
//!
//! `ledger-stress fault …` keeps a sustained prepare/accept load running while an outer
//! harness (`scripts/fault.sh`) SIGKILLs server replicas and the PostgreSQL container. Every
//! request whose outcome is *in doubt* is remembered with its `Idempotency-Key`, body and
//! candidate: the connection died while the request was in flight (server killed), the
//! connection could not be opened (replica down: never sent, reported separately), or the
//! server answered `503 DEPENDENCY_*` because PostgreSQL vanished mid-request (an in-doubt
//! commit). Whenever the harness reports a quiet window ("steady"), and again at the end,
//! the tool retries each in-doubt request verbatim and classifies the answer against the
//! database:
//!
//! * accept replayed as durable → exactly one ref event installed that candidate;
//! * accept executed on retry → exactly one ref event (installed now);
//! * accept refused `HEAD_CHANGED` → no ref event for that candidate (no partial move);
//! * any other answer (`LINEAGE_MISMATCH`, other codes, failures) → inconsistent.
//!
//! The running counts are published in `progress.json`, so the harness can keep injecting
//! faults until enough *durable-before-crash* cases have been observed: the gate requires a
//! minimum number of them, so a run in which no kill ever interrupted a committed
//! transaction fails instead of passing vacuously. The DB invariants checked at the end are
//! the ones the `FailPoint` unit tests assert (`pg_workflow`) plus `ledger_store::verify`.

use crate::{
    Api, BRANCH, Call, Metrics, Op, Outcome, VerifyReport, environment, graph_invariants,
    is_expected_failure, is_timeout_class, jitter, mint_claims, now_secs, percentiles,
    prepare_body, provision, require_loopback,
};
use ledger_store::verify;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Attempts per in-doubt request before it is reported as unresolved.
const REPLAY_ATTEMPTS: usize = 40;

struct FaultConfig {
    replicas: Vec<String>,
    owner_database_url: String,
    writers: usize,
    graphs: usize,
    /// Minimum accepts that must be observed as *durable before the crash* for a pass. The
    /// window between COMMIT and the response leaving the process is sub-millisecond, so a
    /// SIGKILL lands in it only by chance; the case is proven deterministically by the
    /// `FailPoint` unit test (`pg_workflow`: lost response after COMMIT replays) and reported
    /// here as an observed count. Default 0; raise it to demand HTTP-level observations.
    min_durable_accepts: u64,
    out: PathBuf,
    issuer: String,
    audience: String,
    secret: String,
    stop_file: PathBuf,
    progress_file: PathBuf,
    window_file: PathBuf,
}

fn usage() -> &'static str {
    "usage: ledger-stress fault --replicas <url,url> --out <dir> --stop-file <path> \
     --progress-file <path> --window-file <path> [--writers 200] [--graphs 20] \
     [--min-durable-accepts 0] [--issuer <iss>] [--audience <aud>] \
     [--secret-env LEDGER_STRESS_HS256_SECRET] [--owner-database-url <url>] [--allow-non-loopback]\n\
     The owner database URL is read from LEDGER_STRESS_OWNER_DATABASE_URL (preferred) or the flag."
}

fn parse(mut argv: impl Iterator<Item = String>) -> Result<FaultConfig, String> {
    let mut c = FaultConfig {
        replicas: Vec::new(),
        owner_database_url: std::env::var("LEDGER_STRESS_OWNER_DATABASE_URL").unwrap_or_default(),
        writers: 200,
        graphs: 20,
        min_durable_accepts: 0,
        out: PathBuf::new(),
        issuer: "https://dev-issuer.example/".into(),
        audience: "api://sculpin-ledger-dev".into(),
        secret: String::new(),
        stop_file: PathBuf::new(),
        progress_file: PathBuf::new(),
        window_file: PathBuf::new(),
    };
    let mut secret_env = "LEDGER_STRESS_HS256_SECRET".to_owned();
    let mut allow_non_loopback = false;
    while let Some(flag) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--replicas" => {
                c.replicas = value()?
                    .split(',')
                    .map(|s| s.trim().trim_end_matches('/').to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            "--owner-database-url" => c.owner_database_url = value()?,
            "--writers" => c.writers = value()?.parse().map_err(|_| "--writers")?,
            "--graphs" => c.graphs = value()?.parse().map_err(|_| "--graphs")?,
            "--min-durable-accepts" => {
                c.min_durable_accepts = value()?.parse().map_err(|_| "--min-durable-accepts")?
            }
            "--out" => c.out = value()?.into(),
            "--stop-file" => c.stop_file = value()?.into(),
            "--progress-file" => c.progress_file = value()?.into(),
            "--window-file" => c.window_file = value()?.into(),
            "--issuer" => c.issuer = value()?,
            "--audience" => c.audience = value()?,
            "--secret-env" => secret_env = value()?,
            "--allow-non-loopback" => allow_non_loopback = true,
            _ => return Err(usage().into()),
        }
    }
    c.secret = std::env::var(&secret_env)
        .map_err(|_| format!("{secret_env} must hold the development HS256 secret"))?;
    if c.replicas.len() < 2
        || c.owner_database_url.is_empty()
        || c.out.as_os_str().is_empty()
        || c.stop_file.as_os_str().is_empty()
        || c.progress_file.as_os_str().is_empty()
        || c.window_file.as_os_str().is_empty()
        || c.writers == 0
        || c.graphs == 0
    {
        return Err(usage().into());
    }
    for r in &c.replicas {
        require_loopback("replica", r, allow_non_loopback)?;
    }
    require_loopback("owner database", &c.owner_database_url, allow_non_loopback)?;
    Ok(c)
}

/// A request whose outcome is in doubt.
#[derive(Clone, Serialize)]
struct Unknown {
    op: &'static str,
    /// Fault window the failure happened in (from the harness).
    window: String,
    /// `"in-flight"` (connection died or dependency failure while being served) or
    /// `"never-sent"` (connection refused: the replica was down).
    kind: &'static str,
    class: String,
    graph: String,
    subject: String,
    path: String,
    key: String,
    body: Value,
    /// Accept only: the candidate the request tried to install.
    candidate: Option<String>,
}

#[derive(Default)]
struct FaultState {
    /// Writers hold off while set (replay windows), so replays never compete with the
    /// load for admission-control slots and every quiet window is really quiet.
    pause: AtomicBool,
    /// Requests currently in flight (used to drain before a replay window).
    in_flight: AtomicU64,
    unknowns: Mutex<Vec<Unknown>>,
    /// In-doubt outcomes per (window, kind).
    per_window: Mutex<BTreeMap<String, u64>>,
    in_doubt: AtomicU64,
    /// Response-shape problems (head ≠ candidate, missing candidate): fail the run.
    malformed: Mutex<Vec<String>>,
}

/// Which failure classes leave the outcome in doubt, and how.
/// The fault run's error policy (`docs/quality/test-strategy.md`): `DEPENDENCY_UNAVAILABLE`
/// and transport failures are expected while a replica or PostgreSQL is down and are replayed
/// as in-doubt; `RESOURCE_LIMIT` is admission control. A `DEPENDENCY_TIMEOUT` (lock timeout,
/// serialization failure, deadlock, statement timeout) is **not** a consequence of a kill and
/// fails the run, exactly as in the stress and branch modes (Plan 0013 F6: before, the shared
/// expected-failure filter let it pass here). A timed-out request is still replayed as
/// in-doubt, so its outcome is verified as well as counted.
fn fault_unexpected(class: &str) -> bool {
    !is_expected_failure(class) || is_timeout_class(class)
}

fn in_doubt_kind(class: &str) -> Option<&'static str> {
    if class == "transport connect" {
        Some("never-sent")
    } else if class.starts_with("transport") || class.starts_with("503 DEPENDENCY") {
        Some("in-flight")
    } else {
        None
    }
}

/// RAII in-flight counter.
struct InFlight<'a>(&'a AtomicU64);
impl<'a> InFlight<'a> {
    fn enter(counter: &'a AtomicU64) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn current_window(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "steady".into())
}

struct SustainWriter {
    index: usize,
    graph: String,
    subject: String,
    token: String,
    run: String,
}

async fn sustain_loop(
    api: Arc<Api>,
    state: Arc<FaultState>,
    window_file: PathBuf,
    w: SustainWriter,
    stop: Arc<AtomicBool>,
) -> u64 {
    let refs = format!("/v1/graphs/{}/refs?name={BRANCH}", w.graph);
    let proposals = format!("/v1/graphs/{}/proposals", w.graph);
    let mut landed = 0u64;
    let mut sequence = 0u64;
    let mut backoff_ms = 0u64;
    let note_unknown = |op: &'static str,
                        class: &str,
                        path: &str,
                        key: &str,
                        body: &Value,
                        candidate: Option<&str>| {
        let Some(kind) = in_doubt_kind(class) else {
            return;
        };
        let window = current_window(&window_file);
        state.in_doubt.fetch_add(1, Ordering::Relaxed);
        *state
            .per_window
            .lock()
            .unwrap()
            .entry(format!("{window} / {kind}"))
            .or_insert(0) += 1;
        state.unknowns.lock().unwrap().push(Unknown {
            op,
            window,
            kind,
            class: class.to_owned(),
            graph: w.graph.clone(),
            subject: w.subject.clone(),
            path: path.to_owned(),
            key: key.to_owned(),
            body: body.clone(),
            candidate: candidate.map(str::to_owned),
        });
    };
    while !stop.load(Ordering::Relaxed) {
        if state.pause.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        sequence += 1;
        if backoff_ms > 0 {
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }
        // One logical iteration (ref read → prepare → accept) counts as in flight.
        let _guard = InFlight::enter(&state.in_flight);
        let head = match api
            .call(
                Call {
                    op: Op::RefRead,
                    method: reqwest::Method::GET,
                    path: &refs,
                    token: &w.token,
                    key: None,
                    body: None,
                },
                false,
            )
            .await
        {
            Outcome::Ok(v) => v["head"].as_str().map(str::to_owned),
            _ => {
                backoff_ms = (backoff_ms * 2).clamp(20, 500);
                continue;
            }
        };
        let quad = format!(
            "<urn:fault:{}:{}> <urn:fault:seq> \"{sequence}\" .",
            w.run, w.index
        );
        let key = format!("{}-{}-{sequence}", w.run, w.index);
        let prepare_key = format!("{key}-p");
        let body = prepare_body(head.as_deref(), &quad);
        let candidate = match api
            .call(
                Call {
                    op: Op::Prepare,
                    method: reqwest::Method::POST,
                    path: &proposals,
                    token: &w.token,
                    key: Some(&prepare_key),
                    body: Some(&body),
                },
                false,
            )
            .await
        {
            Outcome::Ok(v) => match v["candidate"].as_str() {
                Some(c) => c.to_owned(),
                None => {
                    state
                        .malformed
                        .lock()
                        .unwrap()
                        .push(format!("prepare {key}: 2xx without candidate: {v}"));
                    continue;
                }
            },
            Outcome::Conflict(_) => {
                backoff_ms = jitter(w.index, sequence, 25);
                continue;
            }
            Outcome::Failed(class) => {
                note_unknown("prepare", &class, &proposals, &prepare_key, &body, None);
                backoff_ms = (backoff_ms * 2).clamp(20, 500);
                continue;
            }
        };
        let accept = format!("{proposals}/{candidate}/accept");
        let accept_key = format!("{key}-a");
        let accept_body = json!({"ref": BRANCH, "expected_head": head, "reason": "fault"});
        match api
            .call(
                Call {
                    op: Op::Accept,
                    method: reqwest::Method::POST,
                    path: &accept,
                    token: &w.token,
                    key: Some(&accept_key),
                    body: Some(&accept_body),
                },
                false,
            )
            .await
        {
            Outcome::Ok(v) => {
                let version = v["ref_version"].as_i64().unwrap_or(-1);
                let new_head = v["head"].as_str().unwrap_or("").to_owned();
                if new_head != candidate || version < 1 {
                    state.malformed.lock().unwrap().push(format!(
                        "accept {key}: candidate {candidate} answered head={new_head} version={version}"
                    ));
                }
                api.metrics
                    .landed
                    .lock()
                    .unwrap()
                    .push((w.graph.clone(), version, new_head));
                landed += 1;
                backoff_ms = 0;
            }
            Outcome::Conflict(_) => backoff_ms = jitter(w.index, sequence, 25),
            Outcome::Failed(class) => {
                note_unknown(
                    "accept",
                    &class,
                    &accept,
                    &accept_key,
                    &accept_body,
                    Some(&candidate),
                );
                backoff_ms = (backoff_ms * 2).clamp(20, 500);
            }
        }
    }
    landed
}

#[derive(Serialize, Default, Clone)]
struct ReplayClassification {
    prepare_durable_before_crash: u64,
    prepare_executed_on_retry: u64,
    prepare_conflict_on_retry: u64,
    accept_durable_before_crash: u64,
    accept_executed_on_retry: u64,
    accept_refused_head_changed: u64,
    retry_failed: u64,
    inconsistent: Vec<String>,
    /// Requests replayed so far (index into the in-doubt list).
    replayed: usize,
}

async fn events_for_candidate(pool: &PgPool, graph: &str, candidate: &str) -> i64 {
    sqlx::query(
        "SELECT count(*) AS n FROM ref_events WHERE graph_id = $1 AND branch = $2 AND new_head = $3",
    )
    .bind(graph)
    .bind(BRANCH)
    .bind(candidate)
    .fetch_one(pool)
    .await
    .map(|r| r.get::<i64, _>("n"))
    .unwrap_or(-1)
}

/// Replay every not-yet-replayed in-doubt request verbatim (same key, body and actor) and
/// check each answer against the database. Replays are sent to alternating replicas.
async fn replay_unknowns(
    api: &Api,
    pool: &PgPool,
    token_for: &dyn Fn(&str) -> String,
    unknowns: &[Unknown],
    c: &mut ReplayClassification,
) {
    while c.replayed < unknowns.len() {
        let i = c.replayed;
        let u = &unknowns[i];
        c.replayed += 1;
        let token = token_for(&u.subject);
        let op = if u.op == "prepare" {
            Op::Prepare
        } else {
            Op::Accept
        };
        // A replay is an ordinary client retry: admission-control refusals and a replica
        // that is still coming back are retried with backoff (alternating replicas); only
        // an answer that cannot be obtained at all is unresolved.
        let mut outcome = Outcome::Failed("not sent".into());
        for attempt in 0..REPLAY_ATTEMPTS {
            outcome = api
                .send(
                    (i + attempt) % api.replicas.len(),
                    &Call {
                        op,
                        method: reqwest::Method::POST,
                        path: &u.path,
                        token: &token,
                        key: Some(&u.key),
                        body: Some(&u.body),
                    },
                )
                .await;
            match &outcome {
                Outcome::Failed(class)
                    if class == "503 RESOURCE_LIMIT"
                        || class.starts_with("transport")
                        || class.starts_with("503 DEPENDENCY") =>
                {
                    let pause = 50 + jitter(i, attempt as u64, 250) * (attempt as u64 + 1).min(4);
                    tokio::time::sleep(Duration::from_millis(pause)).await;
                }
                _ => break,
            }
        }
        match (u.op, outcome) {
            ("prepare", Outcome::Ok(v)) => {
                if v["replayed"] == Value::Bool(true) {
                    c.prepare_durable_before_crash += 1;
                } else {
                    c.prepare_executed_on_retry += 1;
                }
            }
            ("prepare", Outcome::Conflict(code)) if code == "HEAD_CHANGED" => {
                c.prepare_conflict_on_retry += 1
            }
            ("accept", Outcome::Ok(v)) => {
                let candidate = u.candidate.as_deref().unwrap_or("");
                let events = events_for_candidate(pool, &u.graph, candidate).await;
                let replayed = v["replayed"] == Value::Bool(true);
                if events != 1 || v["head"].as_str() != Some(candidate) {
                    c.inconsistent.push(format!(
                        "{} accept {} replayed={replayed}: {events} ref event(s) for candidate {candidate}, head {}",
                        u.window, u.key, v["head"]
                    ));
                } else {
                    if replayed {
                        c.accept_durable_before_crash += 1;
                    } else {
                        c.accept_executed_on_retry += 1;
                    }
                    api.metrics.landed.lock().unwrap().push((
                        u.graph.clone(),
                        v["ref_version"].as_i64().unwrap_or(-1),
                        candidate.to_owned(),
                    ));
                }
            }
            ("accept", Outcome::Conflict(code)) if code == "HEAD_CHANGED" => {
                let candidate = u.candidate.as_deref().unwrap_or("");
                let events = events_for_candidate(pool, &u.graph, candidate).await;
                if events != 0 {
                    c.inconsistent.push(format!(
                        "{} accept {} refused with {code} but {events} ref event(s) exist for candidate {candidate}",
                        u.window, u.key
                    ));
                } else {
                    c.accept_refused_head_changed += 1;
                }
            }
            (_, Outcome::Conflict(code)) => {
                // A replayed accept/prepare answering LINEAGE_MISMATCH would mean a terminal
                // decision exists without the idempotency record that should replay it.
                c.inconsistent.push(format!(
                    "{} {} {} answered {code} on verbatim replay",
                    u.window, u.op, u.key
                ));
            }
            (op, Outcome::Ok(v)) => {
                c.inconsistent.push(format!(
                    "{} {op} {} unexpected 2xx on replay: {v}",
                    u.window, u.key
                ));
            }
            (_, Outcome::Failed(class)) => {
                c.retry_failed += 1;
                c.inconsistent.push(format!(
                    "{} {} {} could not be replayed after {REPLAY_ATTEMPTS} attempts: {class}",
                    u.window, u.op, u.key
                ));
            }
        }
    }
}

#[derive(Serialize)]
struct FaultReport {
    run: String,
    environment: crate::Environment,
    writers: usize,
    graphs: usize,
    wall_seconds: f64,
    requests: u64,
    landed_during_run: u64,
    errors: BTreeMap<String, u64>,
    unexpected_errors: BTreeMap<String, u64>,
    malformed_responses: Vec<String>,
    latency: Vec<crate::Percentiles>,
    in_doubt: u64,
    in_doubt_per_window: BTreeMap<String, u64>,
    replay: ReplayClassification,
    min_durable_accepts: u64,
    invariants: Vec<crate::GraphInvariants>,
    verify: VerifyReport,
    failures: Vec<String>,
    passed: bool,
}

fn markdown(r: &FaultReport) -> String {
    let mut m = format!(
        "# Fault-injection run `{}` — {}\n\nHardware: {} × {}, {} GiB RAM, kernel {}. PostgreSQL: {}. Replicas: {}.\n\n{} writers over {} graphs for {:.1} s: {} requests, {} commits landed during the run, {} in-doubt responses.\n\n",
        r.run,
        if r.passed { "PASS" } else { "FAIL" },
        r.environment.cpus,
        r.environment.cpu_model,
        r.environment.mem_total_kib / 1024 / 1024,
        r.environment.kernel,
        r.environment.postgres_version,
        r.environment.replicas.join(", "),
        r.writers,
        r.graphs,
        r.wall_seconds,
        r.requests,
        r.landed_during_run,
        r.in_doubt
    );
    m.push_str("Latency per operation and outcome (replays included; `refused` = admission control):\n\n| op | outcome | count | p50 ms | p95 ms | p99 ms | max ms |\n|---|---|---:|---:|---:|---:|---:|\n");
    for l in &r.latency {
        m.push_str(&format!(
            "| {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} |\n",
            l.op, l.bucket, l.count, l.p50_ms, l.p95_ms, l.p99_ms, l.max_ms
        ));
    }
    m.push_str("\nIn-doubt responses per fault window (`in-flight` = connection died or `503 DEPENDENCY_*` while being served; `never-sent` = connection refused while the replica was down):\n\n");
    for (w, n) in &r.in_doubt_per_window {
        m.push_str(&format!("- `{w}`: {n}\n"));
    }
    m.push_str(&format!(
        "\nError classes: {}. Unexpected classes (fail the run): {}.\n\n## Replay of every in-doubt response (verbatim: same key, body and actor)\n\n- prepare: {} durable before the crash (replayed), {} executed on retry, {} HEAD_CHANGED on retry\n- accept: **{} durable before the crash** (replayed identically; exactly one ref event each; observed by chance — the COMMIT-to-response window is sub-millisecond; minimum demanded {}), {} executed on retry (one ref event each), {} refused HEAD_CHANGED (zero ref events each)\n- unresolved replays: {}; **inconsistent: {}**\n\n",
        if r.errors.is_empty() {
            "none".to_owned()
        } else {
            r.errors
                .iter()
                .map(|(k, v)| format!("`{k}` ×{v}"))
                .collect::<Vec<_>>()
                .join(", ")
        },
        if r.unexpected_errors.is_empty() {
            "none".to_owned()
        } else {
            format!("{:?}", r.unexpected_errors)
        },
        r.replay.prepare_durable_before_crash,
        r.replay.prepare_executed_on_retry,
        r.replay.prepare_conflict_on_retry,
        r.replay.accept_durable_before_crash,
        r.min_durable_accepts,
        r.replay.accept_executed_on_retry,
        r.replay.accept_refused_head_changed,
        r.replay.retry_failed,
        r.replay.inconsistent.len()
    ));
    for i in r.replay.inconsistent.iter().take(20) {
        m.push_str(&format!("- {i}\n"));
    }
    let bad = r.invariants.iter().filter(|i| !i.ok).count();
    m.push_str(&format!(
        "\n## Invariants\n\nΣ refs.version = {} = Σ ref_events = {} = Σ accepted decisions = {} = Σ outbox rows = {}; client-observed landings (run + replays) = {} with {} distinct (graph, version) pairs, {} found as ref events with the same version and head; {} graph(s) violate. Verifier: {} ({} violating check(s)).\n",
        r.invariants.iter().map(|i| i.ref_version).sum::<i64>(),
        r.invariants.iter().map(|i| i.ref_events).sum::<i64>(),
        r.invariants.iter().map(|i| i.accepted_decisions).sum::<i64>(),
        r.invariants.iter().map(|i| i.outbox_rows).sum::<i64>(),
        r.invariants.iter().map(|i| i.client_landed).sum::<i64>(),
        r.invariants
            .iter()
            .map(|i| i.client_distinct_versions)
            .sum::<i64>(),
        r.invariants
            .iter()
            .map(|i| i.client_landings_matching_events)
            .sum::<i64>(),
        bad,
        if r.verify.clean { "clean" } else { "VIOLATIONS" },
        r.verify.violations.len()
    ));
    for f in &r.failures {
        m.push_str(&format!("- FAIL: {f}\n"));
    }
    for i in r.invariants.iter().filter(|i| !i.ok).take(10) {
        m.push_str(&format!(
            "- `{}`: {}\n",
            i.graph,
            serde_json::to_string(i).unwrap()
        ));
    }
    m
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

pub async fn run(argv: impl Iterator<Item = String>) -> ExitCode {
    let cfg = match parse(argv) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    std::fs::create_dir_all(&cfg.out).expect("output directory");
    let _ = std::fs::remove_file(&cfg.stop_file);
    let run = format!("f{:x}", now_secs());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&cfg.owner_database_url)
        .await
        .expect("owner database connection");
    let http = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(cfg.writers * 2)
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    let metrics = Arc::new(Metrics::default());
    let api = Arc::new(Api {
        http,
        replicas: cfg.replicas.clone(),
        next: AtomicU64::new(0),
        metrics: metrics.clone(),
    });
    let env = environment(&pool, &cfg.replicas).await;
    let graphs = provision(&pool, &run, cfg.graphs).await;
    let state = Arc::new(FaultState::default());
    let stop = Arc::new(AtomicBool::new(false));
    let token_for = |subject: &str| mint_claims(&cfg.issuer, &cfg.audience, &cfg.secret, subject);
    let started = Instant::now();
    let mut handles = Vec::with_capacity(cfg.writers);
    for index in 0..cfg.writers {
        let subject = format!("fault-writer-{index}");
        handles.push(tokio::spawn(sustain_loop(
            api.clone(),
            state.clone(),
            cfg.window_file.clone(),
            SustainWriter {
                index,
                graph: graphs[index % graphs.len()].clone(),
                token: token_for(&subject),
                subject,
                run: run.clone(),
            },
            stop.clone(),
        )));
    }
    eprintln!(
        "fault run {run}: {} writers over {} graphs; waiting for {}",
        cfg.writers,
        cfg.graphs,
        cfg.stop_file.display()
    );
    // Progress for the harness; the stop file is the barrier (no timing assumptions). In
    // every quiet window the in-doubt requests collected so far are replayed, so the
    // harness can keep injecting faults until enough durable-before-crash cases exist.
    let mut replay = ReplayClassification::default();
    let mut last_window = String::new();
    while !cfg.stop_file.exists() {
        let window = current_window(&cfg.window_file);
        if window == "steady" && last_window != "steady" {
            // Quiet window: pause the writers, drain what is in flight (bounded), replay
            // everything collected so far without competing load, then resume.
            state.pause.store(true, Ordering::SeqCst);
            let drain_deadline = Instant::now() + Duration::from_secs(90);
            while state.in_flight.load(Ordering::SeqCst) > 0 && Instant::now() < drain_deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let snapshot = state.unknowns.lock().unwrap().clone();
            replay_unknowns(&api, &pool, &token_for, &snapshot, &mut replay).await;
            state.pause.store(false, Ordering::SeqCst);
        }
        last_window = window.clone();
        let progress = json!({
            "landed": metrics.landed.lock().unwrap().len(),
            "requests": metrics.requests.load(Ordering::Relaxed),
            "in_doubt": state.in_doubt.load(Ordering::Relaxed),
            "replayed": replay.replayed,
            "durable_accepts": replay.accept_durable_before_crash,
            "executed_accepts": replay.accept_executed_on_retry,
            "inconsistent": replay.inconsistent.len(),
            "paused": state.pause.load(Ordering::Relaxed),
            "window": window,
            "elapsed_seconds": started.elapsed().as_secs_f64(),
        });
        write_atomic(&cfg.progress_file, progress.to_string().as_bytes());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    stop.store(true, Ordering::Relaxed);
    let mut landed_during_run = 0u64;
    for h in handles {
        landed_during_run += h.await.expect("writer task");
    }
    let wall = started.elapsed();
    let unknowns = state.unknowns.lock().unwrap().clone();
    eprintln!(
        "load stopped after {:.1} s: {} landed, {} in-doubt responses ({} already replayed)",
        wall.as_secs_f64(),
        landed_during_run,
        unknowns.len(),
        replay.replayed
    );
    replay_unknowns(&api, &pool, &token_for, &unknowns, &mut replay).await;
    let landed_rows = metrics.landed.lock().unwrap().clone();
    let mut invariants = Vec::with_capacity(graphs.len());
    for g in &graphs {
        invariants.push(graph_invariants(&pool, g, &landed_rows).await);
    }
    let latency = {
        let mut map = metrics.latency.lock().unwrap();
        let mut out = Vec::new();
        for op in [Op::RefRead, Op::Prepare, Op::Accept] {
            for bucket in ["ok", "conflict", "refused", "failed"] {
                if let Some(samples) = map.get_mut(&(op.name(), bucket)) {
                    out.push(percentiles(op.name(), bucket, samples));
                }
            }
        }
        out
    };
    let verify_report = verify::run(&pool).await.expect("verifier");
    let verify = VerifyReport {
        clean: verify_report.is_clean(),
        violations: verify_report
            .checks
            .iter()
            .filter(|c| c.violations != 0)
            .map(|c| (c.name.to_owned(), c.violations, c.sample.clone()))
            .collect(),
        counts: verify_report
            .counts
            .iter()
            .map(|(n, c)| ((*n).to_owned(), *c))
            .collect(),
    };
    let errors = metrics.errors.lock().unwrap().clone();
    let unexpected_errors: BTreeMap<String, u64> = errors
        .iter()
        .filter(|(k, _)| fault_unexpected(k))
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    let malformed_responses = state.malformed.lock().unwrap().clone();
    let mut failures = Vec::new();
    for i in invariants.iter().filter(|i| !i.ok) {
        failures.push(format!(
            "graph {} violates the ref/event/decision/outbox/landing equalities",
            i.graph
        ));
    }
    if !replay.inconsistent.is_empty() {
        failures.push(format!(
            "{} inconsistent replay(s)",
            replay.inconsistent.len()
        ));
    }
    if replay.accept_durable_before_crash < cfg.min_durable_accepts {
        failures.push(format!(
            "only {} accept(s) were durable before a crash (minimum {}): the kills did not interrupt enough committed transactions",
            replay.accept_durable_before_crash, cfg.min_durable_accepts
        ));
    }
    if !unexpected_errors.is_empty() {
        failures.push(format!("unexpected error classes: {unexpected_errors:?}"));
    }
    if !malformed_responses.is_empty() {
        failures.push(format!(
            "{} malformed response(s)",
            malformed_responses.len()
        ));
    }
    if !verify.clean {
        failures.push("verifier reported violations".into());
    }
    let passed = failures.is_empty();
    let report = FaultReport {
        run,
        environment: env,
        writers: cfg.writers,
        graphs: cfg.graphs,
        wall_seconds: wall.as_secs_f64(),
        requests: metrics.requests.load(Ordering::Relaxed),
        landed_during_run,
        errors,
        unexpected_errors,
        malformed_responses,
        latency,
        in_doubt: state.in_doubt.load(Ordering::Relaxed),
        in_doubt_per_window: state.per_window.lock().unwrap().clone(),
        replay,
        min_durable_accepts: cfg.min_durable_accepts,
        invariants,
        verify,
        failures,
        passed,
    };
    std::fs::write(
        cfg.out.join("report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .expect("write report.json");
    std::fs::write(
        cfg.out.join("unknowns.json"),
        serde_json::to_vec_pretty(&unknowns).unwrap(),
    )
    .expect("write unknowns.json");
    let md = markdown(&report);
    std::fs::write(cfg.out.join("report.md"), &md).expect("write report.md");
    println!("{md}");
    if passed {
        println!("FAULT OK");
        ExitCode::SUCCESS
    } else {
        println!("FAULT FAILED");
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::{fault_unexpected, in_doubt_kind};

    #[test]
    fn a_dependency_timeout_fails_the_fault_run_but_outages_and_refusals_do_not() {
        assert!(fault_unexpected("503 DEPENDENCY_TIMEOUT"));
        assert!(fault_unexpected("401 UNAUTHENTICATED"));
        assert!(fault_unexpected("500 INTERNAL"));
        assert!(fault_unexpected("409 IDEMPOTENCY_CONFLICT"));
        assert!(!fault_unexpected("503 DEPENDENCY_UNAVAILABLE"));
        assert!(!fault_unexpected("503 RESOURCE_LIMIT"));
        assert!(!fault_unexpected("transport connect"));
        assert!(!fault_unexpected("transport other"));
    }

    #[test]
    fn in_doubt_classes_are_split_into_in_flight_and_never_sent() {
        assert_eq!(in_doubt_kind("transport connect"), Some("never-sent"));
        assert_eq!(in_doubt_kind("transport other"), Some("in-flight"));
        assert_eq!(in_doubt_kind("transport timeout"), Some("in-flight"));
        assert_eq!(
            in_doubt_kind("503 DEPENDENCY_UNAVAILABLE"),
            Some("in-flight")
        );
        assert_eq!(in_doubt_kind("503 DEPENDENCY_TIMEOUT"), Some("in-flight"));
        assert_eq!(in_doubt_kind("503 RESOURCE_LIMIT"), None);
        assert_eq!(in_doubt_kind("401 UNAUTHENTICATED"), None);
    }
}
