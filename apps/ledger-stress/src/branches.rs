//! Plan 0008: 100-branch stress on two replicas.
//!
//! `ledger-stress branches …` runs, through the public HTTP API against several server
//! replicas, one graph with many concurrent cognitive work branches while `main` keeps
//! moving:
//!
//! 1. `main` is seeded with two commits (the second is the head, the first a historical
//!    branch point);
//! 2. concurrently: `--branches` agents each create `agent/wNNN` (every fourth from the
//!    historical root, the rest from `main`'s head of the moment) and land
//!    `--commits-per-branch` prepare → accept cycles on it, while `--main-writers` land
//!    commits on `main` under CAS contention; every tenth agent duplicates each request to a
//!    second replica under the same `Idempotency-Key` (one original execution, identical
//!    answers);
//! 3. lifecycle races: every fifth branch is deleted by an administrator while its agent
//!    races one more acceptance (either the acceptance lands before the tombstone, which then
//!    names the new head, or it is refused `BRANCH_DELETED`; nothing in between); a prepare on
//!    each deleted branch is refused; every tenth branch is restored (duplicated across
//!    replicas) and lands one more commit; a second restore is refused
//!    `BRANCH_STATE_CONFLICT`;
//! 4. reads: list, status of every branch and one history per branch across replicas.
//!
//! Then, under the owner identity: per branch `refs.version = count(ref_events)` = 1 +
//! client landings, every client landing is a ref event, the creation event names the
//! expected branch point, `lifecycle_version = count(branch_events)`, the final status is the
//! expected one and a tombstone names the head the branch stopped at; outbox rows = accepted
//! events (creation writes none); `main` passes the Plan-0005 graph invariants with exactly
//! its own landings (branch traffic never moved it); the unconfigured projection backlog is
//! exactly `main`'s rows; no deadlock, no unexpected error class, no pair disagreement; the
//! read-only verifier clean; successful p99 per operation under the budget.

use crate::{
    Api, Call, Metrics, Op, Outcome, VerifyReport, environment, graph_invariants,
    is_expected_failure, jitter, mint_roles, now_secs, percentiles, provision, require_loopback,
};
use ledger_store::verify;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{
    collections::BTreeMap,
    process::ExitCode,
    sync::{Arc, atomic::AtomicU64},
    time::{Duration, Instant},
};

const AGENT_ROLES: [&str; 3] = ["ledger.read", "ledger.propose", "ledger.review"];
const ADMIN_ROLES: [&str; 4] = [
    "ledger.read",
    "ledger.propose",
    "ledger.review",
    "ledger.admin",
];
/// Transient-failure retries per request (same key) before the step is reported failed.
const RETRIES: u32 = 200;

struct BranchConfig {
    replicas: Vec<String>,
    owner_database_url: String,
    branches: usize,
    commits_per_branch: u32,
    main_writers: usize,
    main_commits_per_writer: u32,
    max_p99_ms: f64,
    out: String,
    issuer: String,
    audience: String,
    secret: String,
}

fn usage() -> &'static str {
    "usage: ledger-stress branches --replicas <url,url> --out <dir> [--branches 100] \
     [--commits-per-branch 3] [--main-writers 4] [--main-commits-per-writer 5] \
     [--max-p99-ms 5000] [--issuer <iss>] [--audience <aud>] \
     [--secret-env LEDGER_STRESS_HS256_SECRET] [--owner-database-url <url>] [--allow-non-loopback]\n\
     The owner database URL is read from LEDGER_STRESS_OWNER_DATABASE_URL (preferred) or the flag."
}

fn parse(mut argv: impl Iterator<Item = String>) -> Result<BranchConfig, String> {
    let mut c = BranchConfig {
        replicas: Vec::new(),
        owner_database_url: std::env::var("LEDGER_STRESS_OWNER_DATABASE_URL").unwrap_or_default(),
        branches: 100,
        commits_per_branch: 3,
        main_writers: 4,
        main_commits_per_writer: 5,
        max_p99_ms: 5000.0,
        out: String::new(),
        issuer: "https://dev-issuer.example/".into(),
        audience: "api://sculpin-ledger-dev".into(),
        secret: String::new(),
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
            "--branches" => c.branches = value()?.parse().map_err(|_| "--branches")?,
            "--commits-per-branch" => {
                c.commits_per_branch = value()?.parse().map_err(|_| "--commits-per-branch")?
            }
            "--main-writers" => c.main_writers = value()?.parse().map_err(|_| "--main-writers")?,
            "--main-commits-per-writer" => {
                c.main_commits_per_writer =
                    value()?.parse().map_err(|_| "--main-commits-per-writer")?
            }
            "--max-p99-ms" => c.max_p99_ms = value()?.parse().map_err(|_| "--max-p99-ms")?,
            "--out" => c.out = value()?,
            "--issuer" => c.issuer = value()?,
            "--audience" => c.audience = value()?,
            "--secret-env" => secret_env = value()?,
            "--allow-non-loopback" => allow_non_loopback = true,
            _ => return Err(usage().into()),
        }
    }
    c.secret = std::env::var(&secret_env)
        .map_err(|_| format!("{secret_env} must hold the development HS256 secret"))?;
    if c.replicas.len() < 2 {
        return Err("at least two replicas are required (multi-replica gate)".into());
    }
    if c.owner_database_url.is_empty() || c.out.is_empty() || c.branches < 10 {
        return Err(usage().into());
    }
    for r in &c.replicas {
        require_loopback("replica", r, allow_non_loopback)?;
    }
    require_loopback("owner database", &c.owner_database_url, allow_non_loopback)?;
    Ok(c)
}

