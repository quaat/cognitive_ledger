//! Plan 0005 §10: reproducible single-client performance baselines.
//!
//! `ledger-stress bench …` builds one linear history on one graph through the public API
//! and measures, at chain depths 1 / 100 / 1,000 / 10,000 (configurable), the latency of
//! `prepare`, `accept`, a ref read and a state read at the head (each `samples` times, one
//! client, no concurrency), plus the state size. The numbers feed
//! `docs/quality/performance-baselines.md` and the checkpoint decision (Phase 4/5 input):
//! prepare reconstructs the parent state, so its cost grows with depth until checkpoints
//! exist. Development/qualification tooling only.

use crate::{
    Api, BRANCH, Call, Metrics, Op, Outcome, environment, mint_claims_ttl, now_secs, percentiles,
    prepare_body, provision, require_loopback,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, atomic::AtomicU64},
    time::{Duration, Instant},
};

struct BenchConfig {
    replica: String,
    owner_database_url: String,
    depths: Vec<u64>,
    samples: usize,
    out: PathBuf,
    issuer: String,
    audience: String,
    secret: String,
}

fn usage() -> &'static str {
    "usage: ledger-stress bench --replica <url> --out <dir> [--depths 1,100,1000,10000] \
     [--samples 20] [--issuer <iss>] [--audience <aud>] [--secret-env LEDGER_STRESS_HS256_SECRET] \
     [--owner-database-url <url>] [--allow-non-loopback]"
}

fn parse(mut argv: impl Iterator<Item = String>) -> Result<BenchConfig, String> {
    let mut c = BenchConfig {
        replica: String::new(),
        owner_database_url: std::env::var("LEDGER_STRESS_OWNER_DATABASE_URL").unwrap_or_default(),
        depths: vec![1, 100, 1000, 10_000],
        samples: 20,
        out: PathBuf::new(),
        issuer: "https://dev-issuer.example/".into(),
        audience: "api://sculpin-ledger-dev".into(),
        secret: String::new(),
    };
    let mut secret_env = "LEDGER_STRESS_HS256_SECRET".to_owned();
    let mut allow = false;
    while let Some(flag) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--replica" => c.replica = value()?.trim_end_matches('/').to_owned(),
            "--owner-database-url" => c.owner_database_url = value()?,
            "--depths" => {
                c.depths = value()?
                    .split(',')
                    .map(|d| d.trim().parse::<u64>().map_err(|_| "--depths"))
                    .collect::<Result<_, _>>()?;
                c.depths.sort_unstable();
                c.depths.dedup();
            }
            "--samples" => c.samples = value()?.parse().map_err(|_| "--samples")?,
            "--out" => c.out = value()?.into(),
            "--issuer" => c.issuer = value()?,
            "--audience" => c.audience = value()?,
            "--secret-env" => secret_env = value()?,
            "--allow-non-loopback" => allow = true,
            _ => return Err(usage().into()),
        }
    }
    c.secret = std::env::var(&secret_env)
        .map_err(|_| format!("{secret_env} must hold the development HS256 secret"))?;
    if c.replica.is_empty()
        || c.owner_database_url.is_empty()
        || c.out.as_os_str().is_empty()
        || c.depths.is_empty()
        || c.samples == 0
    {
        return Err(usage().into());
    }
    require_loopback("replica", &c.replica, allow)?;
    require_loopback("owner database", &c.owner_database_url, allow)?;
    Ok(c)
}

#[derive(Serialize, Clone)]
struct DepthReport {
    depth: u64,
    quads_in_state: usize,
    prepare: crate::Percentiles,
    accept: crate::Percentiles,
    ref_read: crate::Percentiles,
    state_read: crate::Percentiles,
}

#[derive(Serialize)]
struct BenchReport {
    run: String,
    environment: crate::Environment,
    samples: usize,
    depths: Vec<DepthReport>,
    total_commits: u64,
    build_seconds: f64,
    errors: Vec<String>,
    passed: bool,
}

