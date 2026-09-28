//! Plan 0005 §2 concurrent-writer qualification harness.
//!
//! Drives ≥1,000 concurrent prepare/accept clients through the public HTTP API against
//! several server replicas (chosen per request, so identical retries land on different
//! replicas), then proves the ledger invariants with SQL under the owner identity:
//! every ref version consumed exactly once, `refs.version = count(ref_events)`, every
//! client-observed landing `(version, head)` present as a ref event, no duplicate
//! publication under duplicated requests (same `Idempotency-Key`, two replicas), no
//! deadlock, no unexpected error class, and the read-only verifier clean. Latency
//! percentiles are reported separately for successful operations and for admission-control
//! refusals; the successful p99 must stay under a budget for the run to pass.
//!
//! Development/qualification tooling only: it mints development HS256 tokens, refuses
//! non-loopback targets unless told otherwise, and is never part of the runtime image. Run
//! through `scripts/stress.sh`.
//!
//! ```text
//! LEDGER_STRESS_OWNER_DATABASE_URL=postgres://… ledger-stress \
//!   --replicas http://127.0.0.1:8080,http://127.0.0.1:8081 --writers 1000 \
//!   --contended-seconds 60 --graphs 100 --commits-per-writer 3 --out target/stress/<run>
//! ```

mod bench;
mod branches;
mod fault;

use ledger_core::{GraphId, TenantId};
use ledger_store::{GraphStatus, NewGraph, PgGraphs, verify};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{
    collections::{BTreeMap, BTreeSet},
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const TENANT: &str = "tenant-stress";
const BRANCH: &str = "main";
const ROLES: [&str; 3] = ["ledger.read", "ledger.propose", "ledger.review"];
/// Every n-th writer duplicates each of its requests to a second replica (lost-response
/// simulation); the two answers must agree and exactly one may be the original execution.
const REPLAY_EVERY: usize = 10;
/// Attempts a writer may spend on one commit before giving up (reported, never hidden).
const MAX_ATTEMPTS_PER_COMMIT: u32 = 50_000;
/// PostgreSQL flushes `pg_stat_database` counters from idle backends within this interval
/// (PGSTAT_IDLE_INTERVAL = 10 s); the deadlock counter is read after it.
const STATS_FLUSH_WAIT: Duration = Duration::from_secs(11);

struct Config {
    replicas: Vec<String>,
    owner_database_url: String,
    writers: usize,
    contended_seconds: u64,
    graphs: usize,
    commits_per_writer: u32,
    /// Successful-operation p99 budget (milliseconds) for the run to pass.
    max_p99_ms: f64,
    /// Minimum commits a phase must land for its result to count.
    min_commits: u64,
    out: String,
    issuer: String,
    audience: String,
    secret: String,
    runtime_role: String,
}

fn usage() -> &'static str {
    "usage: ledger-stress --replicas <url,url,...> [--writers 1000] [--contended-seconds 60] \
     [--graphs 100] [--commits-per-writer 3] [--max-p99-ms 5000] [--min-commits 100] --out <dir> \
     [--issuer <iss>] [--audience <aud>] [--secret-env LEDGER_STRESS_HS256_SECRET] \
     [--runtime-role ledger_runtime] [--owner-database-url <url>] [--allow-non-loopback]\n\
     The owner database URL is read from LEDGER_STRESS_OWNER_DATABASE_URL (preferred) or the flag."
}

/// Qualification tooling writes thousands of commits under owner rights: refuse anything
/// that is not loopback unless the operator says so explicitly. Never echoes the URL.
fn require_loopback(what: &str, url: &str, allow: bool) -> Result<(), String> {
    if allow {
        return Ok(());
    }
    let parsed = url::Url::parse(url)
        .map_err(|_| format!("{what}: URL does not parse (value not shown)"))?;
    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        // Non-special schemes (postgres://) keep the host as an opaque name: parse it.
        Some(url::Host::Domain(name)) => {
            name.eq_ignore_ascii_case("localhost")
                || name
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        None => false,
    };
    if !loopback {
        return Err(format!(
            "{what}: host is not loopback; this tool writes qualification data with owner \
             rights and refuses non-loopback targets (override: --allow-non-loopback)"
        ));
    }
    if parsed.scheme().starts_with("http")
        && (!parsed.username().is_empty() || parsed.password().is_some())
    {
        return Err(format!("{what}: replica URLs must not carry credentials"));
    }
    // libpq-style URLs can redirect the connection with `host=`/`hostaddr=` parameters.
    if parsed
        .query_pairs()
        .any(|(k, _)| k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("hostaddr"))
    {
        return Err(format!(
            "{what}: host/hostaddr query parameters are not allowed"
        ));
    }
    Ok(())
}

fn parse(mut argv: impl Iterator<Item = String>) -> Result<Config, String> {
    let mut c = Config {
        replicas: Vec::new(),
        owner_database_url: std::env::var("LEDGER_STRESS_OWNER_DATABASE_URL").unwrap_or_default(),
        writers: 1000,
        contended_seconds: 60,
        graphs: 100,
        commits_per_writer: 3,
        max_p99_ms: 5000.0,
        min_commits: 100,
        out: String::new(),
        issuer: "https://dev-issuer.example/".into(),
        audience: "api://sculpin-ledger-dev".into(),
        secret: String::new(),
        runtime_role: "ledger_runtime".into(),
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
            "--contended-seconds" => {
                c.contended_seconds = value()?.parse().map_err(|_| "--contended-seconds")?
            }
            "--graphs" => c.graphs = value()?.parse().map_err(|_| "--graphs")?,
            "--commits-per-writer" => {
                c.commits_per_writer = value()?.parse().map_err(|_| "--commits-per-writer")?
            }
            "--max-p99-ms" => c.max_p99_ms = value()?.parse().map_err(|_| "--max-p99-ms")?,
            "--min-commits" => c.min_commits = value()?.parse().map_err(|_| "--min-commits")?,
            "--out" => c.out = value()?,
            "--issuer" => c.issuer = value()?,
            "--audience" => c.audience = value()?,
            "--secret-env" => secret_env = value()?,
            "--runtime-role" => c.runtime_role = value()?,
            "--allow-non-loopback" => allow_non_loopback = true,
            _ => return Err(usage().into()),
        }
    }
    c.secret = std::env::var(&secret_env)
        .map_err(|_| format!("{secret_env} must hold the development HS256 secret"))?;
    if c.replicas.len() < 2 {
        return Err("at least two replicas are required (multi-replica gate)".into());
    }
    if c.owner_database_url.is_empty() || c.out.is_empty() || c.writers == 0 || c.graphs == 0 {
        return Err(usage().into());
    }
    for r in &c.replicas {
        require_loopback("replica", r, allow_non_loopback)?;
    }
    require_loopback("owner database", &c.owner_database_url, allow_non_loopback)?;
    Ok(c)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn mint(c: &Config, subject: &str) -> String {
    mint_claims(&c.issuer, &c.audience, &c.secret, subject)
}