/// The expected fate of branch `i` (fixed by its index, so the report is reproducible).
fn deleted(i: usize) -> bool {
    i % 5 == 0
}
fn restored(i: usize) -> bool {
    i % 10 == 0
}
fn historical(i: usize) -> bool {
    i % 4 == 0
}
fn duplicated(i: usize) -> bool {
    i % 10 == 3
}

fn branch_name(i: usize) -> String {
    format!("agent/w{i:03}")
}

fn prepare(branch: &str, head: Option<&str>, quad: &str) -> Value {
    json!({
        "ref": branch,
        "expected_head": head,
        "operations": [{"op": "add", "quad": quad}],
        "activity": "branch-stress",
        "event_time": "2026-09-28T12:00:00Z",
        "evidence_refs": ["urn:evidence:branch-stress"],
        "source_system": "ledger-stress",
        "message": "branch stress commit",
    })
}

/// Send one logical request, retrying the *same* key on expected transient failures.
async fn send(api: &Api, c: Call<'_>, duplicate: bool) -> Outcome {
    let mut last = Outcome::Failed("never sent".into());
    for attempt in 0..RETRIES {
        let call = Call {
            op: c.op,
            method: c.method.clone(),
            path: c.path,
            token: c.token,
            key: c.key,
            body: c.body,
        };
        match api.call(call, duplicate).await {
            Outcome::Failed(class) if is_expected_failure(&class) => {
                last = Outcome::Failed(class);
                tokio::time::sleep(Duration::from_millis(20 + jitter(attempt as usize, 1, 200)))
                    .await;
            }
            other => return other,
        }
    }
    last
}

fn code(o: &Outcome) -> String {
    match o {
        Outcome::Ok(_) => "ok".into(),
        Outcome::Conflict(c) | Outcome::Failed(c) => c.clone(),
    }
}

/// One prepare → accept on `branch` from a known head (no contention on a work branch).
async fn land(
    api: &Api,
    graph: &str,
    token: &str,
    branch: &str,
    head: Option<&str>,
    key: &str,
    duplicate: bool,
) -> Result<(String, i64), String> {
    let candidate = prepare_candidate(api, graph, token, branch, head, key, duplicate).await?;
    accept_candidate(api, graph, token, branch, head, key, &candidate, duplicate).await
}

async fn prepare_candidate(
    api: &Api,
    graph: &str,
    token: &str,
    branch: &str,
    head: Option<&str>,
    key: &str,
    duplicate: bool,
) -> Result<String, String> {
    let proposals = format!("/v1/graphs/{graph}/proposals");
    let body = prepare(
        branch,
        head,
        &format!("<urn:branch-stress:{key}> <urn:branch-stress:p> \"{key}\" ."),
    );
    match send(
        api,
        Call {
            op: Op::Prepare,
            method: reqwest::Method::POST,
            path: &proposals,
            token,
            key: Some(&format!("{key}-p")),
            body: Some(&body),
        },
        duplicate,
    )
    .await
    {
        Outcome::Ok(v) => v["candidate"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("prepare {key}: 2xx without candidate")),
        other => Err(format!("prepare {key}: {}", code(&other))),
    }
}

#[allow(clippy::too_many_arguments)]
async fn accept_candidate(
    api: &Api,
    graph: &str,
    token: &str,
    branch: &str,
    head: Option<&str>,
    key: &str,
    candidate: &str,
    duplicate: bool,
) -> Result<(String, i64), String> {
    let accept = format!("/v1/graphs/{graph}/proposals/{candidate}/accept");
    let body = json!({"ref": branch, "expected_head": head, "reason": "branch stress"});
    match send(
        api,
        Call {
            op: Op::Accept,
            method: reqwest::Method::POST,
            path: &accept,
            token,
            key: Some(&format!("{key}-a")),
            body: Some(&body),
        },
        duplicate,
    )
    .await
    {
        Outcome::Ok(v) => {
            let h = v["head"].as_str().unwrap_or("").to_owned();
            let version = v["ref_version"].as_i64().unwrap_or(-1);
            if h != candidate || version < 1 {
                return Err(format!(
                    "accept {key}: candidate {candidate} answered head={h} version={version}"
                ));
            }
            Ok((h, version))
        }
        other => Err(format!("accept {key}: {}", code(&other))),
    }
}