fn markdown(r: &BenchReport) -> String {
    let mut m = format!(
        "# Performance baseline `{}` — {}\n\nSingle client, one graph, linear history of {} commits (one quad added per commit), {} samples per cell; {:.1} s to build. Hardware: {} × {}, {} GiB RAM, kernel {}. PostgreSQL: {}. Replica: {}.\n\n| depth | quads in state | prepare p50 / p95 ms | accept p50 / p95 ms | ref read p50 / p95 ms | state read p50 / p95 ms |\n|---:|---:|---:|---:|---:|---:|\n",
        r.run,
        if r.passed { "complete" } else { "INCOMPLETE" },
        r.total_commits,
        r.samples,
        r.build_seconds,
        r.environment.cpus,
        r.environment.cpu_model,
        r.environment.mem_total_kib / 1024 / 1024,
        r.environment.kernel,
        r.environment.postgres_version,
        r.environment.replicas.join(", ")
    );
    for d in &r.depths {
        m.push_str(&format!(
            "| {} | {} | {:.1} / {:.1} | {:.1} / {:.1} | {:.1} / {:.1} | {:.1} / {:.1} |\n",
            d.depth,
            d.quads_in_state,
            d.prepare.p50_ms,
            d.prepare.p95_ms,
            d.accept.p50_ms,
            d.accept.p95_ms,
            d.ref_read.p50_ms,
            d.ref_read.p95_ms,
            d.state_read.p50_ms,
            d.state_read.p95_ms
        ));
    }
    for e in &r.errors {
        m.push_str(&format!("\n- ERROR: {e}"));
    }
    m
}

/// One measuring client: every request is timed individually.
struct Client<'a> {
    api: &'a Api,
    token: &'a str,
    proposals: &'a str,
    refs: &'a str,
    run: &'a str,
}

