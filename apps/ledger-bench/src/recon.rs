//! Reconstruction characterization (`ledger-bench recon`, Plan 0011): a diagnostic profile
//! outside PR CI that varies **ancestry depth independently of state size**. It separates
//! the parts of a reconstruction by measuring the same work through different paths.
//!
//! **Build.** For each state size S (default 1, 1,000 and 10,000 quads), a linear history is
//! built through the public API (prepare + accept) on its own graph: a bulk genesis of S
//! quads, then commits that each replace one quad, so the state stays at S quads while the
//! history deepens. The sizes are built concurrently; nothing is measured while building.
//!
//! **Measure.** Afterwards, serially on a quiet stack, for every (S, depth) point:
//! - `api` / `state_read`: `GET …/commits/{c}/state` of the commit at that depth. This
//!   includes HTTP, server JSON encoding, transfer and client JSON decoding.
//! - `persisted` / `store_reconstruct`: the production fold
//!   (`WorkflowRepository::reconstruct`) on an owner pool in this process: the same fold
//!   without HTTP, but as the owner role (the API runs as the runtime role, whose
//!   row-security predicates and grants may make PostgreSQL do slightly different work).
//! - `algorithm` / `fold_cpu`: the fold's CPU work (SHA-256 re-hash, envelope and patch
//!   decoding, set application) re-executed on objects prefetched in one query. It estimates
//!   the non-I/O share; it is not the production code path.
//! - `api` / `prepare`: prepare (not accepted) on a branch created at that commit, which
//!   reconstructs the parent state.
//! - `api` / `merge_preview_contained`: a preview whose source is already contained. It
//!   returns after the ancestry walk of both sides, with no reconstruction, which isolates
//!   merge ancestry traversal.
//! - `api` / `merge_preview_divergent`: a preview of two branches forked at that commit:
//!   three reconstructions at about that depth plus the ancestry walk.
//!
//! **Correctness.** Every timed result is verified exactly, outside the timed region, against
//! expectations the harness computes with its own set algebra:
//! - every API state reply, store reconstruction, prefetched fold and post-restart read must
//!   have the oracle digest (sorted canonical lines, SHA-256) of that commit's state, not
//!   merely its cardinality;
//! - every prepared candidate's state (read back afterwards) is the base state plus its probe;
//! - every merge preview reply has the expected classification; a divergent preview's
//!   `merged_state_digest` equals the protocol digest of base + both probes (computed with the
//!   labelled `ledger_rdf::state_digest` over the harness's set).
//!
//! **Measurement window** around each measured batch, in this order:
//! 1. reset `pg_stat_statements`, read the baseline statement totals;
//! 2. read the baseline cgroup CPU counters (after the statistics queries, so their CPU is
//!    outside the window);
//! 3. the measured operations, and nothing else;
//! 4. read the ending cgroup CPU counters and memory immediately;
//! 5. read the ending statement totals (after the CPU reading, so the statistics query's CPU
//!    is outside the window); derive per-operation deltas.
//!
//! What each figure contains:
//! - `pg.*`: `pg_stat_statements` deltas for the role that ran the batch (runtime role for
//!   API batches, owner for store batches), statistics queries excluded by text: calls,
//!   rows, shared block hits and reads, temporary blocks, block read time and execution
//!   time. Block figures are 8 KiB buffer counts, **not physical I/O bytes**: a "read" may be
//!   served by the OS page cache.
//! - `cpu.server_cpu_ms` / `cpu.postgres_cpu_ms`: cgroup v2 `cpu.stat usage_usec` deltas of
//!   the whole container: every process in it, including PostgreSQL background workers
//!   (checkpointer, WAL writer, autovacuum) that happen to run during the window.
//! - `cpu.*_memory_bytes`: `memory.current` right after the batch (page cache included).
//!
//! **Cache conditions.**
//! - `warm`: after warm-up repetitions on an active database.
//! - `db-restart-first-ledger-op` (see [`FIRST_AFTER_RESTART`]): PostgreSQL was restarted
//!   (`--restart-cmd`: process and shared buffers restarted); then, before the measured
//!   operation, the server's `/ready` probe, the reconnecting pool and the window's
//!   statistics queries ran. The measured reconstruction is the first *ledger
//!   reconstruction* after the restart, not the first PostgreSQL operation, so shared buffers
//!   are not untouched (the catalog and statistics pages are already loaded).
//!
//! An OS-page-cache-cold condition is never claimed: the host cache is not dropped.
//!
//! **Before measuring**, the database is settled after the concurrent write build
//! (`VACUUM (ANALYZE)`, then `CHECKPOINT`). The instrumented override slows the containers'
//! healthchecks to once a day after start-up, so no probe statement lands in a window.
//!
//! **What the figures cannot support** (Plan 0011 review):
//! - `persisted` / `store_reconstruct` runs in this process on the host and reaches
//!   PostgreSQL through the published port (Docker's port mapping, possibly its userland
//!   proxy), while the server reaches it over the container network. API minus store is
//!   therefore *not* the cost of HTTP and JSON; the benchmark process's own CPU is not
//!   measured.
//! - `algorithm` / `fold_cpu` hashes each object once on cache-hot memory, while production
//!   hashes commits and patches twice and checks limits per patch. It is a lower bound on the
//!   fold's compute, not that cost itself.
//! - Container CPU is whole-container and includes work both sides do around each round
//!   trip. Server plus PostgreSQL CPU can exceed latency, so it cannot be split into a
//!   per-part latency breakdown.
//! - PostgreSQL CPU is measured with `pg_stat_statements.track=all` and `track_io_timing`,
//!   which production does not run; absolute per-statement CPU does not transfer.
//! - Absolute per-round-trip cost is specific to this host and container topology. The
//!   scaling shape (round trips per ancestor, linear growth) transfers; absolute latency
//!   does not.