/// Development HS256 token for one writer subject (the tenant and roles are fixed), valid
/// for one hour.
fn mint_claims(issuer: &str, audience: &str, secret: &str, subject: &str) -> String {
    mint_claims_ttl(issuer, audience, secret, subject, 3600)
}

/// `mint_claims` with an explicit validity (the depth benchmark runs for hours).
fn mint_claims_ttl(issuer: &str, audience: &str, secret: &str, subject: &str, ttl: u64) -> String {
    mint_roles(issuer, audience, secret, subject, ttl, &ROLES)
}

/// `mint_claims_ttl` with explicit roles (the branch stress needs an administrator).
fn mint_roles(
    issuer: &str,
    audience: &str,
    secret: &str,
    subject: &str,
    ttl: u64,
    roles: &[&str],
) -> String {
    let claims = json!({
        "iss": issuer, "aud": audience, "exp": now_secs() + ttl, "nbf": now_secs() - 30,
        "tid": TENANT, "oid": subject, "sculpin_principal_type": "agent", "roles": roles,
    });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .expect("HS256 encoding of static claims")
}

// ---------------------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Op {
    RefRead,
    Prepare,
    Accept,
    BranchCreate,
    BranchDelete,
    BranchRestore,
    BranchRead,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::RefRead => "ref_read",
            Op::Prepare => "prepare",
            Op::Accept => "accept",
            Op::BranchCreate => "branch_create",
            Op::BranchDelete => "branch_delete",
            Op::BranchRestore => "branch_restore",
            Op::BranchRead => "branch_read",
        }
    }
}

/// Which histogram a sample belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Bucket {
    /// 2xx (including replays).
    Ok,
    /// CAS conflicts (`HEAD_CHANGED`/`LINEAGE_MISMATCH`), expected under contention.
    Conflict,
    /// `503 RESOURCE_LIMIT`: refused by admission control before any store work.
    Refused,
    /// Everything else (dependency failures, transport, unexpected codes).
    Failed,
}

impl Bucket {
    fn name(self) -> &'static str {
        match self {
            Bucket::Ok => "ok",
            Bucket::Conflict => "conflict",
            Bucket::Refused => "refused",
            Bucket::Failed => "failed",
        }
    }
}

/// Failure classes that are *expected* under overload or injected faults. Anything else
/// (401, 500, `409 IDEMPOTENCY_CONFLICT`, …) fails the run.
fn is_expected_failure(class: &str) -> bool {
    class.starts_with("transport")
        || class == "503 RESOURCE_LIMIT"
        || class.starts_with("503 DEPENDENCY")
}

/// A `503 DEPENDENCY_TIMEOUT` is how a lock timeout, a serialization failure or a
/// **deadlock** (40P01) reaches the client; the stress run must not produce any.
fn is_timeout_class(class: &str) -> bool {
    class == "503 DEPENDENCY_TIMEOUT"
}

#[derive(Default)]
struct Metrics {
    /// Latency samples in microseconds per (operation, bucket).
    latency: Mutex<BTreeMap<(&'static str, &'static str), Vec<u64>>>,
    /// `"<status> <code>"` or `"transport <kind>"` → count (non-2xx, non-conflict).
    errors: Mutex<BTreeMap<String, u64>>,
    /// HEAD_CHANGED / LINEAGE_MISMATCH responses (expected under contention).
    conflicts: AtomicU64,
    /// Accepted commits as observed by clients (duplicated pairs counted once).
    landed: Mutex<Vec<(String, i64, String)>>,
    /// Duplicated request pairs by how they resolved.
    pairs: Mutex<PairStats>,
    /// Response-shape problems (missing candidate, head ≠ candidate): fail the run.
    malformed: Mutex<Vec<String>>,
    requests: AtomicU64,
}

#[derive(Default, Clone, Serialize)]
struct PairStats {
    /// Both sides answered 2xx: compared field by field; exactly one original execution.
    compared: u64,
    /// Both sides answered with the same conflict code.
    both_conflict: u64,
    /// One side 2xx, the other refused by admission control or lost (acceptable).
    one_side_refused_or_lost: u64,
    /// Both sides refused/lost: nothing to compare.
    both_failed: u64,
    original_executions: u64,
    disagreements: Vec<String>,
}

impl Metrics {
    fn record(&self, op: Op, bucket: Bucket, elapsed: Duration) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.latency
            .lock()
            .unwrap()
            .entry((op.name(), bucket.name()))
            .or_default()
            .push(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX));
    }
    fn error(&self, class: String) {
        *self.errors.lock().unwrap().entry(class).or_insert(0) += 1;
    }
}

#[derive(Serialize, Clone)]
struct Percentiles {
    op: &'static str,
    bucket: &'static str,
    count: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    mean_ms: f64,
}

/// Nearest-rank percentiles (the p-th percentile is the ⌈p·n⌉-th smallest sample).
fn percentiles(op: &'static str, bucket: &'static str, samples: &mut [u64]) -> Percentiles {
    samples.sort_unstable();
    let pick = |q: f64| -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let rank = ((samples.len() as f64) * q).ceil() as usize;
        samples[rank.clamp(1, samples.len()) - 1] as f64 / 1000.0
    };
    let mean = if samples.is_empty() {
        0.0
    } else {
        samples.iter().map(|s| *s as f64).sum::<f64>() / samples.len() as f64 / 1000.0
    };
    Percentiles {
        op,
        bucket,
        count: samples.len(),
        p50_ms: pick(0.50),
        p95_ms: pick(0.95),
        p99_ms: pick(0.99),
        max_ms: pick(1.0),
        mean_ms: mean,
    }
}

// ---------------------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------------------

struct Api {
    http: reqwest::Client,
    replicas: Vec<String>,
    next: AtomicU64,
    metrics: Arc<Metrics>,
}

#[derive(Debug, Clone)]
enum Outcome {
    Ok(Value),
    /// Conflict class the writer recovers from by re-reading the head.
    Conflict(String),
    /// Anything else (auth, 5xx, transport): recorded, retried with backoff.
    Failed(String),
}

