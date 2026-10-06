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
//!   includes HTTP, server JSON encoding, transfer and client decoding.
//! - `persisted` / `store_reconstruct`: the production fold
//!   (`WorkflowRepository::reconstruct`) on an owner pool in this process. Same work, no HTTP.
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
//! Around each measured batch it records:
//! - `pg_stat_statements` deltas for the role that ran it (runtime role for API batches,
//!   owner for store batches): calls, rows, shared block hits and reads, temporary blocks,
//!   block read time and execution time. These are 8 KiB buffer counts from PostgreSQL's
//!   statistics, not physical disk bytes.
//! - cgroup CPU deltas of the server and PostgreSQL containers, and their memory.
//!
//! **Cache conditions.**
//! - `warm`: after warm-up repetitions on an active database.
//! - `db-restart-cold`: the first operation after PostgreSQL was restarted (`--restart-cmd`)
//!   and the server reported ready again.
//!
//! An OS-page-cache-cold condition is never claimed: the host cache is not dropped.

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
    pub run_id: String,
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
    /// `warm` or `db-restart-cold`.
    pub cache: String,
    pub n: usize,
    pub p50_ms: f64,
    /// Only with at least 20 samples.
    pub p95_ms: Option<f64>,
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
        let elapsed = started.elapsed();
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            elapsed,
            bytes.len(),
        ))
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
    }
    Ok(History {
        graph,
        states,
        ids,
        expected,
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

/// `pg_stat_statements` totals for one role in the current database.
async fn pg_totals(owner: &PgPool, role: &str) -> Option<[f64; 7]> {
    let read_time: String = if sqlx::query_scalar::<_, bool>(&format!(
        "SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = '{STATS_SCHEMA}.pg_stat_statements'::regclass \
         AND attname = 'shared_blk_read_time')"
    ))
    .fetch_one(owner)
    .await
    .ok()?
    {
        "shared_blk_read_time".into()
    } else {
        "blk_read_time".into()
    };
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

struct Window {
    pg: Option<[f64; 7]>,
    server_cpu: Option<u64>,
    pg_cpu: Option<u64>,
}

async fn open_window(cfg: &ReconConfig, owner: &PgPool, role: &str) -> Window {
    // Statement statistics are reset so the batch alone is attributed; the reset itself
    // and the totals query are excluded by the query filter.
    let _ = sqlx::query(&format!("SELECT {STATS_SCHEMA}.pg_stat_statements_reset()"))
        .execute(owner)
        .await;
    Window {
        pg: pg_totals(owner, role).await,
        server_cpu: cgroup_cpu_usec(&cfg.server_cgroup),
        pg_cpu: cgroup_cpu_usec(&cfg.postgres_cgroup),
    }
}

async fn close_window(
    cfg: &ReconConfig,
    owner: &PgPool,
    role: &str,
    w: Window,
    ops: usize,
) -> (Option<PgDelta>, Option<CpuDelta>) {
    let n = ops.max(1) as f64;
    let after = pg_totals(owner, role).await;
    let pg = match (w.pg, after) {
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
        server_cpu_ms: delta(w.server_cpu, cgroup_cpu_usec(&cfg.server_cgroup)),
        postgres_cpu_ms: delta(w.pg_cpu, cgroup_cpu_usec(&cfg.postgres_cgroup)),
        server_memory_bytes: cgroup_memory(&cfg.server_cgroup),
        postgres_memory_bytes: cgroup_memory(&cfg.postgres_cgroup),
    };
    (pg, Some(cpu))
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
    let (_, bytes, _, fold_ops) = h.expected[depth];
    let tail = s.count >= crate::result::MIN_TAIL_SAMPLES;
    ReconPoint {
        state_quads: h.states,
        depth,
        fold_ops,
        canonical_state_bytes: bytes,
        category: category.into(),
        op: op.into(),
        cache: cache.into(),
        n: s.count,
        p50_ms: s.p50_ms,
        p95_ms: tail.then_some(s.p95_ms),
        p99_ms: tail.then_some(s.p99_ms),
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
fn fold_cpu(chain: &[(Vec<u8>, Vec<u8>)]) -> Result<usize, String> {
    let mut state: BTreeSet<Quad> = BTreeSet::new();
    for (commit, patch) in chain {
        let _ = ContentId::for_bytes(commit);
        AnyCommit::from_canonical_bytes(commit).map_err(|e| e.to_string())?;
        let _ = ContentId::for_bytes(patch);
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
    Ok(state.len())
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

    // ---- build (concurrently per state size; not measured) ----
    let started = Instant::now();
    let builds = futures_join(cfg, &client, &owner).await?;
    result.build_ms.insert(
        "all histories (concurrent)".into(),
        started.elapsed().as_millis(),
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
            let w = open_window(cfg, &owner, RUNTIME_ROLE).await;
            let mut samples = Vec::new();
            let mut last = Value::Null;
            for _ in 0..cfg.reps {
                let (v, t, n) = client
                    .expect(reqwest::Method::GET, &state_path, false, None, 200)
                    .await?;
                samples.push(us(t));
                bytes = n;
                last = v;
            }
            let (pg, cpu) = close_window(cfg, &owner, RUNTIME_ROLE, w, cfg.reps).await;
            let lines: Vec<&str> = last["quads"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            check(
                result,
                "state at depth equals the expected constant state",
                lines.len() == quads && oracle_digest(lines.iter().copied()) == digest,
                format!("S={} depth={depth}: {} quads", h.states, lines.len()),
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
            let w = open_window(cfg, &owner, OWNER_ROLE).await;
            let mut samples = Vec::new();
            for _ in 0..cfg.reps {
                let t = Instant::now();
                let s = store
                    .workflows()
                    .reconstruct(&id, &limits)
                    .await
                    .map_err(|e| e.to_string())?;
                samples.push(us(t.elapsed()));
                if s.len() != quads {
                    check(
                        result,
                        "store reconstruction size",
                        false,
                        format!("S={} depth={depth}", h.states),
                    );
                }
            }
            let (pg, cpu) = close_window(cfg, &owner, OWNER_ROLE, w, cfg.reps).await;
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
            for i in 0..cfg.warmup + cfg.reps {
                let t = Instant::now();
                let n = fold_cpu(&chain)?;
                if i >= cfg.warmup {
                    samples.push(us(t.elapsed()));
                }
                if n != quads {
                    check(
                        result,
                        "fold_cpu size",
                        false,
                        format!("S={} depth={depth}", h.states),
                    );
                }
            }
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
                    "operations": [{"op": "add", "quad": format!("<urn:recon:prep> <urn:recon:p> \"d{depth}-{i}\" .")}],
                    "activity": "benchmark-recon", "message": "prepare probe", "evidence_refs": []})
            };
            let path = format!("/v1/graphs/{}/proposals", h.graph);
            for i in 0..cfg.warmup {
                client
                    .expect(reqwest::Method::POST, &path, true, Some(&prepare(i)), 201)
                    .await?;
            }
            let w = open_window(cfg, &owner, RUNTIME_ROLE).await;
            let mut samples = Vec::new();
            for i in 0..cfg.reps {
                let (_, t, _) = client
                    .expect(
                        reqwest::Method::POST,
                        &path,
                        true,
                        Some(&prepare(cfg.warmup + i)),
                        201,
                    )
                    .await?;
                samples.push(us(t));
            }
            let (pg, cpu) = close_window(cfg, &owner, RUNTIME_ROLE, w, cfg.reps).await;
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
                let w = open_window(cfg, &owner, RUNTIME_ROLE).await;
                let mut samples = Vec::new();
                for _ in 0..cfg.preview_reps {
                    let (_, t, _) = client
                        .expect(
                            reqwest::Method::POST,
                            &preview_path,
                            false,
                            Some(&body),
                            200,
                        )
                        .await?;
                    samples.push(us(t));
                }
                let (pg, cpu) = close_window(cfg, &owner, RUNTIME_ROLE, w, cfg.preview_reps).await;
                result.points.push(point(
                    h,
                    depth,
                    ("api", op, "warm"),
                    &mut samples,
                    None,
                    (pg, cpu),
                ));
            }

            // Database-restart cold: the first operation after a PostgreSQL restart.
            if cfg.cold_depths.contains(&depth) && cfg.restart_cmd.is_some() {
                for (category, op) in [("api", "state_read"), ("persisted", "store_reconstruct")] {
                    let mut samples = Vec::new();
                    let mut reads = Vec::new();
                    for _ in 0..cfg.cold_reps {
                        restart_database(cfg, &client).await?;
                        let w = open_window(
                            cfg,
                            &owner,
                            if category == "api" {
                                RUNTIME_ROLE
                            } else {
                                OWNER_ROLE
                            },
                        )
                        .await;
                        let t = Instant::now();
                        if category == "api" {
                            client
                                .expect(reqwest::Method::GET, &state_path, false, None, 200)
                                .await?;
                        } else {
                            store
                                .workflows()
                                .reconstruct(&id, &limits)
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        samples.push(us(t.elapsed()));
                        let (pg, _) = close_window(
                            cfg,
                            &owner,
                            if category == "api" {
                                RUNTIME_ROLE
                            } else {
                                OWNER_ROLE
                            },
                            w,
                            1,
                        )
                        .await;
                        reads.push(pg);
                    }
                    let pg = average_pg(&reads);
                    result.points.push(point(
                        h,
                        depth,
                        (category, op, "db-restart-cold"),
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
    let _ = writeln!(
        out,
        "Generated from `recon.json` ({}); the JSON is authoritative. p95/p99 only for n ≥ 20. PostgreSQL figures are per operation, from `pg_stat_statements` (8 KiB buffer counts, not physical disk bytes). CPU is cgroup `cpu.stat` per operation; memory is `memory.current` after the batch (page cache included).\n",
        r.schema
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
        assert_eq!(fold_cpu(&chain).unwrap(), 1);
    }

    #[test]
    fn fingerprints_count_canonical_bytes() {
        let s: BTreeSet<String> = [quad(0, 1)].into_iter().collect();
        let (n, bytes, _, ops) = fingerprint(&s, 7);
        assert_eq!((n, bytes, ops), (1, quad(0, 1).len() as u64 + 1, 7));
    }
}