use crate::{result::stats, workload::oracle_digest};
use ledger_core::{AnyCommit, CommitId, ContentId, GraphId, TenantId};
use ledger_rdf::{OperationKind, Patch, Quad};
use ledger_store::{
    GraphStatus, NewGraph, PgGraphs, PostgresLedgerStore, ReconstructionLimits, V1Binding,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

pub const RECON_SCHEMA: &str = "sculpin-ledger-bench-recon/v1";
const TENANT: &str = "tenant-bench";
const RUNTIME_ROLE: &str = "ledger_runtime";
const OWNER_ROLE: &str = "ledger";
const STATS_SCHEMA: &str = "bench_stats";
const BULK: usize = 5_000;
/// Cache label of the measurements after a PostgreSQL restart (module docs).
pub const FIRST_AFTER_RESTART: &str = "db-restart-first-ledger-op";

pub struct ReconConfig {
    pub replica: String,
    pub owner_database_url: String,
    pub secret: String,
    pub issuer: String,
    pub audience: String,
    pub states: Vec<usize>,
    pub depths: Vec<usize>,
    pub reps: usize,
    pub warmup: usize,
    pub preview_reps: usize,
    pub cold_depths: Vec<usize>,
    pub cold_reps: usize,
    pub restart_cmd: Option<String>,
    pub server_cgroup: Option<PathBuf>,
    pub postgres_cgroup: Option<PathBuf>,
    /// The server's reconstruction depth limit (`LEDGER_LIMIT_RECONSTRUCTION_DEPTH`).
    pub depth_limit: usize,
    pub run_id: String,
}

impl ReconConfig {
    /// Refuse a configuration that would panic, silently measure nothing, or ask for depths
    /// the stack cannot reconstruct.
    pub fn validate(&self) -> Result<(), String> {
        if self.states.is_empty() || self.depths.is_empty() {
            return Err("--states and --depths must not be empty".into());
        }
        if self.states.contains(&0) {
            return Err(
                "--states must not contain 0 (a constant-state history needs ≥ 1 quad)".into(),
            );
        }
        let max_quads = ReconstructionLimits::DEVELOPMENT.max_quads;
        if let Some(s) = self.states.iter().find(|s| **s > max_quads) {
            return Err(format!(
                "state size {s} exceeds the reconstruction limit of {max_quads} quads"
            ));
        }
        if self.reps == 0 || self.preview_reps == 0 {
            return Err("--reps and --preview-reps must be at least 1".into());
        }
        // The base state is built in chunks of `BULK` quads, and every chunk is one ancestry
        // level: depth d holds the complete state only once (d + 1) * BULK >= S. A smaller
        // depth would be measured against a partial state and mislabelled with S.
        let min_depth = self.depths.iter().copied().min().unwrap_or(0);
        if let Some(s) = self.states.iter().find(|s| **s > (min_depth + 1) * BULK) {
            return Err(format!(
                "state size {s} is complete only from depth {} ({} commits of {BULK} quads), \
                 but depth {min_depth} was requested",
                s.div_ceil(BULK) - 1,
                s.div_ceil(BULK)
            ));
        }
        // The divergent merge preview reconstructs one commit beyond the measured depth, and
        // the store path runs under ReconstructionLimits::DEVELOPMENT.
        let limit = self
            .depth_limit
            .min(ReconstructionLimits::DEVELOPMENT.max_depth);
        if let Some(d) = self.depths.iter().find(|d| **d + 1 > limit) {
            return Err(format!(
                "depth {d} cannot be measured: depth + 1 must be within the reconstruction depth limit {limit}"
            ));
        }
        if !self.cold_depths.is_empty() {
            if self.cold_reps == 0 {
                return Err("--cold-reps must be at least 1 when --cold-depths is given".into());
            }
            if self.restart_cmd.is_none() {
                return Err(
                    "--cold-depths needs --restart-cmd (or pass an empty --cold-depths)".into(),
                );
            }
            if let Some(d) = self.cold_depths.iter().find(|d| !self.depths.contains(d)) {
                return Err(format!("cold depth {d} is not one of --depths"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PgDelta {
    /// Per operation (the batch delta divided by the batch's operation count).
    pub calls: f64,
    pub rows: f64,
    pub shared_blks_hit: f64,
    pub shared_blks_read: f64,
    pub temp_blks: f64,
    pub blk_read_time_ms: f64,
    pub exec_time_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CpuDelta {
    /// cgroup `cpu.stat` usage per operation.
    pub server_cpu_ms: Option<f64>,
    pub postgres_cpu_ms: Option<f64>,
    /// `memory.current` after the batch (page cache included).
    pub server_memory_bytes: Option<u64>,
    pub postgres_memory_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReconPoint {
    pub state_quads: usize,
    pub depth: usize,
    /// Patch operations folded to reconstruct the commit at this depth.
    pub fold_ops: u64,
    /// Bytes of the state as canonical N-Quads lines (line plus newline).
    pub canonical_state_bytes: u64,
    pub category: String,
    pub op: String,
    /// `warm` or [`FIRST_AFTER_RESTART`].
    pub cache: String,
    pub n: usize,
    pub p50_ms: f64,
    /// Only with at least 20 samples (nearest rank: with n = 20 it is the second-largest).
    pub p95_ms: Option<f64>,
    /// Only with at least 100 samples (below that the nearest-rank p99 is the maximum).
    pub p99_ms: Option<f64>,
    pub mean_ms: f64,
    pub max_ms: f64,
    pub response_bytes: Option<usize>,
    pub pg: Option<PgDelta>,
    pub cpu: Option<CpuDelta>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReconResult {
    pub schema: String,
    pub status: String,
    pub meta: BTreeMap<String, String>,
    pub environment: BTreeMap<String, String>,
    pub build_ms: BTreeMap<String, u128>,
    pub correctness: BTreeMap<String, u64>,
    pub failures: Vec<String>,
    pub points: Vec<ReconPoint>,
}

fn quad(entity: usize, value: u64) -> String {
    format!("<urn:recon:e{entity}> <urn:recon:p> \"v{value}\" .")
}

/// One constant-state history: commit ids by index (index = parent-0 depth) and the
/// expected state at every index (only its fingerprint and size are kept).
struct History {
    graph: String,
    states: usize,
    ids: Vec<CommitId>,
    /// (quads, canonical bytes, oracle digest, fold ops) per index.
    expected: Vec<(usize, u64, String, u64)>,
    /// The full expected state at every measured depth (harness set algebra).
    snapshots: BTreeMap<usize, BTreeSet<String>>,
}

impl History {
    fn state_at(&self, depth: usize) -> BTreeSet<String> {
        self.snapshots[&depth].clone()
    }
}

/// The quad a prepare probe adds at `depth` (repetition `i`).
fn probe_quad(depth: usize, i: usize) -> String {
    format!("<urn:recon:prep> <urn:recon:p> \"d{depth}-{i}\" .")
}

/// Whether a state reply holds exactly the expected state (count and oracle digest).
fn reply_matches(v: &Value, quads: usize, digest: &str) -> bool {
    let lines: Vec<&str> = v["quads"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    lines.len() == quads && oracle_digest(lines) == digest
}

/// `sculpin-rdf-state/v1` digest of an expected state: the ledger's labelled digest function
/// over the harness's own set (used only to compare a preview's `merged_state_digest`).
fn protocol_digest(state: &BTreeSet<String>) -> String {
    let quads: BTreeSet<Quad> = state
        .iter()
        .map(|q| q.parse::<Quad>().expect("harness quads are canonical"))
        .collect();
    ledger_rdf::state_digest(&quads).to_string()
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
    keys: std::sync::atomic::AtomicU64,
    run_id: String,
}

impl Client {
    fn new(cfg: &ReconConfig) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let claims = json!({
            "iss": cfg.issuer, "aud": cfg.audience, "exp": now + 12 * 3600, "nbf": now - 30,
            "tid": TENANT, "oid": "ledger-bench-recon", "sculpin_principal_type": "agent",
            "roles": ["ledger.read", "ledger.propose", "ledger.review"],
        });
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(cfg.secret.as_bytes()),
        )
        .expect("HS256");
        Self {
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("http client"),
            base: cfg.replica.clone(),
            token,
            keys: Default::default(),
            run_id: cfg.run_id.clone(),
        }
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        idempotent: bool,
        body: Option<&Value>,
    ) -> Result<(u16, Value, Duration, usize), String> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token));
        if idempotent {
            let n = self.keys.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            request = request.header("idempotency-key", format!("recon-{}-{n}", self.run_id));
        }
        if let Some(b) = body {
            request = request.json(b);
        }
        let started = Instant::now();
        let response = request.send().await.map_err(|e| format!("{path}: {e}"))?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(|e| format!("{path}: {e}"))?;
        // Client decoding is part of the API timing (module docs), as in the `run` profiles.
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let elapsed = started.elapsed();
        Ok((status, value, elapsed, bytes.len()))
    }

    async fn expect(
        &self,
        method: reqwest::Method,
        path: &str,
        idempotent: bool,
        body: Option<&Value>,
        want: u16,
    ) -> Result<(Value, Duration, usize), String> {
        let (status, v, t, n) = self.call(method, path, idempotent, body).await?;
        if status != want {
            return Err(format!("{path}: HTTP {status} {v}"));
        }
        Ok((v, t, n))
    }

    /// prepare + accept; the new commit id.
    async fn commit(
        &self,
        graph: &str,
        branch: &str,
        head: Option<&CommitId>,
        deletes: &[String],
        adds: &[String],
    ) -> Result<CommitId, String> {
        let operations: Vec<Value> = deletes
            .iter()
            .map(|q| json!({"op": "delete", "quad": q}))
            .chain(adds.iter().map(|q| json!({"op": "add", "quad": q})))
            .collect();
        let head = head.map(ToString::to_string);
        let body = json!({"ref": branch, "expected_head": head, "operations": operations,
            "activity": "benchmark-recon", "message": "recon", "evidence_refs": []});
        let (v, _, _) = self
            .expect(
                reqwest::Method::POST,
                &format!("/v1/graphs/{graph}/proposals"),
                true,
                Some(&body),
                201,
            )
            .await?;
        let candidate = v["candidate"].as_str().ok_or("no candidate")?.to_owned();
        let body = json!({"ref": branch, "expected_head": head, "reason": "recon"});
        self.expect(
            reqwest::Method::POST,
            &format!("/v1/graphs/{graph}/proposals/{candidate}/accept"),
            true,
            Some(&body),
            200,
        )
        .await?;
        CommitId::from_str(&candidate).map_err(|e| e.to_string())
    }
}

async fn build(
    cfg: &ReconConfig,
    client: &Client,
    owner: &PgPool,
    states: usize,
) -> Result<History, String> {
    let max_depth = cfg.depths.iter().copied().max().unwrap_or(1);
    let graph = format!("recon-s{states}-{}", cfg.run_id);
    PgGraphs::new(owner.clone())
        .create(&NewGraph {
            graph_id: GraphId::new(graph.clone()).map_err(|e| e.to_string())?,
            tenant_id: TenantId::new(TENANT).map_err(|e| e.to_string())?,
            knowledge_base_id: None,
            purpose: Some("ledger-bench recon".into()),
            status: GraphStatus::Active,
        })
        .await
        .map_err(|e| e.to_string())?;
    let mut values: Vec<u64> = (0..states as u64).collect();
    let mut next_value = states as u64;
    let mut current: BTreeSet<String> = BTreeSet::new();
    let mut ids = Vec::new();
    let mut expected = Vec::new();
    let mut snapshots = BTreeMap::new();
    let mut fold_ops = 0u64;
    let all: Vec<String> = (0..states).map(|e| quad(e, values[e])).collect();
    for chunk in all.chunks(BULK) {
        let id = client
            .commit(&graph, "main", ids.last(), &[], chunk)
            .await?;
        current.extend(chunk.iter().cloned());
        fold_ops += chunk.len() as u64;
        ids.push(id);
        expected.push(fingerprint(&current, fold_ops));
        if cfg.depths.contains(&(ids.len() - 1)) {
            snapshots.insert(ids.len() - 1, current.clone());
        }
    }
    while ids.len() <= max_depth {
        let e = ids.len() % states;
        let old = quad(e, values[e]);
        values[e] = next_value;
        next_value += 1;
        let new = quad(e, values[e]);
        let id = client
            .commit(
                &graph,
                "main",
                ids.last(),
                std::slice::from_ref(&old),
                std::slice::from_ref(&new),
            )
            .await?;
        current.remove(&old);
        current.insert(new);
        fold_ops += 2;
        ids.push(id);
        expected.push(fingerprint(&current, fold_ops));
        if cfg.depths.contains(&(ids.len() - 1)) {
            snapshots.insert(ids.len() - 1, current.clone());
        }
    }
    Ok(History {
        graph,
        states,
        ids,
        expected,
        snapshots,
    })
}

fn fingerprint(state: &BTreeSet<String>, fold_ops: u64) -> (usize, u64, String, u64) {
    let bytes: u64 = state.iter().map(|q| q.len() as u64 + 1).sum();
    (
        state.len(),
        bytes,
        oracle_digest(state.iter().map(String::as_str)),
        fold_ops,
    )
}

/// The block-read-time column of this PostgreSQL version (`shared_blk_read_time` from 17).
async fn read_time_column(owner: &PgPool) -> Option<&'static str> {
    let modern = sqlx::query_scalar::<_, bool>(&format!(
        "SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = '{STATS_SCHEMA}.pg_stat_statements'::regclass \
         AND attname = 'shared_blk_read_time')"
    ))
    .fetch_one(owner)
    .await
    .ok()?;
    Some(if modern {
        "shared_blk_read_time"
    } else {
        "blk_read_time"
    })
}

/// `pg_stat_statements` totals for one role in the current database (statistics queries,
/// which mention `pg_stat_statements`, excluded).
async fn pg_totals(owner: &PgPool, read_time: Option<&str>, role: &str) -> Option<[f64; 7]> {
    let read_time = read_time?;
    let row = sqlx::query(&format!(
        "SELECT COALESCE(sum(calls),0)::float8 c, COALESCE(sum(rows),0)::float8 r, \
         COALESCE(sum(shared_blks_hit),0)::float8 h, COALESCE(sum(shared_blks_read),0)::float8 rd, \
         COALESCE(sum(temp_blks_read + temp_blks_written),0)::float8 t, \
         COALESCE(sum({read_time}),0)::float8 bt, COALESCE(sum(total_exec_time),0)::float8 e \
         FROM {STATS_SCHEMA}.pg_stat_statements s JOIN pg_roles ro ON ro.oid = s.userid \
         WHERE ro.rolname = $1 AND s.dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
         AND s.query NOT ILIKE '%pg_stat_statements%'"
    ))
    .bind(role)
    .fetch_one(owner)
    .await
    .ok()?;
    let g = |c: &str| row.try_get::<f64, _>(c).unwrap_or(0.0);
    Some([g("c"), g("r"), g("h"), g("rd"), g("t"), g("bt"), g("e")])
}

fn cgroup_cpu_usec(dir: &Option<PathBuf>) -> Option<u64> {
    let text = std::fs::read_to_string(dir.as_ref()?.join("cpu.stat")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

fn cgroup_memory(dir: &Option<PathBuf>) -> Option<u64> {
    std::fs::read_to_string(dir.as_ref()?.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Container CPU counters and memory at one instant.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CpuReading {
    server_usec: Option<u64>,
    postgres_usec: Option<u64>,
    server_memory: Option<u64>,
    postgres_memory: Option<u64>,
}

fn read_cpu(cfg: &ReconConfig) -> CpuReading {
    CpuReading {
        server_usec: cgroup_cpu_usec(&cfg.server_cgroup),
        postgres_usec: cgroup_cpu_usec(&cfg.postgres_cgroup),
        server_memory: cgroup_memory(&cfg.server_cgroup),
        postgres_memory: cgroup_memory(&cfg.postgres_cgroup),
    }
}

struct Window {
    pg: Option<[f64; 7]>,
    cpu: CpuReading,
}

/// Opens a window (module docs, steps 1–2): statistics first, CPU baseline last.
async fn open_window(
    cfg: &ReconConfig,
    owner: &PgPool,
    read_time: Option<&str>,
    role: &str,
) -> Window {
    open_with(
        || async {
            // Reset so the batch alone is attributed; the reset and the totals query are
            // excluded by the totals query's filter.
            let _ = sqlx::query(&format!("SELECT {STATS_SCHEMA}.pg_stat_statements_reset()"))
                .execute(owner)
                .await;
            pg_totals(owner, read_time, role).await
        },
        || read_cpu(cfg),
    )
    .await
}

/// Closes a window (steps 4–5): CPU immediately, statistics afterwards.
async fn close_window(
    cfg: &ReconConfig,
    owner: &PgPool,
    read_time: Option<&str>,
    role: &str,
    w: Window,
    ops: usize,
) -> (Option<PgDelta>, Option<CpuDelta>) {
    close_with(
        w,
        ops,
        || read_cpu(cfg),
        || pg_totals(owner, read_time, role),
    )
    .await
}

/// The ordering of [`open_window`], separated from the sources so it can be tested.
async fn open_with<P, F>(pg: P, cpu: impl FnOnce() -> CpuReading) -> Window
where
    P: FnOnce() -> F,
    F: std::future::Future<Output = Option<[f64; 7]>>,
{
    let pg = pg().await;
    Window { pg, cpu: cpu() }
}

/// The ordering of [`close_window`], separated from the sources so it can be tested.
async fn close_with<P, F>(
    w: Window,
    ops: usize,
    cpu: impl FnOnce() -> CpuReading,
    pg: P,
) -> (Option<PgDelta>, Option<CpuDelta>)
where
    P: FnOnce() -> F,
    F: std::future::Future<Output = Option<[f64; 7]>>,
{
    let end_cpu = cpu();
    let end_pg = pg().await;
    derive(&w, end_pg, end_cpu, ops)
}

/// Per-operation deltas of one window.
fn derive(
    w: &Window,
    end_pg: Option<[f64; 7]>,
    end: CpuReading,
    ops: usize,
) -> (Option<PgDelta>, Option<CpuDelta>) {
    let n = ops.max(1) as f64;
    let pg = match (w.pg, end_pg) {
        (Some(a), Some(b)) => Some(PgDelta {
            calls: (b[0] - a[0]) / n,
            rows: (b[1] - a[1]) / n,
            shared_blks_hit: (b[2] - a[2]) / n,
            shared_blks_read: (b[3] - a[3]) / n,
            temp_blks: (b[4] - a[4]) / n,
            blk_read_time_ms: (b[5] - a[5]) / n,
            exec_time_ms: (b[6] - a[6]) / n,
        }),
        _ => None,
    };
    let delta = |before: Option<u64>, now: Option<u64>| match (before, now) {
        (Some(a), Some(b)) => Some((b.saturating_sub(a)) as f64 / 1000.0 / n),
        _ => None,
    };
    let cpu = CpuDelta {
        server_cpu_ms: delta(w.cpu.server_usec, end.server_usec),
        postgres_cpu_ms: delta(w.cpu.postgres_usec, end.postgres_usec),
        server_memory_bytes: end.server_memory,
        postgres_memory_bytes: end.postgres_memory,
    };
    (pg, Some(cpu))
}

/// The oracle digest of a reconstructed state (sorted canonical lines).
fn state_digest(state: &BTreeSet<Quad>) -> String {
    let mut lines: Vec<&str> = state.iter().map(Quad::as_str).collect();
    lines.sort_unstable();
    oracle_digest(lines)
}

fn point(
    h: &History,
    depth: usize,
    (category, op, cache): (&str, &str, &str),
    micros: &mut [u64],
    response_bytes: Option<usize>,
    (pg, cpu): (Option<PgDelta>, Option<CpuDelta>),
) -> ReconPoint {
    let s = stats(category, op, cache, micros);
    let (quads, bytes, _, fold_ops) = h.expected[depth];
    let tail = s.count >= crate::result::MIN_TAIL_SAMPLES;
    ReconPoint {
        // The state actually at this depth (a bulk genesis holds fewer quads at depth 0).
        state_quads: quads,
        depth,
        fold_ops,
        canonical_state_bytes: bytes,
        category: category.into(),
        op: op.into(),
        cache: cache.into(),
        n: s.count,
        p50_ms: s.p50_ms,
        p95_ms: tail.then_some(s.p95_ms),
        // With fewer than 100 samples the nearest-rank p99 is the maximum: not reported.
        p99_ms: (s.count >= 100).then_some(s.p99_ms),
        mean_ms: s.mean_ms,
        max_ms: s.max_ms,
        response_bytes,
        pg,
        cpu,
    }
}

fn us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// The fold's CPU work on prefetched objects: hash, decode, apply.
fn fold_cpu(chain: &[(Vec<u8>, Vec<u8>)]) -> Result<BTreeSet<Quad>, String> {
    let mut state: BTreeSet<Quad> = BTreeSet::new();
    for (commit, patch) in chain {
        std::hint::black_box(ContentId::for_bytes(commit));
        AnyCommit::from_canonical_bytes(commit).map_err(|e| e.to_string())?;
        std::hint::black_box(ContentId::for_bytes(patch));
        let p = Patch::from_canonical_bytes(patch).map_err(|e| e.to_string())?;
        for op in p.operations() {
            match op.kind {
                OperationKind::Add => {
                    state.insert(op.quad.clone());
                }
                OperationKind::Delete => {
                    state.remove(&op.quad);
                }
            }
        }
    }
    Ok(state)
}

/// Commit and patch bytes from genesis to `id` (parent-0 order, genesis first).
async fn prefetch_chain(
    owner: &PgPool,
    ids: &[CommitId],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
    let commit_ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
    let rows = sqlx::query("SELECT id, bytes FROM immutable_objects WHERE id = ANY($1)")
        .bind(&commit_ids)
        .fetch_all(owner)
        .await
        .map_err(|e| e.to_string())?;
    let mut by_id: BTreeMap<String, Vec<u8>> = rows
        .into_iter()
        .map(|r| (r.get::<String, _>("id"), r.get::<Vec<u8>, _>("bytes")))
        .collect();
    let mut patches = Vec::new();
    for c in &commit_ids {
        let bytes = by_id.get(c).ok_or("commit object missing")?;
        let commit = AnyCommit::from_canonical_bytes(bytes).map_err(|e| e.to_string())?;
        patches.push(commit.patch().to_string());
    }
    let rows = sqlx::query("SELECT id, bytes FROM immutable_objects WHERE id = ANY($1)")
        .bind(&patches)
        .fetch_all(owner)
        .await
        .map_err(|e| e.to_string())?;
    for r in rows {
        by_id.insert(r.get::<String, _>("id"), r.get::<Vec<u8>, _>("bytes"));
    }
    commit_ids
        .iter()
        .zip(&patches)
        .map(|(c, p)| {
            Ok((
                by_id.get(c).ok_or("commit")?.clone(),
                by_id.get(p).ok_or("patch")?.clone(),
            ))
        })
        .collect()
}

async fn restart_database(cfg: &ReconConfig, client: &Client) -> Result<Duration, String> {
    let cmd = cfg.restart_cmd.as_ref().ok_or("no --restart-cmd")?;
    let started = Instant::now();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("restart command failed: {status}"));
    }
    // Ready again: the server answers /ready (which checks the database).
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok((200, _, _, _)) = client
            .call(reqwest::Method::GET, "/ready", false, None)
            .await
        {
            return Ok(started.elapsed());
        }
        if Instant::now() > deadline {
            return Err("the stack did not become ready after the database restart".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Run the reconstruction characterization.
pub async fn run(cfg: &ReconConfig) -> ReconResult {
    let mut result = ReconResult {
        schema: RECON_SCHEMA.into(),
        status: "pass".into(),
        ..ReconResult::default()
    };
    let outcome = run_inner(cfg, &mut result).await;
    if let Err(e) = outcome {
        result.failures.push(format!("run aborted: {e}"));
    }
    if !result.failures.is_empty() {
        result.status = "fail".into();
    }
    result
}

async fn run_inner(cfg: &ReconConfig, result: &mut ReconResult) -> Result<(), String> {
    let owner = PgPool::connect(&cfg.owner_database_url)
        .await
        .map_err(|e| e.to_string())?;
    let store = PostgresLedgerStore::from_pool_migrated(owner.clone(), V1Binding::Reject);
    let client = Client::new(cfg);
    let limits = ReconstructionLimits::DEVELOPMENT;
    let rt = read_time_column(&owner).await;

    // ---- build (concurrently per state size; not measured) ----
    let started = Instant::now();
    let builds = futures_join(cfg, &client, &owner).await?;
    result.build_ms.insert(
        "all histories (concurrent)".into(),
        started.elapsed().as_millis(),
    );
    // Settle the database after the concurrent write build, so measurement does not start
    // in the middle of autovacuum or a checkpoint: VACUUM (ANALYZE), then CHECKPOINT.
    // A settle failure aborts the run: measuring an unsettled database would violate the
    // stated precondition of every figure below, so no result may be emitted as `pass`.
    let settle = Instant::now();
    for sql in ["VACUUM (ANALYZE)", "CHECKPOINT"] {
        sqlx::query(sql)
            .execute(&owner)
            .await
            .map_err(|e| format!("settle `{sql}` failed, the database is not quiesced: {e}"))?;
        result
            .environment
            .insert(format!("settle: {sql}"), "done".into());
    }
    result.build_ms.insert(
        "settle (vacuum analyze, checkpoint)".into(),
        settle.elapsed().as_millis(),
    );

    let check = |result: &mut ReconResult, family: &str, ok: bool, detail: String| {
        *result.correctness.entry(family.into()).or_default() += 1;
        if !ok {
            result.failures.push(format!("{family}: {detail}"));
        }
    };

    for h in &builds {
        for &depth in &cfg.depths {
            let id = h.ids[depth].clone();
            let (quads, _, digest, _) = h.expected[depth].clone();
            let state_path = format!("/v1/graphs/{}/commits/{id}/state", h.graph);

            // API state read (warm).
            let mut bytes = 0;
            for _ in 0..cfg.warmup {
                client
                    .expect(reqwest::Method::GET, &state_path, false, None, 200)
                    .await?;
            }
            let w = open_window(cfg, &owner, rt, RUNTIME_ROLE).await;
            let mut samples = Vec::new();
            let mut replies = Vec::with_capacity(cfg.reps);
            for _ in 0..cfg.reps {
                let (v, t, n) = client
                    .expect(reqwest::Method::GET, &state_path, false, None, 200)
                    .await?;
                samples.push(us(t));
                bytes = n;
                replies.push(v);
            }
            let (pg, cpu) = close_window(cfg, &owner, rt, RUNTIME_ROLE, w, cfg.reps).await;
            // Untimed: every timed reply equals the oracle state exactly.
            let wrong = replies
                .iter()
                .filter(|v| !reply_matches(v, quads, &digest))
                .count();
            drop(replies);
            check(
                result,
                "API state equals the oracle state (digest)",
                wrong == 0,
                format!(
                    "S={} depth={depth}: {wrong} of {} differ",
                    h.states, cfg.reps
                ),
            );
            result.points.push(point(
                h,
                depth,
                ("api", "state_read", "warm"),
                &mut samples,
                Some(bytes),
                (pg, cpu),
            ));

            // Direct store reconstruction (same production fold, no HTTP).
            for _ in 0..cfg.warmup {
                store
                    .workflows()
                    .reconstruct(&id, &limits)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let w = open_window(cfg, &owner, rt, OWNER_ROLE).await;
            let mut samples = Vec::new();
            let mut states = Vec::with_capacity(cfg.reps);
            for _ in 0..cfg.reps {
                let t = Instant::now();
                let s = store
                    .workflows()
                    .reconstruct(&id, &limits)
                    .await
                    .map_err(|e| e.to_string())?;
                samples.push(us(t.elapsed()));
                states.push(s);
            }
            let (pg, cpu) = close_window(cfg, &owner, rt, OWNER_ROLE, w, cfg.reps).await;
            // Untimed: every timed result equals the oracle state exactly.
            let wrong = states
                .iter()
                .filter(|s| s.len() != quads || state_digest(s) != digest)
                .count();
            drop(states);
            check(
                result,
                "store reconstruction equals the oracle state (digest)",
                wrong == 0,
                format!(
                    "S={} depth={depth}: {wrong} of {} differ",
                    h.states, cfg.reps
                ),
            );
            result.points.push(point(
                h,
                depth,
                ("persisted", "store_reconstruct", "warm"),
                &mut samples,
                None,
                (pg, cpu),
            ));

            // The fold's CPU work alone, on prefetched objects.
            let chain = prefetch_chain(&owner, &h.ids[..=depth]).await?;
            let mut samples = Vec::new();
            let mut wrong = 0;
            for i in 0..cfg.warmup + cfg.reps {
                let t = Instant::now();
                let state = fold_cpu(&chain)?;
                let elapsed = t.elapsed();
                if i >= cfg.warmup {
                    samples.push(us(elapsed));
                }
                // Untimed (after the elapsed time was taken).
                if state.len() != quads || state_digest(&state) != digest {
                    wrong += 1;
                }
            }
            check(
                result,
                "prefetched fold equals the oracle state (digest)",
                wrong == 0,
                format!("S={} depth={depth}: {wrong} differ", h.states),
            );
            result.points.push(point(
                h,
                depth,
                ("algorithm", "fold_cpu", "warm"),
                &mut samples,
                None,
                (None, None),
            ));

            // Prepare at this depth: a branch at the commit, then prepares (not accepted).
            let branch = format!("recon/d{depth}");
            client
                .expect(
                    reqwest::Method::POST,
                    &format!("/v1/graphs/{}/branches", h.graph),
                    true,
                    Some(&json!({"name": branch, "source": "main", "from_commit": id.to_string()})),
                    201,
                )
                .await?;
            let prepare = |i: usize| {
                json!({"ref": branch, "expected_head": id.to_string(),
                    "operations": [{"op": "add", "quad": probe_quad(depth, i)}],
                    "activity": "benchmark-recon", "message": "prepare probe", "evidence_refs": []})
            };
            let path = format!("/v1/graphs/{}/proposals", h.graph);
            for i in 0..cfg.warmup {
                client
                    .expect(reqwest::Method::POST, &path, true, Some(&prepare(i)), 201)
                    .await?;
            }
            let w = open_window(cfg, &owner, rt, RUNTIME_ROLE).await;
            let mut samples = Vec::new();
            let mut candidates = Vec::with_capacity(cfg.reps);
            for i in 0..cfg.reps {
                let (v, t, _) = client
                    .expect(
                        reqwest::Method::POST,
                        &path,
                        true,
                        Some(&prepare(cfg.warmup + i)),
                        201,
                    )
                    .await?;
                samples.push(us(t));
                candidates.push(v["candidate"].as_str().unwrap_or_default().to_owned());
            }
            let (pg, cpu) = close_window(cfg, &owner, rt, RUNTIME_ROLE, w, cfg.reps).await;
            // Untimed: every prepared candidate holds the base state plus its probe quad.
            let mut wrong = 0;
            for (i, candidate) in candidates.iter().enumerate() {
                let mut want = h.state_at(depth);
                want.insert(probe_quad(depth, cfg.warmup + i));
                let (v, _, _) = client
                    .expect(
                        reqwest::Method::GET,
                        &format!("/v1/graphs/{}/commits/{candidate}/state", h.graph),
                        false,
                        None,
                        200,
                    )
                    .await?;
                if !reply_matches(
                    &v,
                    want.len(),
                    &oracle_digest(want.iter().map(String::as_str)),
                ) {
                    wrong += 1;
                }
            }
            check(
                result,
                "prepared candidate equals base state plus probe (digest)",
                wrong == 0,
                format!(
                    "S={} depth={depth}: {wrong} of {} differ",
                    h.states, cfg.reps
                ),
            );
            result.points.push(point(
                h,
                depth,
                ("api", "prepare", "warm"),
                &mut samples,
                None,
                (pg, cpu),
            ));

            // Merge shapes at this depth: x at the commit, y one commit ahead → x is
            // contained in y (ancestry walk only); then x one commit ahead → divergent.
            let (x, y) = (format!("recon/x{depth}"), format!("recon/y{depth}"));
            for b in [&x, &y] {
                client
                    .expect(
                        reqwest::Method::POST,
                        &format!("/v1/graphs/{}/branches", h.graph),
                        true,
                        Some(&json!({"name": b, "source": "main", "from_commit": id.to_string()})),
                        201,
                    )
                    .await?;
            }
            let probe = |tag: &str| {
                vec![format!(
                    "<urn:recon:merge> <urn:recon:{tag}> \"d{depth}\" ."
                )]
            };
            client
                .commit(&h.graph, &y, Some(&id), &[], &probe("y"))
                .await?;
            let preview_path = format!("/v1/graphs/{}/merges/preview", h.graph);
            let body = json!({"source": x, "target": y});
            for (op, expected_class) in [
                ("merge_preview_contained", "already_contained"),
                ("merge_preview_divergent", "divergent"),
            ] {
                if op == "merge_preview_divergent" {
                    client
                        .commit(&h.graph, &x, Some(&id), &[], &probe("x"))
                        .await?;
                }
                let (v, _, _) = client
                    .expect(
                        reqwest::Method::POST,
                        &preview_path,
                        false,
                        Some(&body),
                        200,
                    )
                    .await?;
                check(
                    result,
                    "merge preview classification",
                    v["classification"] == expected_class,
                    format!(
                        "S={} depth={depth}: {} (expected {expected_class})",
                        h.states, v["classification"]
                    ),
                );
                let w = open_window(cfg, &owner, rt, RUNTIME_ROLE).await;
                let mut samples = Vec::new();
                let mut replies = Vec::with_capacity(cfg.preview_reps);
                for _ in 0..cfg.preview_reps {
                    let (v, t, _) = client
                        .expect(
                            reqwest::Method::POST,
                            &preview_path,
                            false,
                            Some(&body),
                            200,
                        )
                        .await?;
                    samples.push(us(t));
                    replies.push(v);
                }
                let (pg, cpu) =
                    close_window(cfg, &owner, rt, RUNTIME_ROLE, w, cfg.preview_reps).await;
                // Untimed: every timed reply is classified correctly; a divergent preview's
                // merged state is the base state plus both probes (its protocol digest is
                // computed by the labelled `ledger_rdf::state_digest` over the harness's set).
                let merged = (op == "merge_preview_divergent").then(|| {
                    let mut want = h.state_at(depth);
                    want.extend(probe("x"));
                    want.extend(probe("y"));
                    protocol_digest(&want)
                });
                let wrong = replies
                    .iter()
                    .filter(|v| {
                        v["classification"] != expected_class
                            || merged.as_ref().is_some_and(|m| {
                                v["merged_state_digest"].as_str() != Some(m.as_str())
                            })
                    })
                    .count();
                check(
                    result,
                    "merge preview replies (classification, merged state digest)",
                    wrong == 0,
                    format!(
                        "S={} depth={depth} {op}: {wrong} of {} differ",
                        h.states, cfg.preview_reps
                    ),
                );
                result.points.push(point(
                    h,
                    depth,
                    ("api", op, "warm"),
                    &mut samples,
                    None,
                    (pg, cpu),
                ));
            }

            // After a PostgreSQL restart: the first ledger reconstruction (module docs).
            if cfg.cold_depths.contains(&depth) {
                for (category, op) in [("api", "state_read"), ("persisted", "store_reconstruct")] {
                    let role = if category == "api" {
                        RUNTIME_ROLE
                    } else {
                        OWNER_ROLE
                    };
                    let mut samples = Vec::new();
                    let mut reads = Vec::new();
                    let mut wrong = 0;
                    for _ in 0..cfg.cold_reps {
                        restart_database(cfg, &client).await?;
                        // The store path gets a fresh one-connection pool, connected before
                        // the window: the timed reconstruction is the first ledger operation,
                        // never a pool reconnection (the API server's own pool reconnects as
                        // in production).
                        let cold_store = if category == "api" {
                            None
                        } else {
                            let pool = sqlx::postgres::PgPoolOptions::new()
                                .max_connections(1)
                                .connect(&cfg.owner_database_url)
                                .await
                                .map_err(|e| e.to_string())?;
                            Some(PostgresLedgerStore::from_pool_migrated(
                                pool,
                                V1Binding::Reject,
                            ))
                        };
                        let w = open_window(cfg, &owner, rt, role).await;
                        let t = Instant::now();
                        let got = if category == "api" {
                            let (v, _, _) = client
                                .expect(reqwest::Method::GET, &state_path, false, None, 200)
                                .await?;
                            Err(v)
                        } else {
                            Ok(cold_store
                                .as_ref()
                                .expect("store path")
                                .workflows()
                                .reconstruct(&id, &limits)
                                .await
                                .map_err(|e| e.to_string())?)
                        };
                        samples.push(us(t.elapsed()));
                        let (pg, _) = close_window(cfg, &owner, rt, role, w, 1).await;
                        reads.push(pg);
                        // Untimed exact check of the measured result.
                        let ok = match &got {
                            Ok(state) => state.len() == quads && state_digest(state) == digest,
                            Err(v) => reply_matches(v, quads, &digest),
                        };
                        if !ok {
                            wrong += 1;
                        }
                    }
                    check(
                        result,
                        "state after a database restart equals the oracle state (digest)",
                        wrong == 0,
                        format!("S={} depth={depth} {op}: {wrong} differ", h.states),
                    );
                    let pg = average_pg(&reads);
                    result.points.push(point(
                        h,
                        depth,
                        (category, op, FIRST_AFTER_RESTART),
                        &mut samples,
                        None,
                        (pg, None),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn average_pg(deltas: &[Option<PgDelta>]) -> Option<PgDelta> {
    let all: Vec<&PgDelta> = deltas.iter().flatten().collect();
    if all.is_empty() {
        return None;
    }
    let n = all.len() as f64;
    let avg = |f: fn(&PgDelta) -> f64| all.iter().map(|d| f(d)).sum::<f64>() / n;
    Some(PgDelta {
        calls: avg(|d| d.calls),
        rows: avg(|d| d.rows),
        shared_blks_hit: avg(|d| d.shared_blks_hit),
        shared_blks_read: avg(|d| d.shared_blks_read),
        temp_blks: avg(|d| d.temp_blks),
        blk_read_time_ms: avg(|d| d.blk_read_time_ms),
        exec_time_ms: avg(|d| d.exec_time_ms),
    })
}

async fn futures_join(
    cfg: &ReconConfig,
    client: &Client,
    owner: &PgPool,
) -> Result<Vec<History>, String> {
    let mut handles = Vec::new();
    for &s in &cfg.states {
        handles.push(build(cfg, client, owner, s));
    }
    let mut out = Vec::new();
    // Concurrent build of every size (a join of the futures on this task).
    let results = join_all(handles).await;
    for r in results {
        out.push(r?);
    }
    Ok(out)
}

/// Minimal `join_all` (no extra dependency): polls every future to completion concurrently.
async fn join_all<F: std::future::Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut pinned: Vec<std::pin::Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = (0..pinned.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in pinned.iter_mut().enumerate() {
            if outputs[i].is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => outputs[i] = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    outputs.into_iter().map(|o| o.expect("completed")).collect()
}

/// The report, rendered only from the JSON result.
pub fn markdown(r: &ReconResult) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "# Reconstruction characterization: **{}**\n", r.status);
    if r.meta.get("official").map(String::as_str) != Some("yes") {
        let _ = writeln!(
            out,
            "> **NON-OFFICIAL RUN** (official = `{}`): not from a clean checkout of a recorded revision; not architecture evidence.\n",
            r.meta.get("official").map_or("unset", String::as_str)
        );
    }
    let _ = writeln!(
        out,
        "Generated from `recon.json` ({}); the JSON is authoritative. p95 only for n ≥ 20 (the second-largest of 20), p99 only for n ≥ 100. PostgreSQL figures are per operation, from `pg_stat_statements` (8 KiB buffer counts, not physical disk bytes). CPU is cgroup `cpu.stat` per operation of the whole container, read immediately around the measured operations (statistics queries outside); memory is `memory.current` after the batch (page cache included). `{}`: PostgreSQL restarted, then readiness and statistics queries, then the measured first ledger reconstruction; OS page cache not dropped.\n",
        r.schema, FIRST_AFTER_RESTART
    );
    let _ = writeln!(out, "| | |\n|---|---|");
    for (k, v) in r.meta.iter().chain(&r.environment) {
        let _ = writeln!(out, "| {k} | `{v}` |");
    }
    for (k, v) in &r.build_ms {
        let _ = writeln!(out, "| build: {k} | {:.1} s |", *v as f64 / 1000.0);
    }
    let _ = writeln!(
        out,
        "\nCorrectness: {:?}; failures: {}\n",
        r.correctness,
        r.failures.len()
    );
    for f in &r.failures {
        let _ = writeln!(out, "- {f}");
    }
    let _ = writeln!(
        out,
        "| S quads | depth | fold ops | state bytes | category | op | cache | n | p50 ms | p95 ms | mean ms | resp KiB | PG calls | rows | blks hit | blks read | read ms | exec ms | server CPU ms | PG CPU ms |\n|---:|---:|---:|---:|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    );
    let f1 = |v: Option<f64>| v.map_or("—".to_owned(), |v| format!("{v:.1}"));
    for p in &r.points {
        let pg = p.pg.clone().unwrap_or_default();
        let has_pg = p.pg.is_some();
        let cpu = p.cpu.clone().unwrap_or_default();
        let g = |v: f64| {
            if has_pg {
                format!("{v:.1}")
            } else {
                "—".into()
            }
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {:.2} | {} | {:.2} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            p.state_quads,
            p.depth,
            p.fold_ops,
            p.canonical_state_bytes,
            p.category,
            p.op,
            p.cache,
            p.n,
            p.p50_ms,
            f1(p.p95_ms),
            p.mean_ms,
            p.response_bytes
                .map_or("—".into(), |b| format!("{:.1}", b as f64 / 1024.0)),
            g(pg.calls),
            g(pg.rows),
            g(pg.shared_blks_hit),
            g(pg.shared_blks_read),
            g(pg.blk_read_time_ms),
            g(pg.exec_time_ms),
            f1(cpu.server_cpu_ms),
            f1(cpu.postgres_cpu_ms),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_all_completes_every_future_in_order() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(join_all(vec![
            Box::pin(async { 1 }) as std::pin::Pin<Box<dyn std::future::Future<Output = i32>>>,
            Box::pin(async {
                tokio::task::yield_now().await;
                2
            }),
        ]));
        assert_eq!(out, [1, 2]);
    }

    #[test]
    fn the_fold_cpu_reproduces_a_state() {
        let p1 = Patch::new([ledger_rdf::Operation {
            kind: OperationKind::Add,
            quad: quad(0, 0).parse().unwrap(),
        }])
        .unwrap();
        let p2 = Patch::new([
            ledger_rdf::Operation {
                kind: OperationKind::Delete,
                quad: quad(0, 0).parse().unwrap(),
            },
            ledger_rdf::Operation {
                kind: OperationKind::Add,
                quad: quad(0, 1).parse().unwrap(),
            },
        ])
        .unwrap();
        let commit = |patch: &Patch| {
            AnyCommit::V1(ledger_core::Commit {
                parents: vec![],
                patch: patch.id(),
                author: "a".into(),
                message: "m".into(),
                event_time: "e".into(),
                recorded_time: "r".into(),
            })
            .canonical_bytes()
            .unwrap()
        };
        let chain = vec![
            (commit(&p1), p1.canonical_bytes()),
            (commit(&p2), p2.canonical_bytes()),
        ];
        let state = fold_cpu(&chain).unwrap();
        let expected: BTreeSet<String> = [quad(0, 1)].into_iter().collect();
        assert_eq!(state_digest(&state), fingerprint(&expected, 0).2);
    }

    #[test]
    fn a_same_cardinality_wrong_state_does_not_match_the_oracle_digest() {
        let expected: BTreeSet<String> = [quad(0, 1), quad(1, 1)].into_iter().collect();
        let right: BTreeSet<Quad> = expected.iter().map(|q| q.parse().unwrap()).collect();
        let wrong: BTreeSet<Quad> = [quad(0, 1), quad(1, 2)]
            .iter()
            .map(|q| q.parse().unwrap())
            .collect();
        let digest = fingerprint(&expected, 0).2;
        assert_eq!(state_digest(&right), digest);
        assert_eq!(wrong.len(), right.len());
        assert_ne!(state_digest(&wrong), digest);
    }

    fn config() -> ReconConfig {
        ReconConfig {
            replica: "http://127.0.0.1:8080".into(),
            owner_database_url: String::new(),
            secret: "s".into(),
            issuer: String::new(),
            audience: String::new(),
            states: vec![1, 1_000, 10_000],
            depths: vec![1, 10, 100, 500, 1_000, 2_500, 5_000],
            reps: 20,
            warmup: 3,
            preview_reps: 10,
            cold_depths: vec![100, 1_000, 5_000],
            cold_reps: 3,
            restart_cmd: Some("true".into()),
            server_cgroup: None,
            postgres_cgroup: None,
            depth_limit: 10_000,
            run_id: "t".into(),
        }
    }

    #[test]
    fn invalid_configurations_are_refused_before_running() {
        assert_eq!(config().validate(), Ok(()));
        type Mutation = fn(&mut ReconConfig);
        let cases: Vec<(Mutation, &str)> = vec![
            (|c| c.states = vec![], "empty"),
            (|c| c.depths = vec![], "empty"),
            (|c| c.states = vec![1, 0], "contain 0"),
            (|c| c.states = vec![2_000_000], "exceeds"),
            (
                |c| {
                    c.states = vec![10_001];
                    c.depths = vec![1, 100];
                    c.cold_depths = vec![];
                },
                "complete only from depth 2",
            ),
            (|c| c.reps = 0, "at least 1"),
            (|c| c.preview_reps = 0, "at least 1"),
            (|c| c.cold_reps = 0, "--cold-reps"),
            (|c| c.restart_cmd = None, "--restart-cmd"),
            (|c| c.cold_depths = vec![7], "not one of --depths"),
            (|c| c.depths = vec![10_000], "depth limit"),
            (
                |c| {
                    c.depth_limit = 1_000;
                    c.depths = vec![1_000];
                    c.cold_depths = vec![];
                },
                "depth limit 1000",
            ),
        ];
        for (mutate, needle) in cases {
            let mut c = config();
            mutate(&mut c);
            let err = c.validate().unwrap_err();
            assert!(err.contains(needle), "{err}");
        }
        // No cold measurements: neither a restart command nor cold repetitions are needed.
        let mut c = config();
        c.cold_depths = vec![];
        c.cold_reps = 0;
        c.restart_cmd = None;
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn the_cpu_window_excludes_the_statistics_queries() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let log = std::cell::RefCell::new(Vec::new());
        let cpu = |at: u64| CpuReading {
            server_usec: Some(at),
            postgres_usec: Some(at),
            server_memory: Some(at),
            postgres_memory: None,
        };
        let w = rt.block_on(open_with(
            || async {
                log.borrow_mut().push("open: statistics");
                Some([0.0; 7])
            },
            || {
                log.borrow_mut().push("open: cpu");
                cpu(1_000)
            },
        ));
        log.borrow_mut().push("measured operations");
        let (pg, c) = rt.block_on(close_with(
            w,
            2,
            || {
                log.borrow_mut().push("close: cpu");
                cpu(5_000)
            },
            || async {
                log.borrow_mut().push("close: statistics");
                Some([10.0, 4.0, 6.0, 2.0, 0.0, 1.0, 3.0])
            },
        ));
        assert_eq!(
            *log.borrow(),
            [
                "open: statistics",
                "open: cpu",
                "measured operations",
                "close: cpu",
                "close: statistics"
            ]
        );
        let pg = pg.unwrap();
        assert_eq!((pg.calls, pg.rows, pg.exec_time_ms), (5.0, 2.0, 1.5));
        let c = c.unwrap();
        // (5000 - 1000) µs over 2 operations = 2 ms each.
        assert_eq!(c.server_cpu_ms, Some(2.0));
        assert_eq!(c.postgres_cpu_ms, Some(2.0));
        assert_eq!(c.server_memory_bytes, Some(5_000));
        assert_eq!(c.postgres_memory_bytes, None);
    }

    #[test]
    fn missing_sources_give_no_figures_rather_than_zeros() {
        let w = Window {
            pg: None,
            cpu: CpuReading::default(),
        };
        let (pg, c) = derive(&w, Some([1.0; 7]), CpuReading::default(), 3);
        assert!(pg.is_none());
        let c = c.unwrap();
        assert_eq!((c.server_cpu_ms, c.postgres_cpu_ms), (None, None));
    }

    #[test]
    fn fingerprints_count_canonical_bytes() {
        let s: BTreeSet<String> = [quad(0, 1)].into_iter().collect();
        let (n, bytes, _, ops) = fingerprint(&s, 7);
        assert_eq!((n, bytes, ops), (1, quad(0, 1).len() as u64 + 1, 7));
    }
}