/// One HTTP request of the workload (borrowed so duplicates share the same body).
struct Call<'a> {
    op: Op,
    method: reqwest::Method,
    path: &'a str,
    token: &'a str,
    key: Option<&'a str>,
    body: Option<&'a Value>,
}

/// How two answers to one duplicated request relate.
#[derive(Debug, PartialEq, Eq)]
enum PairVerdict {
    /// Both 2xx, identical durable fields, exactly one original execution.
    Agree,
    /// Both the same conflict code.
    BothConflict,
    /// One 2xx, the other refused by admission control or lost in transport.
    OneSideRefusedOrLost,
    /// Neither side reached a durable answer.
    BothFailed,
    /// Anything that would indicate a duplicate execution or divergent replicas.
    Disagree(String),
}

fn classify_pair(a: &Outcome, b: &Outcome) -> PairVerdict {
    match (a, b) {
        (Outcome::Ok(x), Outcome::Ok(y)) => {
            let originals = [x, y]
                .iter()
                .filter(|v| v["replayed"] == Value::Bool(false))
                .count();
            let strip = |v: &Value| {
                let mut v = v.clone();
                v["correlation_id"] = Value::Null;
                v["replayed"] = Value::Null;
                v
            };
            if originals != 1 {
                PairVerdict::Disagree(format!("originals={originals} a={x} b={y}"))
            } else if strip(x) != strip(y) {
                PairVerdict::Disagree(format!("bodies differ a={x} b={y}"))
            } else {
                PairVerdict::Agree
            }
        }
        (Outcome::Conflict(x), Outcome::Conflict(y)) if x == y => PairVerdict::BothConflict,
        (Outcome::Conflict(x), Outcome::Conflict(y)) => {
            PairVerdict::Disagree(format!("conflict codes differ {x} vs {y}"))
        }
        (Outcome::Ok(_), Outcome::Failed(class)) | (Outcome::Failed(class), Outcome::Ok(_)) => {
            if is_expected_failure(class) && !is_timeout_class(class) {
                PairVerdict::OneSideRefusedOrLost
            } else {
                PairVerdict::Disagree(format!("one side ok, other {class}"))
            }
        }
        (Outcome::Ok(_), Outcome::Conflict(code)) | (Outcome::Conflict(code), Outcome::Ok(_)) => {
            PairVerdict::Disagree(format!("one side ok, other conflict {code}"))
        }
        (Outcome::Conflict(_), Outcome::Failed(class))
        | (Outcome::Failed(class), Outcome::Conflict(_)) => {
            if is_expected_failure(class) {
                PairVerdict::BothFailed
            } else {
                PairVerdict::Disagree(format!("one side conflict, other {class}"))
            }
        }
        (Outcome::Failed(x), Outcome::Failed(y)) => {
            if is_expected_failure(x) && is_expected_failure(y) {
                PairVerdict::BothFailed
            } else {
                PairVerdict::Disagree(format!("failures {x} / {y}"))
            }
        }
    }
}

impl Api {
    fn pick(&self) -> usize {
        (self.next.fetch_add(1, Ordering::Relaxed) as usize) % self.replicas.len()
    }

    async fn send(&self, replica: usize, c: &Call<'_>) -> Outcome {
        let (op, path, key, body) = (c.op, c.path, c.key, c.body);
        let url = format!("{}{}", self.replicas[replica], path);
        let mut request = self
            .http
            .request(c.method.clone(), url)
            .header("authorization", format!("Bearer {}", c.token));
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let started = Instant::now();
        let response = request.send().await;
        let (outcome, bucket) = match response {
            Ok(response) => {
                let status = response.status();
                let value: Value = response.json().await.unwrap_or(Value::Null);
                if status.is_success() {
                    (Outcome::Ok(value), Bucket::Ok)
                } else {
                    let code = value["code"].as_str().unwrap_or("?").to_owned();
                    if status.as_u16() == 409
                        && (code == "HEAD_CHANGED" || code == "LINEAGE_MISMATCH")
                    {
                        self.metrics.conflicts.fetch_add(1, Ordering::Relaxed);
                        (Outcome::Conflict(code), Bucket::Conflict)
                    } else if status.as_u16() == 404 && op == Op::RefRead {
                        (
                            Outcome::Ok(json!({"head": null, "version": null})),
                            Bucket::Ok,
                        )
                    } else {
                        let class = format!("{} {code}", status.as_u16());
                        self.metrics.error(class.clone());
                        let bucket = if class == "503 RESOURCE_LIMIT" {
                            Bucket::Refused
                        } else {
                            Bucket::Failed
                        };
                        (Outcome::Failed(class), bucket)
                    }
                }
            }
            Err(e) => {
                let kind = if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect"
                } else {
                    "other"
                };
                let class = format!("transport {kind}");
                self.metrics.error(class.clone());
                (Outcome::Failed(class), Bucket::Failed)
            }
        };
        self.metrics.record(op, bucket, started.elapsed());
        outcome
    }

    /// One logical request; `duplicate` sends it twice concurrently to two different
    /// replicas under the same key and cross-checks the answers.
    async fn call(&self, c: Call<'_>, duplicate: bool) -> Outcome {
        let (path, key) = (c.path, c.key);
        let first = self.pick();
        if !duplicate || key.is_none() {
            return self.send(first, &c).await;
        }
        let second = (first + 1) % self.replicas.len();
        let (a, b) = tokio::join!(self.send(first, &c), self.send(second, &c));
        let verdict = classify_pair(&a, &b);
        {
            let mut pairs = self.metrics.pairs.lock().unwrap();
            match &verdict {
                PairVerdict::Agree => {
                    pairs.compared += 1;
                    pairs.original_executions += 1;
                }
                PairVerdict::BothConflict => pairs.both_conflict += 1,
                PairVerdict::OneSideRefusedOrLost => pairs.one_side_refused_or_lost += 1,
                PairVerdict::BothFailed => pairs.both_failed += 1,
                PairVerdict::Disagree(why) => pairs
                    .disagreements
                    .push(format!("{path} key={} {why}", key.unwrap_or(""))),
            }
        }
        // The writer continues with the strongest answer available.
        match (a, b) {
            (Outcome::Ok(v), _) | (_, Outcome::Ok(v)) => Outcome::Ok(v),
            (Outcome::Conflict(c), _) | (_, Outcome::Conflict(c)) => Outcome::Conflict(c),
            (Outcome::Failed(f), _) => Outcome::Failed(f),
        }
    }
}

fn prepare_body(expected_head: Option<&str>, quad: &str) -> Value {
    json!({
        "ref": BRANCH,
        "expected_head": expected_head,
        "operations": [{"op": "add", "quad": quad}],
        "activity": "stress",
        "event_time": "2026-09-26T12:00:00Z",
        "evidence_refs": ["urn:evidence:stress"],
        "source_system": "ledger-stress",
        "message": "stress commit",
    })
}