/// `main` writer: read head, prepare, accept; CAS conflicts re-read.
async fn main_writer(
    api: Arc<Api>,
    graph: String,
    token: String,
    index: usize,
    target: u32,
) -> (Vec<(String, i64, String)>, Vec<String>) {
    let refs = format!("/v1/graphs/{graph}/refs?name=main");
    let (mut landed, mut failures, mut sequence) = (Vec::new(), Vec::new(), 0u64);
    while (landed.len() as u32) < target && sequence < 5000 {
        sequence += 1;
        let head = match send(
            &api,
            Call {
                op: Op::RefRead,
                method: reqwest::Method::GET,
                path: &refs,
                token: &token,
                key: None,
                body: None,
            },
            false,
        )
        .await
        {
            Outcome::Ok(v) => v["head"].as_str().map(str::to_owned),
            other => {
                failures.push(format!("main read: {}", code(&other)));
                return (landed, failures);
            }
        };
        match land(
            &api,
            &graph,
            &token,
            "main",
            head.as_deref(),
            &format!("main-{index}-{sequence}"),
            false,
        )
        .await
        {
            Ok((h, v)) => landed.push((graph.clone(), v, h)),
            Err(e) if e.ends_with("HEAD_CHANGED") || e.ends_with("LINEAGE_MISMATCH") => {
                tokio::time::sleep(Duration::from_millis(jitter(index, sequence, 30))).await;
            }
            Err(e) => {
                failures.push(e);
                return (landed, failures);
            }
        }
    }
    if (landed.len() as u32) < target {
        failures.push(format!(
            "main writer {index} gave up after {sequence} attempts"
        ));
    }
    (landed, failures)
}

/// What one agent observed on its branch.
#[derive(Serialize, Clone, Default)]
struct AgentLog {
    index: usize,
    branch: String,
    /// Head the creation answered (the branch point).
    created_at: String,
    /// `main`'s version read just before the create request: a head-created branch must
    /// start at this version or a later one (never at an older head).
    main_version_before: i64,
    /// Client-observed landings `(version, head)` on the branch.
    landings: Vec<(i64, String)>,
    head: String,
    /// Race outcome of the acceptance raced against deletion: `landed` / `refused`.
    race: Option<String>,
    /// Head the tombstone recorded.
    tombstone_head: Option<String>,
    failures: Vec<String>,
}

async fn agent(
    api: Arc<Api>,
    graph: String,
    token: String,
    i: usize,
    root: String,
    commits: u32,
) -> AgentLog {
    let branch = branch_name(i);
    let mut log = AgentLog {
        index: i,
        branch: branch.clone(),
        ..AgentLog::default()
    };
    let path = format!("/v1/graphs/{graph}/branches");
    let refs = format!("/v1/graphs/{graph}/refs?name=main");
    match send(
        &api,
        Call {
            op: Op::RefRead,
            method: reqwest::Method::GET,
            path: &refs,
            token: &token,
            key: None,
            body: None,
        },
        false,
    )
    .await
    {
        Outcome::Ok(v) => log.main_version_before = v["version"].as_i64().unwrap_or(i64::MAX),
        other => {
            log.failures.push(format!("main read: {}", code(&other)));
            return log;
        }
    }
    let body = if historical(i) {
        json!({"name": branch, "source": "main", "from_commit": root})
    } else {
        json!({"name": branch, "source": "main"})
    };
    match send(
        &api,
        Call {
            op: Op::BranchCreate,
            method: reqwest::Method::POST,
            path: &path,
            token: &token,
            key: Some(&format!("create-{i}")),
            body: Some(&body),
        },
        duplicated(i),
    )
    .await
    {
        Outcome::Ok(v) => {
            log.created_at = v["event"]["head"].as_str().unwrap_or("").to_owned();
            if v["event"]["version"] != json!(1) || v["event"]["operation"] != json!("created") {
                log.failures
                    .push(format!("create {branch}: unexpected event {v}"));
            }
        }
        other => {
            log.failures
                .push(format!("create {branch}: {}", code(&other)));
            return log;
        }
    }
    let mut head = log.created_at.clone();
    for n in 0..commits {
        match land(
            &api,
            &graph,
            &token,
            &branch,
            Some(&head),
            &format!("b{i}-{n}"),
            duplicated(i),
        )
        .await
        {
            Ok((h, v)) => {
                log.landings.push((v, h.clone()));
                head = h;
            }
            Err(e) => {
                log.failures.push(e);
                break;
            }
        }
    }
    log.head = head;
    log
}