impl Client<'_> {
    async fn send(
        &self,
        op: Op,
        method: reqwest::Method,
        path: String,
        key: Option<String>,
        body: Option<Value>,
    ) -> (Outcome, Duration) {
        let t = Instant::now();
        let out = self
            .api
            .send(
                0,
                &Call {
                    op,
                    method,
                    path: &path,
                    token: self.token,
                    key: key.as_deref(),
                    body: body.as_ref(),
                },
            )
            .await;
        (out, t.elapsed())
    }

    /// One commit (prepare + accept on the current head); returns both latencies.
    async fn commit(
        &self,
        head: &mut Option<String>,
        depth: &mut u64,
    ) -> Result<(Duration, Duration), String> {
        let _ = self.refs;
        let seq = *depth + 1;
        let quad = format!("<urn:bench:{}:{seq}> <urn:bench:seq> \"{seq}\" .", self.run);
        let (p, p_ms) = self
            .send(
                Op::Prepare,
                reqwest::Method::POST,
                self.proposals.to_owned(),
                Some(format!("{}-p{seq}", self.run)),
                Some(prepare_body(head.as_deref(), &quad)),
            )
            .await;
        let candidate = match p {
            Outcome::Ok(v) => v["candidate"].as_str().unwrap_or("").to_owned(),
            other => return Err(format!("prepare at depth {seq}: {other:?}")),
        };
        let (a, a_ms) = self
            .send(
                Op::Accept,
                reqwest::Method::POST,
                format!("{}/{candidate}/accept", self.proposals),
                Some(format!("{}-a{seq}", self.run)),
                Some(json!({"ref": BRANCH, "expected_head": head, "reason": "bench"})),
            )
            .await;
        match a {
            Outcome::Ok(v) if v["head"].as_str() == Some(candidate.as_str()) => {
                *head = Some(candidate);
                *depth = seq;
                Ok((p_ms, a_ms))
            }
            other => Err(format!("accept at depth {seq}: {other:?}")),
        }
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
    let run = format!("b{:x}", now_secs());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&cfg.owner_database_url)
        .await
        .expect("owner database connection");
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    let metrics = Arc::new(Metrics::default());
    let api = Api {
        http,
        replicas: vec![cfg.replica.clone()],
        next: AtomicU64::new(0),
        metrics,
    };
    let env = environment(&pool, std::slice::from_ref(&cfg.replica)).await;
    let graph = provision(&pool, &run, 1).await.remove(0);
    // Building a 10,000-deep history takes hours (prepare reconstructs the parent state):
    // the client token must outlive the run.
    let token = mint_claims_ttl(
        &cfg.issuer,
        &cfg.audience,
        &cfg.secret,
        "bench-client",
        12 * 3600,
    );
    let refs = format!("/v1/graphs/{graph}/refs?name={BRANCH}");
    let proposals = format!("/v1/graphs/{graph}/proposals");
    let mut errors = Vec::new();
    let mut reports = Vec::new();
    let mut head: Option<String> = None;
    let mut depth: u64 = 0;
    let started = Instant::now();
    let client = Client {
        api: &api,
        token: &token,
        proposals: &proposals,
        refs: &refs,
        run: &run,
    };
    'outer: for target in cfg.depths.clone() {
        // Build up to (target - 1) unmeasured, then measure `samples` commits that land at
        // depths target .. target + samples - 1 (the first one is exactly `target`).
        while depth + 1 < target {
            if let Err(e) = client.commit(&mut head, &mut depth).await {
                errors.push(e);
                break 'outer;
            }
        }
        let mut p_lat = Vec::new();
        let mut a_lat = Vec::new();
        for _ in 0..cfg.samples {
            match client.commit(&mut head, &mut depth).await {
                Ok((p, a)) => {
                    p_lat.push(p.as_micros() as u64);
                    a_lat.push(a.as_micros() as u64);
                }
                Err(e) => {
                    errors.push(e);
                    break 'outer;
                }
            }
        }
        let head_id = head.clone().unwrap_or_default();
        let mut r_lat = Vec::new();
        let mut s_lat = Vec::new();
        let mut quads = 0usize;
        for _ in 0..cfg.samples {
            let (r, r_ms) = client
                .send(Op::RefRead, reqwest::Method::GET, refs.clone(), None, None)
                .await;
            if !matches!(r, Outcome::Ok(_)) {
                errors.push(format!("ref read at depth {depth}: {r:?}"));
                break 'outer;
            }
            r_lat.push(r_ms.as_micros() as u64);
            let (s, s_ms) = client
                .send(
                    Op::RefRead,
                    reqwest::Method::GET,
                    format!("/v1/graphs/{graph}/commits/{head_id}/state"),
                    None,
                    None,
                )
                .await;
            match s {
                Outcome::Ok(v) => quads = v["quads"].as_array().map(Vec::len).unwrap_or(0),
                other => {
                    errors.push(format!("state read at depth {depth}: {other:?}"));
                    break 'outer;
                }
            }
            s_lat.push(s_ms.as_micros() as u64);
        }
        eprintln!(
            "depth {target}: {} quads, prepare p50 {:.1} ms, state read p50 {:.1} ms",
            quads,
            percentiles("prepare", "ok", &mut p_lat.clone()).p50_ms,
            percentiles("state", "ok", &mut s_lat.clone()).p50_ms
        );
        reports.push(DepthReport {
            depth: target,
            quads_in_state: quads,
            prepare: percentiles("prepare", "ok", &mut p_lat),
            accept: percentiles("accept", "ok", &mut a_lat),
            ref_read: percentiles("ref_read", "ok", &mut r_lat),
            state_read: percentiles("state_read", "ok", &mut s_lat),
        });
    }
    let passed = errors.is_empty() && reports.len() == cfg.depths.len();
    let report = BenchReport {
        run,
        environment: env,
        samples: cfg.samples,
        depths: reports,
        total_commits: depth,
        build_seconds: started.elapsed().as_secs_f64(),
        errors,
        passed,
    };
    std::fs::write(
        cfg.out.join("report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .expect("write report.json");
    let md = markdown(&report);
    std::fs::write(cfg.out.join("report.md"), &md).expect("write report.md");
    println!("{md}");
    if passed {
        println!("BENCH OK");
        ExitCode::SUCCESS
    } else {
        println!("BENCH INCOMPLETE");
        ExitCode::FAILURE
    }
}