// ---------------------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------------------

struct Writer {
    index: usize,
    graph: String,
    token: String,
    run: String,
}

#[derive(Default, Serialize, Clone)]
struct WriterOutcome {
    landed: u32,
    attempts: u32,
    gave_up: bool,
}

/// Land commits until `target` is reached, `stop` is raised or the attempt budget is spent.
async fn write_loop(api: Arc<Api>, w: Writer, target: u32, stop: Arc<AtomicBool>) -> WriterOutcome {
    let duplicate = w.index % REPLAY_EVERY == 0;
    let mut out = WriterOutcome::default();
    let refs = format!("/v1/graphs/{}/refs?name={BRANCH}", w.graph);
    let proposals = format!("/v1/graphs/{}/proposals", w.graph);
    let mut backoff_ms: u64 = 0;
    let mut sequence: u64 = 0;
    let budget = MAX_ATTEMPTS_PER_COMMIT.saturating_mul(target.max(1));
    while out.landed < target && !stop.load(Ordering::Relaxed) {
        if out.attempts >= budget {
            out.gave_up = true;
            break;
        }
        out.attempts += 1;
        sequence += 1;
        if backoff_ms > 0 {
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }
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
            "<urn:stress:{}:{}> <urn:stress:seq> \"{sequence}\" .",
            w.run, w.index
        );
        let key = format!("{}-{}-{sequence}", w.run, w.index);
        let candidate = match api
            .call(
                Call {
                    op: Op::Prepare,
                    method: reqwest::Method::POST,
                    path: &proposals,
                    token: &w.token,
                    key: Some(&format!("{key}-p")),
                    body: Some(&prepare_body(head.as_deref(), &quad)),
                },
                duplicate,
            )
            .await
        {
            Outcome::Ok(v) => match v["candidate"].as_str() {
                Some(c) => c.to_owned(),
                None => {
                    api.metrics
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
            Outcome::Failed(_) => {
                backoff_ms = (backoff_ms * 2).clamp(20, 500);
                continue;
            }
        };
        let accept = format!("{proposals}/{candidate}/accept");
        match api
            .call(
                Call {
                    op: Op::Accept,
                    method: reqwest::Method::POST,
                    path: &accept,
                    token: &w.token,
                    key: Some(&format!("{key}-a")),
                    body: Some(&json!({"ref": BRANCH, "expected_head": head, "reason": "stress"})),
                },
                duplicate,
            )
            .await
        {
            Outcome::Ok(v) => {
                let version = v["ref_version"].as_i64().unwrap_or(-1);
                let new_head = v["head"].as_str().unwrap_or("").to_owned();
                if new_head != candidate || version < 1 {
                    api.metrics.malformed.lock().unwrap().push(format!(
                        "accept {key}: candidate {candidate} answered head={new_head} version={version}"
                    ));
                }
                api.metrics
                    .landed
                    .lock()
                    .unwrap()
                    .push((w.graph.clone(), version, new_head));
                out.landed += 1;
                backoff_ms = 0;
            }
            Outcome::Conflict(_) => backoff_ms = jitter(w.index, sequence, 25),
            Outcome::Failed(_) => backoff_ms = (backoff_ms * 2).clamp(20, 500),
        }
    }
    out
}

/// Deterministic pseudo-random backoff in `0..max_ms` (no RNG dependency; the writer index
/// and attempt sequence make the schedule reproducible).
fn jitter(index: usize, sequence: u64, max_ms: u64) -> u64 {
    let mut x = (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ sequence.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 31;
    x % max_ms.max(1)
}

// ---------------------------------------------------------------------------------------
// PostgreSQL side: provisioning, session sampling, invariants
// ---------------------------------------------------------------------------------------

#[derive(Serialize, Default, Clone)]
struct SessionSample {
    samples: u64,
    max_total: i64,
    max_active: i64,
    max_lock_waiting: i64,
    mean_active: f64,
}

async fn sample_sessions(pool: PgPool, role: String, stop: Arc<AtomicBool>) -> SessionSample {
    let mut out = SessionSample::default();
    let mut active_sum: i64 = 0;
    while !stop.load(Ordering::Relaxed) {
        if let Ok(row) = sqlx::query(
            "SELECT count(*) AS total, \
                    count(*) FILTER (WHERE state = 'active') AS active, \
                    count(*) FILTER (WHERE wait_event_type = 'Lock') AS lock_waiting \
             FROM pg_stat_activity WHERE usename = $1 AND datname = current_database()",
        )
        .bind(&role)
        .fetch_one(&pool)
        .await
        {
            let total: i64 = row.get("total");
            let active: i64 = row.get("active");
            let waiting: i64 = row.get("lock_waiting");
            out.samples += 1;
            out.max_total = out.max_total.max(total);
            out.max_active = out.max_active.max(active);
            out.max_lock_waiting = out.max_lock_waiting.max(waiting);
            active_sum += active;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if out.samples > 0 {
        out.mean_active = active_sum as f64 / out.samples as f64;
    }
    out
}

/// `pg_stat_database.deadlocks` for this database; `None` if it could not be read (a
/// failed read never passes as "no deadlocks").
async fn deadlocks(pool: &PgPool) -> Option<i64> {
    sqlx::query("SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()")
        .fetch_one(pool)
        .await
        .ok()
        .map(|r| r.get::<i64, _>("deadlocks"))
}

#[derive(Serialize, Clone)]
struct GraphInvariants {
    graph: String,
    ref_version: i64,
    ref_events: i64,
    distinct_event_versions: i64,
    max_event_version: i64,
    accepted_decisions: i64,
    outbox_rows: i64,
    distinct_new_heads: i64,
    client_landed: i64,
    client_distinct_versions: i64,
    /// Client landings `(version, head)` that exist as a ref event of this graph/branch.
    client_landings_matching_events: i64,
    ok: bool,
}

async fn graph_invariants(
    pool: &PgPool,
    graph: &str,
    landed: &[(String, i64, String)],
) -> GraphInvariants {
    let row = sqlx::query(
        "SELECT \
           coalesce((SELECT version FROM refs WHERE graph_id = $1 AND branch = $2), 0) AS ref_version, \
           (SELECT count(*) FROM ref_events WHERE graph_id = $1 AND branch = $2) AS ref_events, \
           (SELECT count(DISTINCT new_version) FROM ref_events WHERE graph_id = $1 AND branch = $2) AS distinct_versions, \
           coalesce((SELECT max(new_version) FROM ref_events WHERE graph_id = $1 AND branch = $2), 0) AS max_version, \
           (SELECT count(*) FROM decisions d JOIN ref_events e ON e.event_id = d.ref_event_id \
              WHERE e.graph_id = $1 AND e.branch = $2 AND d.decision = 'accepted') AS accepted, \
           (SELECT count(*) FROM projection_outbox WHERE graph_id = $1 AND branch = $2) AS outbox, \
           (SELECT count(DISTINCT new_head) FROM ref_events WHERE graph_id = $1 AND branch = $2) AS distinct_heads",
    )
    .bind(graph)
    .bind(BRANCH)
    .fetch_one(pool)
    .await
    .expect("invariant query");
    let ref_version: i64 = row.get("ref_version");
    let ref_events: i64 = row.get("ref_events");
    let distinct_event_versions: i64 = row.get("distinct_versions");
    let max_event_version: i64 = row.get("max_version");
    let accepted_decisions: i64 = row.get("accepted");
    let outbox_rows: i64 = row.get("outbox");
    let distinct_new_heads: i64 = row.get("distinct_heads");
    let mine: Vec<&(String, i64, String)> = landed.iter().filter(|(g, _, _)| g == graph).collect();
    let client_landed = mine.len() as i64;
    let client_distinct_versions = mine
        .iter()
        .map(|(_, v, _)| *v)
        .collect::<BTreeSet<_>>()
        .len() as i64;
    // Every client-observed `(version, head)` must be a real ref event of this ref: the
    // server's answers are checked against the audit trail, not only counted.
    let versions: Vec<i64> = mine.iter().map(|(_, v, _)| *v).collect();
    let heads: Vec<String> = mine.iter().map(|(_, _, h)| h.clone()).collect();
    let client_landings_matching_events: i64 = sqlx::query(
        "SELECT count(*) AS n FROM unnest($3::bigint[], $4::text[]) AS c(version, head) \
         JOIN ref_events e ON e.graph_id = $1 AND e.branch = $2 \
           AND e.new_version = c.version AND e.new_head = c.head",
    )
    .bind(graph)
    .bind(BRANCH)
    .bind(&versions)
    .bind(&heads)
    .fetch_one(pool)
    .await
    .expect("landing match query")
    .get("n");
    let ok = ref_version == ref_events
        && ref_events == distinct_event_versions
        && max_event_version == ref_version
        && accepted_decisions == ref_events
        && outbox_rows == ref_events
        && distinct_new_heads == ref_events
        && client_landed == ref_version
        && client_distinct_versions == client_landed
        && client_landings_matching_events == client_landed;
    GraphInvariants {
        graph: graph.to_owned(),
        ref_version,
        ref_events,
        distinct_event_versions,
        max_event_version,
        accepted_decisions,
        outbox_rows,
        distinct_new_heads,
        client_landed,
        client_distinct_versions,
        client_landings_matching_events,
        ok,
    }
}

async fn provision(pool: &PgPool, run: &str, count: usize) -> Vec<String> {
    let graphs = PgGraphs::new(pool.clone());
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let id = format!("stress-{run}-{i:04}");
        graphs
            .create(&NewGraph {
                graph_id: GraphId::new(id.clone()).expect("valid graph id"),
                tenant_id: TenantId::new(TENANT).expect("valid tenant id"),
                knowledge_base_id: None,
                purpose: Some("Plan 0005 stress qualification".into()),
                status: GraphStatus::Active,
            })
            .await
            .expect("graph provisioning under the owner identity");
        ids.push(id);
    }
    ids
}

// ---------------------------------------------------------------------------------------
// Phases and report
// ---------------------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct PhaseReport {
    name: String,
    writers: usize,
    graphs: usize,
    wall_seconds: f64,
    requests: u64,
    landed: u64,
    commits_per_second: f64,
    requests_per_second: f64,
    conflicts: u64,
    errors: BTreeMap<String, u64>,
    unexpected_errors: BTreeMap<String, u64>,
    malformed_responses: Vec<String>,
    latency: Vec<Percentiles>,
    /// Successful-operation p99 (ms) per op and the budget it was held to.
    ok_p99_ms: BTreeMap<&'static str, f64>,
    max_p99_ms: f64,
    pairs: PairStats,
    writers_gave_up: usize,
    writers_incomplete: usize,
    sessions: SessionSample,
    deadlocks_before: Option<i64>,
    deadlocks_after: Option<i64>,
    invariants: Vec<GraphInvariants>,
    /// Why the phase failed, if it did (empty on pass).
    failures: Vec<String>,
    invariants_ok: bool,
}

/// Shared context of one stress run.
struct Ctx<'a> {
    http: &'a reqwest::Client,
    pool: &'a PgPool,
    cfg: &'a Config,
}

/// One phase: `graphs` written by all writers (round-robin assignment) until every writer
/// landed `target` commits, or until `time_box` elapses.
struct Phase<'a> {
    name: &'a str,
    run: &'a str,
    graphs: &'a [String],
    target: u32,
    time_box: Option<Duration>,
}