/// Lifecycle race on a branch due for deletion: admin delete vs one more acceptance.
async fn race_delete(
    api: Arc<Api>,
    graph: String,
    agent_token: String,
    admin_token: String,
    mut log: AgentLog,
) -> AgentLog {
    let i = log.index;
    let branch = log.branch.clone();
    let delete_path = format!("/v1/graphs/{graph}/branches/delete");
    let delete_body = json!({"name": branch, "reason": "branch stress race"});
    let head = log.head.clone();
    let (delete_key, race_key) = (format!("delete-{i}"), format!("race-{i}"));
    let delete = send(
        &api,
        Call {
            op: Op::BranchDelete,
            method: reqwest::Method::POST,
            path: &delete_path,
            token: &admin_token,
            key: Some(&delete_key),
            body: Some(&delete_body),
        },
        true,
    );
    // Prepare first: only the acceptance races the tombstone.
    let candidate = match prepare_candidate(
        &api,
        &graph,
        &agent_token,
        &branch,
        Some(&head),
        &race_key,
        false,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            log.failures.push(format!("race prepare: {e}"));
            return log;
        }
    };
    let accept = accept_candidate(
        &api,
        &graph,
        &agent_token,
        &branch,
        Some(&head),
        &race_key,
        &candidate,
        false,
    );
    let (deleted_outcome, accepted) = tokio::join!(delete, accept);
    match &deleted_outcome {
        Outcome::Ok(v) => {
            log.tombstone_head = v["event"]["head"].as_str().map(str::to_owned);
            if v["event"]["status"] != json!("deleted") {
                log.failures.push(format!("delete {branch}: {v}"));
            }
        }
        other => log
            .failures
            .push(format!("delete {branch}: {}", code(other))),
    }
    match accepted {
        Ok((h, v)) => {
            log.race = Some("landed".into());
            log.landings.push((v, h.clone()));
            log.head = h;
        }
        Err(e) if e.ends_with("409 BRANCH_DELETED") => log.race = Some("refused".into()),
        Err(e) => log.failures.push(format!("race accept: {e}")),
    }
    // A prepare on the tombstoned branch is refused.
    let proposals = format!("/v1/graphs/{graph}/proposals");
    let body = prepare(
        &branch,
        Some(&log.head),
        "<urn:x> <urn:y> \"after delete\" .",
    );
    let after = send(
        &api,
        Call {
            op: Op::Prepare,
            method: reqwest::Method::POST,
            path: &proposals,
            token: &agent_token,
            key: Some(&format!("after-delete-{i}")),
            body: Some(&body),
        },
        false,
    )
    .await;
    if code(&after) != "409 BRANCH_DELETED" {
        log.failures
            .push(format!("prepare on deleted {branch}: {}", code(&after)));
    }
    log
}

async fn restore(
    api: Arc<Api>,
    graph: String,
    agent_token: String,
    admin_token: String,
    mut log: AgentLog,
) -> AgentLog {
    let i = log.index;
    let branch = log.branch.clone();
    let path = format!("/v1/graphs/{graph}/branches/restore");
    let body = json!({"name": branch});
    match send(
        &api,
        Call {
            op: Op::BranchRestore,
            method: reqwest::Method::POST,
            path: &path,
            token: &admin_token,
            key: Some(&format!("restore-{i}")),
            body: Some(&body),
        },
        true,
    )
    .await
    {
        Outcome::Ok(v) => {
            if v["event"]["head"].as_str() != Some(log.head.as_str()) {
                log.failures
                    .push(format!("restore {branch}: head moved {v} vs {}", log.head));
            }
        }
        other => log
            .failures
            .push(format!("restore {branch}: {}", code(&other))),
    }
    let again = send(
        &api,
        Call {
            op: Op::BranchRestore,
            method: reqwest::Method::POST,
            path: &path,
            token: &admin_token,
            key: Some(&format!("restore-again-{i}")),
            body: Some(&body),
        },
        false,
    )
    .await;
    if code(&again) != "409 BRANCH_STATE_CONFLICT" {
        log.failures
            .push(format!("second restore {branch}: {}", code(&again)));
    }
    let head = log.head.clone();
    match land(
        &api,
        &graph,
        &agent_token,
        &branch,
        Some(&head),
        &format!("after-restore-{i}"),
        false,
    )
    .await
    {
        Ok((h, v)) => {
            log.landings.push((v, h.clone()));
            log.head = h;
        }
        Err(e) => log.failures.push(e),
    }
    log
}

#[derive(Serialize)]
struct BranchInvariants {
    branch: String,
    expected_status: &'static str,
    status: String,
    ref_version: i64,
    ref_events: i64,
    client_landings: i64,
    landings_matching_events: i64,
    lifecycle_version: i64,
    lifecycle_events: i64,
    outbox_rows: i64,
    creation_head: String,
    ok: bool,
    why: Vec<String>,
}