async fn run_phase(ctx: &Ctx<'_>, phase: Phase<'_>) -> PhaseReport {
    let (pool, cfg, run, graphs, target, time_box) = (
        ctx.pool,
        ctx.cfg,
        phase.run,
        phase.graphs,
        phase.target,
        phase.time_box,
    );
    let name = phase.name;
    let metrics = Arc::new(Metrics::default());
    let api = Arc::new(Api {
        http: ctx.http.clone(),
        replicas: cfg.replicas.clone(),
        next: AtomicU64::new(0),
        metrics: metrics.clone(),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let deadlocks_before = deadlocks(pool).await;
    let sampler = tokio::spawn(sample_sessions(
        pool.clone(),
        cfg.runtime_role.clone(),
        stop.clone(),
    ));
    let started = Instant::now();
    let mut handles = Vec::with_capacity(cfg.writers);
    for index in 0..cfg.writers {
        let w = Writer {
            index,
            graph: graphs[index % graphs.len()].clone(),
            token: mint(cfg, &format!("stress-writer-{index}")),
            run: run.to_owned(),
        };
        handles.push(tokio::spawn(write_loop(
            api.clone(),
            w,
            target,
            stop.clone(),
        )));
    }
    if let Some(limit) = time_box {
        tokio::time::sleep(limit).await;
        stop.store(true, Ordering::Relaxed);
    }
    let mut outcomes = Vec::with_capacity(handles.len());
    for h in handles {
        outcomes.push(h.await.expect("writer task"));
    }
    let wall = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    let sessions = sampler.await.expect("sampler task");
    // Deadlock counters are flushed lazily by idle backends; read them after the interval.
    tokio::time::sleep(STATS_FLUSH_WAIT).await;
    let deadlocks_after = deadlocks(pool).await;

    let landed_rows = metrics.landed.lock().unwrap().clone();
    let mut invariants = Vec::with_capacity(graphs.len());
    for g in graphs {
        invariants.push(graph_invariants(pool, g, &landed_rows).await);
    }
    let latency: Vec<Percentiles> = {
        let mut map = metrics.latency.lock().unwrap();
        let mut out = Vec::new();
        for op in [Op::RefRead, Op::Prepare, Op::Accept] {
            for bucket in [
                Bucket::Ok,
                Bucket::Conflict,
                Bucket::Refused,
                Bucket::Failed,
            ] {
                if let Some(samples) = map.get_mut(&(op.name(), bucket.name())) {
                    out.push(percentiles(op.name(), bucket.name(), samples));
                }
            }
        }
        out
    };
    let ok_p99_ms: BTreeMap<&'static str, f64> = latency
        .iter()
        .filter(|p| p.bucket == "ok")
        .map(|p| (p.op, p.p99_ms))
        .collect();
    let landed = landed_rows.len() as u64;
    let requests = metrics.requests.load(Ordering::Relaxed);
    let errors = metrics.errors.lock().unwrap().clone();
    let unexpected_errors: BTreeMap<String, u64> = errors
        .iter()
        .filter(|(k, _)| !is_expected_failure(k) || is_timeout_class(k))
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    let malformed_responses = metrics.malformed.lock().unwrap().clone();
    let pairs = metrics.pairs.lock().unwrap().clone();

    let mut failures = Vec::new();
    for i in invariants.iter().filter(|i| !i.ok) {
        failures.push(format!(
            "graph {} violates the ref/event/decision/outbox/landing equalities",
            i.graph
        ));
    }
    if !pairs.disagreements.is_empty() {
        failures.push(format!(
            "{} duplicated-pair disagreement(s)",
            pairs.disagreements.len()
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
    match (deadlocks_before, deadlocks_after) {
        (Some(b), Some(a)) if a == b => {}
        (Some(b), Some(a)) => failures.push(format!("{} deadlock(s) during the phase", a - b)),
        _ => failures.push("deadlock counter could not be read".into()),
    }
    if landed < cfg.min_commits {
        failures.push(format!(
            "only {landed} commits landed (minimum {})",
            cfg.min_commits
        ));
    }
    if time_box.is_none() && outcomes.iter().any(|o| o.gave_up || o.landed < target) {
        failures.push("not every writer reached its commit target".into());
    }
    for (op, p99) in &ok_p99_ms {
        if *p99 > cfg.max_p99_ms {
            failures.push(format!(
                "{op} successful p99 {p99:.1} ms exceeds budget {:.0} ms",
                cfg.max_p99_ms
            ));
        }
    }
    let invariants_ok = failures.is_empty();
    PhaseReport {
        name: name.to_owned(),
        writers: cfg.writers,
        graphs: graphs.len(),
        wall_seconds: wall.as_secs_f64(),
        requests,
        landed,
        commits_per_second: landed as f64 / wall.as_secs_f64(),
        requests_per_second: requests as f64 / wall.as_secs_f64(),
        conflicts: metrics.conflicts.load(Ordering::Relaxed),
        errors,
        unexpected_errors,
        malformed_responses,
        latency,
        ok_p99_ms,
        max_p99_ms: cfg.max_p99_ms,
        pairs,
        writers_gave_up: outcomes.iter().filter(|o| o.gave_up).count(),
        writers_incomplete: outcomes.iter().filter(|o| o.landed < target).count(),
        sessions,
        deadlocks_before,
        deadlocks_after,
        invariants,
        failures,
        invariants_ok,
    }
}

#[derive(Serialize)]
struct Environment {
    cpu_model: String,
    cpus: usize,
    mem_total_kib: u64,
    kernel: String,
    postgres_version: String,
    postgres_max_connections: String,
    postgres_shared_buffers: String,
    replicas: Vec<String>,
}

fn read_first(path: &str, key: &str) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_owned()))
        })
        .unwrap_or_else(|| "unknown".into())
}

async fn environment(pool: &PgPool, replicas: &[String]) -> Environment {
    let show = |name: &'static str| async move {
        sqlx::query(&format!("SHOW {name}"))
            .fetch_one(pool)
            .await
            .map(|r| r.get::<String, _>(0))
            .unwrap_or_else(|_| "unknown".into())
    };
    Environment {
        cpu_model: read_first("/proc/cpuinfo", "model name"),
        cpus: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        mem_total_kib: read_first("/proc/meminfo", "MemTotal")
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|_| "unknown".into()),
        postgres_version: sqlx::query("SELECT version()")
            .fetch_one(pool)
            .await
            .map(|r| r.get::<String, _>(0))
            .unwrap_or_else(|_| "unknown".into()),
        postgres_max_connections: show("max_connections").await,
        postgres_shared_buffers: show("shared_buffers").await,
        replicas: replicas.to_vec(),
    }
}

#[derive(Serialize)]
struct VerifyReport {
    clean: bool,
    violations: Vec<(String, i64, Vec<String>)>,
    counts: Vec<(String, i64)>,
}

#[derive(Serialize)]
struct Report {
    run: String,
    started_utc_epoch: u64,
    environment: Environment,
    phases: Vec<PhaseReport>,
    verify: VerifyReport,
    passed: bool,
}