async fn branch_invariants(
    pool: &PgPool,
    graph: &str,
    log: &AgentLog,
    root: &str,
    main_versions: &BTreeMap<String, i64>,
) -> BranchInvariants {
    let b = log.branch.as_str();
    let row = sqlx::query(
        "SELECT r.version, r.head, br.status, br.lifecycle_version, br.origin, br.source_commit, \
           (SELECT count(*) FROM ref_events WHERE graph_id = $1 AND branch = $2) AS events, \
           (SELECT count(*) FROM branch_events WHERE graph_id = $1 AND branch = $2) AS lifecycle, \
           (SELECT count(*) FROM projection_outbox WHERE graph_id = $1 AND branch = $2) AS outbox, \
           (SELECT new_head FROM ref_events WHERE graph_id = $1 AND branch = $2 AND new_version = 1) AS created_head, \
           (SELECT head FROM branch_events WHERE graph_id = $1 AND branch = $2 AND operation = 'deleted' \
              ORDER BY event_id LIMIT 1) AS tombstone_head \
         FROM refs r JOIN branches br USING (graph_id, branch) WHERE r.graph_id = $1 AND r.branch = $2",
    )
    .bind(graph)
    .bind(b)
    .fetch_one(pool)
    .await
    .expect("branch invariant query");
    let (versions, heads): (Vec<i64>, Vec<String>) = log.landings.iter().cloned().unzip();
    let matching: i64 = sqlx::query(
        "SELECT count(*) AS n FROM unnest($3::bigint[], $4::text[]) AS c(version, head) \
         JOIN ref_events e ON e.graph_id = $1 AND e.branch = $2 \
           AND e.new_version = c.version AND e.new_head = c.head",
    )
    .bind(graph)
    .bind(b)
    .bind(&versions)
    .bind(&heads)
    .fetch_one(pool)
    .await
    .expect("landing query")
    .get("n");
    let i = log.index;
    let expected_status = if deleted(i) && !restored(i) {
        "deleted"
    } else {
        "active"
    };
    let expected_lifecycle = 1 + i64::from(deleted(i)) + i64::from(restored(i));
    let status: String = row.get("status");
    let ref_version: i64 = row.get("version");
    let ref_events: i64 = row.get("events");
    let lifecycle_version: i64 = row.get("lifecycle_version");
    let lifecycle_events: i64 = row.get("lifecycle");
    let outbox_rows: i64 = row.get("outbox");
    let creation_head: String = row.get("created_head");
    let head: String = row.get("head");
    let origin: String = row.get("origin");
    let source_commit: Option<String> = row.get("source_commit");
    let tombstone_head: Option<String> = row.get("tombstone_head");
    let client_landings = log.landings.len() as i64;
    let mut why = Vec::new();
    let mut check = |cond: bool, what: String| {
        if !cond {
            why.push(what);
        }
    };
    check(status == expected_status, format!("status {status}"));
    check(
        ref_version == ref_events && ref_events == 1 + client_landings,
        format!("version {ref_version} events {ref_events} landings {client_landings}"),
    );
    check(
        matching == client_landings,
        format!("{matching} landings match events"),
    );
    check(
        lifecycle_version == lifecycle_events && lifecycle_events == expected_lifecycle,
        format!("lifecycle {lifecycle_version}/{lifecycle_events} expected {expected_lifecycle}"),
    );
    check(
        outbox_rows == ref_events - 1,
        format!("outbox {outbox_rows}"),
    );
    check(
        head == log.head,
        format!("head {head} vs client {}", log.head),
    );
    check(
        origin == "created" && source_commit.as_deref() == Some(creation_head.as_str()),
        format!("origin {origin} source_commit {source_commit:?}"),
    );
    check(
        creation_head == log.created_at
            && if historical(i) {
                creation_head == root
            } else {
                main_versions
                    .get(&creation_head)
                    .is_some_and(|v| *v >= log.main_version_before)
            },
        format!(
            "branch point {creation_head} (main v{:?}, read v{} before create)",
            main_versions.get(&creation_head),
            log.main_version_before
        ),
    );
    if deleted(i) {
        // The tombstone names the head the branch stopped at: after a landed race
        // acceptance, the new head; after a refused one, the head before it.
        let stopped_at = if restored(i) {
            log.landings
                .iter()
                .rev()
                .nth(1)
                .map(|(_, h)| h.clone())
                .unwrap_or_else(|| log.created_at.clone())
        } else {
            log.head.clone()
        };
        check(
            tombstone_head.as_deref() == Some(stopped_at.as_str())
                && log.tombstone_head.as_deref() == Some(stopped_at.as_str()),
            format!(
                "tombstone {tombstone_head:?} client {:?} expected {stopped_at}",
                log.tombstone_head
            ),
        );
    }
    check(
        log.failures.is_empty(),
        format!("client failures {:?}", log.failures),
    );
    BranchInvariants {
        branch: log.branch.clone(),
        expected_status,
        status,
        ref_version,
        ref_events,
        client_landings,
        landings_matching_events: matching,
        lifecycle_version,
        lifecycle_events,
        outbox_rows,
        creation_head,
        ok: why.is_empty(),
        why,
    }
}