fn markdown(r: &Report) -> String {
    let mut m = String::new();
    m.push_str(&format!(
        "# Stress run `{}` — {}\n\n",
        r.run,
        if r.passed { "PASS" } else { "FAIL" }
    ));
    m.push_str(&format!(
        "Hardware: {} × {}, {} GiB RAM, kernel {}. PostgreSQL: {} (`max_connections` {}, `shared_buffers` {}). Replicas: {}.\n\n",
        r.environment.cpus,
        r.environment.cpu_model,
        r.environment.mem_total_kib / 1024 / 1024,
        r.environment.kernel,
        r.environment.postgres_version,
        r.environment.postgres_max_connections,
        r.environment.postgres_shared_buffers,
        r.environment.replicas.join(", ")
    ));
    for p in &r.phases {
        m.push_str(&format!(
            "## {} — {}\n\n{} writers over {} graph(s), {:.1} s wall, {} requests ({:.0} req/s), {} commits landed ({:.1} commits/s), {} conflicts (HEAD_CHANGED/LINEAGE_MISMATCH), writers incomplete {}, gave up {}.\n\n",
            p.name,
            if p.invariants_ok { "PASS" } else { "FAIL" },
            p.writers,
            p.graphs,
            p.wall_seconds,
            p.requests,
            p.requests_per_second,
            p.landed,
            p.commits_per_second,
            p.conflicts,
            p.writers_incomplete,
            p.writers_gave_up
        ));
        m.push_str("Latency per operation and outcome (successful operations are the ones held to the p99 budget; `refused` = `503 RESOURCE_LIMIT` admission control):\n\n| op | outcome | count | p50 ms | p95 ms | p99 ms | max ms | mean ms |\n|---|---|---:|---:|---:|---:|---:|---:|\n");
        for l in &p.latency {
            m.push_str(&format!(
                "| {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |\n",
                l.op, l.bucket, l.count, l.p50_ms, l.p95_ms, l.p99_ms, l.max_ms, l.mean_ms
            ));
        }
        m.push_str(&format!(
            "\nSuccessful p99 budget {:.0} ms: {}.\n\nError classes: {}. Unexpected classes (fail the run): {}.\n\nDuplicated requests (same key, two replicas, concurrent): {} pairs compared with both answers (exactly one original execution each, identical durable fields), {} pairs both conflicting identically, {} pairs with one side refused by admission control or lost, {} pairs with neither side answering; **{} disagreement(s)**.\n\nRuntime-role sessions (sampled every 250 ms, {} samples): max total {}, max active {}, mean active {:.1}, max waiting on locks {}. Deadlocks during phase: {}.\n\n",
            p.max_p99_ms,
            p.ok_p99_ms
                .iter()
                .map(|(op, v)| format!("{op} {v:.1} ms"))
                .collect::<Vec<_>>()
                .join(", "),
            if p.errors.is_empty() {
                "none".to_owned()
            } else {
                p.errors
                    .iter()
                    .map(|(k, v)| format!("`{k}` ×{v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            if p.unexpected_errors.is_empty() {
                "none".to_owned()
            } else {
                format!("{:?}", p.unexpected_errors)
            },
            p.pairs.compared,
            p.pairs.both_conflict,
            p.pairs.one_side_refused_or_lost,
            p.pairs.both_failed,
            p.pairs.disagreements.len(),
            p.sessions.samples,
            p.sessions.max_total,
            p.sessions.max_active,
            p.sessions.mean_active,
            p.sessions.max_lock_waiting,
            match (p.deadlocks_before, p.deadlocks_after) {
                (Some(b), Some(a)) => (a - b).to_string(),
                _ => "unreadable".to_owned(),
            }
        ));
        let bad: Vec<&GraphInvariants> = p.invariants.iter().filter(|i| !i.ok).collect();
        m.push_str(&format!(
            "Invariants over {} graph(s): Σ refs.version = {} = Σ ref_events = {} = Σ accepted decisions = {} = Σ outbox rows = {}; client-observed landings = {} with {} distinct (graph, version) pairs, {} of them found as ref events with the same version and head; {} graph(s) violate.\n\n",
            p.invariants.len(),
            p.invariants.iter().map(|i| i.ref_version).sum::<i64>(),
            p.invariants.iter().map(|i| i.ref_events).sum::<i64>(),
            p.invariants.iter().map(|i| i.accepted_decisions).sum::<i64>(),
            p.invariants.iter().map(|i| i.outbox_rows).sum::<i64>(),
            p.invariants.iter().map(|i| i.client_landed).sum::<i64>(),
            p.invariants
                .iter()
                .map(|i| i.client_distinct_versions)
                .sum::<i64>(),
            p.invariants
                .iter()
                .map(|i| i.client_landings_matching_events)
                .sum::<i64>(),
            bad.len()
        ));
        for f in &p.failures {
            m.push_str(&format!("- FAIL: {f}\n"));
        }
        for i in bad.iter().take(10) {
            m.push_str(&format!(
                "- `{}`: {}\n",
                i.graph,
                serde_json::to_string(i).unwrap()
            ));
        }
        for d in p.pairs.disagreements.iter().take(10) {
            m.push_str(&format!("- pair disagreement: {d}\n"));
        }
        for d in p.malformed_responses.iter().take(10) {
            m.push_str(&format!("- malformed response: {d}\n"));
        }
        if !p.failures.is_empty() {
            m.push('\n');
        }
    }
    m.push_str(&format!(
        "## Verifier\n\n`ledger_store::verify` over the whole database: {} ({} violating check(s)).\n",
        if r.verify.clean { "clean" } else { "VIOLATIONS" },
        r.verify.violations.len()
    ));
    for (name, n, sample) in &r.verify.violations {
        m.push_str(&format!("- {name}: {n} ({})\n", sample.join(", ")));
    }
    m
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut argv = std::env::args().skip(1).peekable();
    if argv.peek().map(String::as_str) == Some("fault") {
        argv.next();
        return fault::run(argv).await;
    }
    if argv.peek().map(String::as_str) == Some("branches") {
        argv.next();
        return branches::run(argv).await;
    }
    if argv.peek().map(String::as_str) == Some("bench") {
        argv.next();
        return bench::run(argv).await;
    }
    let cfg = match parse(argv) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    std::fs::create_dir_all(&cfg.out).expect("output directory");
    let run = format!("{:x}", now_secs());
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
    let ctx = Ctx {
        http: &http,
        pool: &pool,
        cfg: &cfg,
    };
    let environment = environment(&pool, &cfg.replicas).await;
    eprintln!(
        "run {run}: {} writers, {} replicas, contended {} s on one graph, then {} commits/writer over {} graphs",
        cfg.writers,
        cfg.replicas.len(),
        cfg.contended_seconds,
        cfg.commits_per_writer,
        cfg.graphs
    );

    let contended_graph = provision(&pool, &run, 1).await;
    let contended = run_phase(
        &ctx,
        Phase {
            name: "contended (one graph, time-boxed)",
            run: &format!("{run}c"),
            graphs: &contended_graph,
            target: u32::MAX,
            time_box: Some(Duration::from_secs(cfg.contended_seconds)),
        },
    )
    .await;
    eprintln!(
        "contended: {} commits, {} requests, {} conflicts, {}",
        contended.landed,
        contended.requests,
        contended.conflicts,
        if contended.invariants_ok {
            "PASS"
        } else {
            "FAIL"
        }
    );

    let many = provision(&pool, &format!("{run}i"), cfg.graphs).await;
    let independent = run_phase(
        &ctx,
        Phase {
            name: "independent (many graphs, fixed commits per writer)",
            run: &format!("{run}i"),
            graphs: &many,
            target: cfg.commits_per_writer,
            time_box: None,
        },
    )
    .await;
    eprintln!(
        "independent: {} commits, {} requests, {} conflicts, {}",
        independent.landed,
        independent.requests,
        independent.conflicts,
        if independent.invariants_ok {
            "PASS"
        } else {
            "FAIL"
        }
    );

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
    let passed = contended.invariants_ok && independent.invariants_ok && verify.clean;
    let report = Report {
        run: run.clone(),
        started_utc_epoch: now_secs(),
        environment,
        phases: vec![contended, independent],
        verify,
        passed,
    };
    std::fs::write(
        format!("{}/report.json", cfg.out),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .expect("write report.json");
    let md = markdown(&report);
    std::fs::write(format!("{}/report.md", cfg.out), &md).expect("write report.md");
    println!("{md}");
    if passed {
        println!("STRESS OK");
        ExitCode::SUCCESS
    } else {
        println!("STRESS FAILED");
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::{Outcome, PairVerdict, classify_pair, jitter, percentiles, require_loopback};
    use serde_json::json;

    #[test]
    fn percentiles_are_nearest_rank_and_sort_their_input() {
        // 1..=100 ms, deliberately unsorted (descending): p50 = 50th smallest = 50 ms,
        // p95 = 95 ms, p99 = 99 ms, max = 100 ms.
        let mut s: Vec<u64> = (1..=100).rev().map(|i| i * 1000).collect();
        let p = percentiles("x", "ok", &mut s);
        assert_eq!(p.count, 100);
        assert_eq!(
            (p.p50_ms, p.p95_ms, p.p99_ms, p.max_ms),
            (50.0, 95.0, 99.0, 100.0)
        );
        // Small sample: nearest rank of p99 over 3 samples is the 3rd (⌈2.97⌉).
        let mut three = vec![3000, 1000, 2000];
        let p = percentiles("x", "ok", &mut three);
        assert_eq!((p.p50_ms, p.p99_ms), (2.0, 3.0));
        let mut empty: Vec<u64> = Vec::new();
        assert_eq!(percentiles("e", "ok", &mut empty).count, 0);
    }

    #[test]
    fn jitter_is_bounded_and_deterministic() {
        for i in 0..100 {
            for s in 0..100 {
                let j = jitter(i, s, 25);
                assert!(j < 25);
                assert_eq!(j, jitter(i, s, 25));
            }
        }
        assert_eq!(jitter(3, 4, 0), 0);
    }

    fn ok(replayed: bool, head: &str) -> Outcome {
        Outcome::Ok(json!({
            "head": head, "ref_version": 7, "replayed": replayed, "correlation_id": head.len()
        }))
    }

    #[test]
    fn duplicated_pairs_are_classified_conservatively() {
        assert_eq!(
            classify_pair(&ok(false, "h"), &ok(true, "h")),
            PairVerdict::Agree
        );
        // Two originals = duplicate execution; different bodies = divergent replicas.
        assert!(matches!(
            classify_pair(&ok(false, "h"), &ok(false, "h")),
            PairVerdict::Disagree(_)
        ));
        assert!(matches!(
            classify_pair(&ok(true, "h"), &ok(true, "h")),
            PairVerdict::Disagree(_)
        ));
        assert!(matches!(
            classify_pair(&ok(false, "h1"), &ok(true, "h2")),
            PairVerdict::Disagree(_)
        ));
        // One side refused by admission control or lost: acceptable, counted separately.
        for class in [
            "503 RESOURCE_LIMIT",
            "transport other",
            "503 DEPENDENCY_UNAVAILABLE",
        ] {
            assert_eq!(
                classify_pair(&ok(false, "h"), &Outcome::Failed(class.into())),
                PairVerdict::OneSideRefusedOrLost,
                "{class}"
            );
        }
        // One side ok, other a semantic failure or conflict: a disagreement.
        for class in [
            "409 IDEMPOTENCY_CONFLICT",
            "500 INTERNAL",
            "401 UNAUTHENTICATED",
            "503 DEPENDENCY_TIMEOUT",
        ] {
            assert!(
                matches!(
                    classify_pair(&Outcome::Failed(class.into()), &ok(false, "h")),
                    PairVerdict::Disagree(_)
                ),
                "{class}"
            );
        }
        assert!(matches!(
            classify_pair(&ok(false, "h"), &Outcome::Conflict("HEAD_CHANGED".into())),
            PairVerdict::Disagree(_)
        ));
        assert_eq!(
            classify_pair(
                &Outcome::Conflict("HEAD_CHANGED".into()),
                &Outcome::Conflict("HEAD_CHANGED".into())
            ),
            PairVerdict::BothConflict
        );
        assert!(matches!(
            classify_pair(
                &Outcome::Conflict("HEAD_CHANGED".into()),
                &Outcome::Conflict("LINEAGE_MISMATCH".into())
            ),
            PairVerdict::Disagree(_)
        ));
        assert_eq!(
            classify_pair(
                &Outcome::Failed("503 RESOURCE_LIMIT".into()),
                &Outcome::Failed("transport connect".into())
            ),
            PairVerdict::BothFailed
        );
        assert!(matches!(
            classify_pair(
                &Outcome::Failed("503 RESOURCE_LIMIT".into()),
                &Outcome::Failed("500 INTERNAL".into())
            ),
            PairVerdict::Disagree(_)
        ));
    }

    #[test]
    fn non_loopback_targets_are_refused_unless_overridden() {
        for ok in [
            "http://127.0.0.1:8080",
            "http://localhost:8081",
            "http://[::1]:8080",
            "postgres://ledger:pw@localhost:55432/ledger?sslmode=disable",
            "postgres://ledger:pw@127.0.0.1/ledger",
        ] {
            assert!(require_loopback("t", ok, false).is_ok(), "{ok}");
        }
        for bad in [
            "http://10.0.0.5:8080",
            "http://ledger.internal:8080",
            "http://user:secret@127.0.0.1:8080",
            "postgres://ledger:pw@db.prod.example/ledger",
            "postgres://ledger:pw@localhost/ledger?host=db.prod.example",
            "postgres://ledger:pw@127.0.0.1/ledger?hostaddr=10.0.0.5",
        ] {
            let err = require_loopback("t", bad, false).unwrap_err();
            assert!(
                !err.contains("secret") && !err.contains("prod.example"),
                "{bad}: {err}"
            );
            assert!(require_loopback("t", bad, true).is_ok());
        }
        assert!(require_loopback("t", "not a url", false).is_err());
    }
}