#[derive(Serialize)]
struct BranchReport {
    run: String,
    started_utc_epoch: u64,
    environment: crate::Environment,
    graph: String,
    branches: usize,
    commits_per_branch: u32,
    main_writers: usize,
    wall_seconds: f64,
    requests: u64,
    races_landed: usize,
    races_refused: usize,
    listed_branches: usize,
    latency: Vec<crate::Percentiles>,
    ok_p99_ms: BTreeMap<&'static str, f64>,
    max_p99_ms: f64,
    errors: BTreeMap<String, u64>,
    unexpected_errors: BTreeMap<String, u64>,
    pairs: crate::PairStats,
    deadlocks_before: Option<i64>,
    deadlocks_after: Option<i64>,
    main: crate::GraphInvariants,
    unconfigured_pending: i64,
    main_outbox_undelivered: i64,
    branch_invariants_failed: Vec<BranchInvariants>,
    failures: Vec<String>,
    verify: VerifyReport,
    passed: bool,
}

fn markdown(r: &BranchReport) -> String {
    let mut m = format!(
        "# Branch stress `{}` — {}\n\n{} branches × {} commits + {} `main` writers on graph `{}` \
         across {} replicas; {:.1} s wall, {} requests.\n\n",
        r.run,
        if r.passed { "PASS" } else { "FAIL" },
        r.branches,
        r.commits_per_branch,
        r.main_writers,
        r.graph,
        r.environment.replicas.len(),
        r.wall_seconds,
        r.requests
    );
    m.push_str(&format!(
        "- delete-vs-accept races: {} acceptances landed before the tombstone, {} refused `BRANCH_DELETED`\n\
         - list returned {} branches; `main` at v{} ({} ref events, invariants {})\n\
         - unconfigured projection backlog {} = `main` undelivered {}\n\
         - duplicated pairs: {} compared, {} disagreements\n\
         - deadlocks {:?} → {:?}; verifier clean: {}\n\
         - errors {:?}; unexpected {:?}\n\n",
        r.races_landed,
        r.races_refused,
        r.listed_branches,
        r.main.ref_version,
        r.main.ref_events,
        if r.main.ok { "ok" } else { "FAILED" },
        r.unconfigured_pending,
        r.main_outbox_undelivered,
        r.pairs.compared,
        r.pairs.disagreements.len(),
        r.deadlocks_before,
        r.deadlocks_after,
        r.verify.clean,
        r.errors,
        r.unexpected_errors,
    ));
    m.push_str(
        "| op | bucket | n | p50 ms | p95 ms | p99 ms | max ms |\n|---|---|---|---|---|---|---|\n",
    );
    for p in &r.latency {
        m.push_str(&format!(
            "| {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} |\n",
            p.op, p.bucket, p.count, p.p50_ms, p.p95_ms, p.p99_ms, p.max_ms
        ));
    }
    if !r.failures.is_empty() {
        m.push_str("\n## Failures\n");
        for f in &r.failures {
            m.push_str(&format!("- {f}\n"));
        }
    }
    m
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
    let run = format!("{:x}b", now_secs());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&cfg.owner_database_url)
        .await
        .expect("owner database connection");
    let http = reqwest::Client::builder()
        .no_proxy()
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
    let environment = environment(&pool, &cfg.replicas).await;
    let graph = provision(&pool, &run, 1).await.remove(0);
    let token = |subject: &str, roles: &[&str]| {
        mint_roles(
            &cfg.issuer,
            &cfg.audience,
            &cfg.secret,
            subject,
            3600,
            roles,
        )
    };
    let admin = token("branch-stress-admin", &ADMIN_ROLES);
    let seeder = token("branch-stress-seeder", &AGENT_ROLES);
    let mut failures = Vec::new();
    let deadlocks_before = crate::deadlocks(&pool).await;
    let started = Instant::now();

    // 1. Seed main: root (historical branch point) and head.
    let mut main_landed = Vec::new();
    let root = match land(&api, &graph, &seeder, "main", None, "seed-0", false).await {
        Ok((h, v)) => {
            main_landed.push((graph.clone(), v, h.clone()));
            h
        }
        Err(e) => {
            eprintln!("seed: {e}");
            return ExitCode::FAILURE;
        }
    };
    match land(&api, &graph, &seeder, "main", Some(&root), "seed-1", false).await {
        Ok((h, v)) => main_landed.push((graph.clone(), v, h)),
        Err(e) => {
            eprintln!("seed: {e}");
            return ExitCode::FAILURE;
        }
    }
    eprintln!(
        "run {run}: graph {graph}, {} branches × {} commits, {} main writers × {}, {} replicas",
        cfg.branches,
        cfg.commits_per_branch,
        cfg.main_writers,
        cfg.main_commits_per_writer,
        cfg.replicas.len()
    );

    // 2. Branch agents and main writers, concurrently.
    let mut agents = Vec::new();
    for i in 0..cfg.branches {
        agents.push(tokio::spawn(agent(
            api.clone(),
            graph.clone(),
            token(&format!("branch-agent-{i}"), &AGENT_ROLES),
            i,
            root.clone(),
            cfg.commits_per_branch,
        )));
    }
    let mut writers = Vec::new();
    for w in 0..cfg.main_writers {
        writers.push(tokio::spawn(main_writer(
            api.clone(),
            graph.clone(),
            token(&format!("main-writer-{w}"), &AGENT_ROLES),
            w,
            cfg.main_commits_per_writer,
        )));
    }
    let mut logs = Vec::new();
    for a in agents {
        logs.push(a.await.expect("agent task"));
    }
    for w in writers {
        let (landed, f) = w.await.expect("main writer task");
        main_landed.extend(landed);
        failures.extend(f);
    }
    eprintln!("created and advanced {} branches", logs.len());

    // 3. Lifecycle races, then restores.
    let mut tasks = Vec::new();
    for log in logs.drain(..) {
        let i = log.index;
        let agent_token = token(&format!("branch-agent-{i}"), &AGENT_ROLES);
        if deleted(i) && log.failures.is_empty() {
            tasks.push(tokio::spawn(race_delete(
                api.clone(),
                graph.clone(),
                agent_token,
                admin.clone(),
                log,
            )));
        } else {
            tasks.push(tokio::spawn(async move { log }));
        }
    }
    for t in tasks {
        logs.push(t.await.expect("race task"));
    }
    let mut tasks = Vec::new();
    for log in logs.drain(..) {
        let i = log.index;
        let agent_token = token(&format!("branch-agent-{i}"), &AGENT_ROLES);
        if restored(i) && log.failures.is_empty() {
            tasks.push(tokio::spawn(restore(
                api.clone(),
                graph.clone(),
                agent_token,
                admin.clone(),
                log,
            )));
        } else {
            tasks.push(tokio::spawn(async move { log }));
        }
    }
    for t in tasks {
        logs.push(t.await.expect("restore task"));
    }

    // 4. Reads across replicas.
    let list_path = format!("/v1/graphs/{graph}/branches?limit=1000");
    let listed_branches = match send(
        &api,
        Call {
            op: Op::BranchRead,
            method: reqwest::Method::GET,
            path: &list_path,
            token: &seeder,
            key: None,
            body: None,
        },
        false,
    )
    .await
    {
        Outcome::Ok(v) => v["branches"].as_array().map_or(0, Vec::len),
        other => {
            failures.push(format!("list: {}", code(&other)));
            0
        }
    };
    if listed_branches != cfg.branches + 1 {
        failures.push(format!(
            "list returned {listed_branches} branches, expected {}",
            cfg.branches + 1
        ));
    }
    let mut reads = Vec::new();
    for log in &logs {
        let (api, token, graph, log) = (api.clone(), seeder.clone(), graph.clone(), log.clone());
        reads.push(tokio::spawn(async move {
            let mut f = Vec::new();
            for what in ["status", "history"] {
                let path = format!("/v1/graphs/{graph}/branches/{what}?name={}", log.branch);
                match send(
                    &api,
                    Call {
                        op: Op::BranchRead,
                        method: reqwest::Method::GET,
                        path: &path,
                        token: &token,
                        key: None,
                        body: None,
                    },
                    false,
                )
                .await
                {
                    Outcome::Ok(v) if what == "status" => {
                        if v["head"].as_str() != Some(log.head.as_str()) {
                            f.push(format!(
                                "status {}: head {} vs {}",
                                log.branch, v["head"], log.head
                            ));
                        }
                    }
                    Outcome::Ok(v) => {
                        let n = v["movements"].as_array().map_or(0, Vec::len);
                        if n != 1 + log.landings.len() {
                            f.push(format!("history {}: {n} movements", log.branch));
                        }
                    }
                    other => f.push(format!("{what} {}: {}", log.branch, code(&other))),
                }
            }
            f
        }));
    }
    for r in reads {
        failures.extend(r.await.expect("read task"));
    }
    let wall_seconds = started.elapsed().as_secs_f64();

    // 5. Invariants under the owner identity.
    tokio::time::sleep(crate::STATS_FLUSH_WAIT).await;
    let deadlocks_after = crate::deadlocks(&pool).await;
    let main = graph_invariants(&pool, &graph, &main_landed).await;
    if !main.ok {
        failures.push("main graph invariants".into());
    }
    let main_versions: BTreeMap<String, i64> = sqlx::query_as::<_, (String, i64)>(
        "SELECT new_head, new_version FROM ref_events WHERE graph_id = $1 AND branch = 'main'",
    )
    .bind(&graph)
    .fetch_all(&pool)
    .await
    .expect("main heads")
    .into_iter()
    .collect();
    let mut branch_invariants_failed = Vec::new();
    for log in &logs {
        let inv = branch_invariants(&pool, &graph, log, &root, &main_versions).await;
        if !inv.ok {
            branch_invariants_failed.push(inv);
        }
    }
    if !branch_invariants_failed.is_empty() {
        failures.push(format!(
            "{} branches failed their invariants",
            branch_invariants_failed.len()
        ));
    }
    let unconfigured_pending = ledger_store::ProjectionRepository::new(pool.clone())
        .unconfigured_pending()
        .await
        .expect("unconfigured backlog");
    let main_outbox_undelivered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM projection_outbox WHERE branch = 'main' AND delivered_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("main outbox");
    if unconfigured_pending != main_outbox_undelivered {
        failures.push(format!(
            "unconfigured backlog {unconfigured_pending} != main undelivered {main_outbox_undelivered}"
        ));
    }
    if deadlocks_before.is_none() || deadlocks_before != deadlocks_after {
        failures.push(format!(
            "deadlocks {deadlocks_before:?} -> {deadlocks_after:?}"
        ));
    }
    let races_landed = logs
        .iter()
        .filter(|l| l.race.as_deref() == Some("landed"))
        .count();
    let races_refused = logs
        .iter()
        .filter(|l| l.race.as_deref() == Some("refused"))
        .count();
    let expected_races = (0..cfg.branches).filter(|i| deleted(*i)).count();
    if races_landed + races_refused != expected_races {
        failures.push(format!(
            "{} of {expected_races} delete races resolved",
            races_landed + races_refused
        ));
    }
    let errors = metrics.errors.lock().unwrap().clone();
    // BRANCH_DELETED (refused race acceptances and prepares on tombstones) and
    // BRANCH_STATE_CONFLICT (the deliberate second restore) are asserted per branch above.
    let unexpected_errors: BTreeMap<String, u64> = errors
        .iter()
        .filter(|(class, _)| {
            !(is_expected_failure(class)
                || class.as_str() == "409 BRANCH_DELETED"
                || class.as_str() == "409 BRANCH_STATE_CONFLICT")
                || class.as_str() == "503 DEPENDENCY_TIMEOUT"
        })
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    if !unexpected_errors.is_empty() {
        failures.push(format!("unexpected error classes {unexpected_errors:?}"));
    }
    let pairs = metrics.pairs.lock().unwrap().clone();
    if !pairs.disagreements.is_empty() {
        failures.push(format!("{} pair disagreements", pairs.disagreements.len()));
    }
    let mut latency = Vec::new();
    let mut ok_p99_ms = BTreeMap::new();
    for ((op, bucket), samples) in metrics.latency.lock().unwrap().iter_mut() {
        let p = percentiles(op, bucket, samples);
        if *bucket == "ok" {
            ok_p99_ms.insert(*op, p.p99_ms);
            if p.p99_ms > cfg.max_p99_ms {
                failures.push(format!(
                    "{op} ok p99 {:.1} ms > {} ms",
                    p.p99_ms, cfg.max_p99_ms
                ));
            }
        }
        latency.push(p);
    }
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
    if !verify.clean {
        failures.push("verifier reported violations".into());
    }
    for log in &logs {
        for f in &log.failures {
            failures.push(format!("{}: {f}", log.branch));
        }
    }
    let report = BranchReport {
        run,
        started_utc_epoch: now_secs(),
        environment,
        graph,
        branches: cfg.branches,
        commits_per_branch: cfg.commits_per_branch,
        main_writers: cfg.main_writers,
        wall_seconds,
        requests: metrics.requests.load(std::sync::atomic::Ordering::Relaxed),
        races_landed,
        races_refused,
        listed_branches,
        latency,
        ok_p99_ms,
        max_p99_ms: cfg.max_p99_ms,
        errors,
        unexpected_errors,
        pairs,
        deadlocks_before,
        deadlocks_after,
        main,
        unconfigured_pending,
        main_outbox_undelivered,
        branch_invariants_failed,
        passed: failures.is_empty(),
        failures,
        verify,
    };
    std::fs::write(
        format!("{}/report.json", cfg.out),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .expect("write report.json");
    let md = markdown(&report);
    std::fs::write(format!("{}/report.md", cfg.out), &md).expect("write report.md");
    println!("{md}");
    if report.passed {
        println!("BRANCH STRESS OK");
        ExitCode::SUCCESS
    } else {
        println!("BRANCH STRESS FAILED");
        ExitCode::FAILURE
    }
}
