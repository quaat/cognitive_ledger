//! `ledger-bench`: deterministic benchmark datasets with independent correctness oracles,
//! run against the production-shaped ledger stack (Plan 0010).
//!
//! ```text
//! ledger-bench list
//! ledger-bench validate --profile ci [--manifests benchmark/datasets]
//! ledger-bench manifest --profile ci            (print the computed manifests)
//! ledger-bench run --profile ci --replica http://127.0.0.1:8080 --out <dir>
//!                  [--owner-database-url <url>] [--manifests <dir>] [--meta key=value]...
//!                  [--issuer <iss>] [--audience <aud>] [--secret-env LEDGER_BENCH_HS256_SECRET]
//!                  [--allow-non-loopback]
//! ledger-bench report <result.json>              (re-render the report from the JSON)
//! ledger-bench annotate <result.json> key=value... (record script-side resource readings)
//! ledger-bench fetch <dataset> [--manifests <dir>] [--cache <dir>]   (network: pinned sources)
//! ledger-bench prepare <dataset> [--manifests <dir>] [--cache <dir>] (offline extraction)
//! ledger-bench clean <dataset> [--all] [--cache <dir>]
//! ledger-bench recon --replica <url> --out <dir> [--states 1,1000,10000]
//!                    [--depths 1,10,100,500,1000,2500,5000] [--reps 20] [--warmup 3]
//!                    [--preview-reps 10] [--cold-depths 100,1000,5000] [--cold-reps 3]
//!                    [--restart-cmd <shell>] [--server-cgroup <dir>] [--postgres-cgroup <dir>]
//!                    [--depth-limit 10000]  (the server's LEDGER_LIMIT_RECONSTRUCTION_DEPTH)
//!                    [--meta key=value]...   (reconstruction characterization; scripts/benchmark-recon.sh)
//! ```
//!
//! Every command except `fetch` is offline. Exit status: 0 pass; 1 correctness failure or
//! aborted run; 2 usage; 3 invalid or unprepared dataset (manifest mismatch, missing or
//! unverified cache, failed preparation); 4 `fetch` could not obtain the pinned source
//! (network, HTTP, or a download that does not match its pin).

use ledger_bench::{
    bear,
    dataset::{self, Context, Dataset, PROFILES, verify_manifest},
    manifest::Manifest,
    recon,
    result::{
        BenchResult, Counts, DatasetInfo, DatasetResult, Environment, OpStats, Performance,
        RESULT_SCHEMA, Resources, RunInfo, markdown, stats,
    },
    runner::{self, Config, Sample},
    workload::{Step, Workload},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const USAGE: &str = "usage: ledger-bench (list | validate --profile <p> | manifest --profile <p> | \
     run --profile <p> --replica <url> --out <dir> [--owner-database-url <url>] [--manifests <dir>] \
     [--meta key=value]... [--issuer <iss>] [--audience <aud>] [--secret-env <VAR>] [--allow-non-loopback] | \
     report <result.json> | annotate <result.json> key=value... | \
     fetch <dataset> | prepare <dataset> | clean <dataset> [--all])  [--manifests <dir>] [--cache <dir>]";

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis()
}

fn read_first(path: &str, key: &str) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| {
            t.lines().find(|l| l.starts_with(key)).and_then(|l| {
                l.split_once([':', '='])
                    .map(|(_, v)| v.trim().trim_matches('"').to_owned())
            })
        })
        .unwrap_or_else(|| "unknown".into())
}

fn environment(server: &str, postgres: &str) -> Environment {
    Environment {
        os: read_first("/etc/os-release", "PRETTY_NAME"),
        kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|_| "unknown".into()),
        cpu_model: read_first("/proc/cpuinfo", "model name"),
        cpus: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        mem_total_kib: read_first("/proc/meminfo", "MemTotal")
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0),
        postgres_version: postgres.into(),
        server: server.into(),
    }
}

fn peak_rss_kib() -> Option<u64> {
    let v = read_first("/proc/self/status", "VmHWM");
    v.split_whitespace().next()?.parse().ok()
}

fn is_loopback(url: &str) -> bool {
    let rest = url.split("://").nth(1).unwrap_or("");
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// The host of a `postgres://user:password@host:port/db?…` URL is loopback.
fn dsn_is_loopback(dsn: &str) -> bool {
    let rest = dsn.split("://").nth(1).unwrap_or("");
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    is_loopback(&format!("postgres://{host_port}"))
}

struct Args {
    profile: String,
    replica: String,
    out: PathBuf,
    owner_database_url: String,
    manifests: PathBuf,
    cache: PathBuf,
    meta: BTreeMap<String, String>,
    issuer: String,
    audience: String,
    secret_env: String,
    allow_non_loopback: bool,
}

fn parse(mut argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        profile: String::new(),
        replica: String::new(),
        out: PathBuf::new(),
        owner_database_url: std::env::var("LEDGER_BENCH_OWNER_DATABASE_URL").unwrap_or_default(),
        manifests: PathBuf::from("benchmark/datasets"),
        cache: PathBuf::from("target/benchmark-cache"),
        meta: BTreeMap::new(),
        issuer: "https://dev-issuer.example/".into(),
        audience: "api://sculpin-ledger-dev".into(),
        secret_env: "LEDGER_BENCH_HS256_SECRET".into(),
        allow_non_loopback: false,
    };
    while let Some(flag) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--profile" => a.profile = value()?,
            "--replica" => a.replica = value()?.trim_end_matches('/').to_owned(),
            "--out" => a.out = value()?.into(),
            "--owner-database-url" => a.owner_database_url = value()?,
            "--manifests" => a.manifests = value()?.into(),
            "--cache" => a.cache = value()?.into(),
            "--meta" => {
                let kv = value()?;
                let (k, v) = kv.split_once('=').ok_or("--meta takes key=value")?;
                a.meta.insert(k.to_owned(), v.to_owned());
            }
            "--issuer" => a.issuer = value()?,
            "--audience" => a.audience = value()?,
            "--secret-env" => a.secret_env = value()?,
            "--allow-non-loopback" => a.allow_non_loopback = true,
            _ => return Err(USAGE.into()),
        }
    }
    if !PROFILES.contains(&a.profile.as_str()) {
        return Err(format!("--profile must be one of {PROFILES:?}"));
    }
    Ok(a)
}

fn counts(w: &Workload) -> Counts {
    let mut c = Counts {
        commits: w.commit_count(),
        branches: w.final_heads.len(),
        genesis_quads: w
            .expected
            .values()
            .filter(|e| e.kind == "bulk")
            .map(|e| e.quads)
            .max()
            .unwrap_or(0),
        final_main_quads: w
            .final_heads
            .get("main")
            .and_then(|l| w.expected.get(l))
            .map_or(0, |e| e.quads),
        max_depth: w.expected.values().map(|e| e.depth).max().unwrap_or(0),
        ..Counts::default()
    };
    for step in &w.steps {
        if let Step::Merge(m) = step {
            c.merge_previews += m.previews.len();
            c.merges_applied += usize::from(m.apply.is_some());
            c.designed_conflicts = c.designed_conflicts.max(
                m.previews
                    .iter()
                    .map(|p| p.conflict_keys.len())
                    .max()
                    .unwrap_or(0),
            );
        }
    }
    c.integration_commits = c.merges_applied;
    c
}

fn bucket(depth: u32) -> &'static str {
    match depth {
        0..=9 => "depth 0-9",
        10..=49 => "depth 10-49",
        50..=99 => "depth 50-99",
        100..=199 => "depth 100-199",
        200..=499 => "depth 200-499",
        500..=999 => "depth 500-999",
        _ => "depth 1000+",
    }
}

fn performance(samples: &[Sample], phases_ms: BTreeMap<String, u128>) -> Performance {
    let mut by_op: BTreeMap<(&str, &str), Vec<u64>> = BTreeMap::new();
    let mut by_depth: BTreeMap<(&str, &str, &str), Vec<u64>> = BTreeMap::new();
    for s in samples {
        by_op.entry((s.category, s.op)).or_default().push(s.micros);
        if let (Some(d), true) = (
            s.depth,
            matches!(
                s.op,
                "prepare" | "state_read_head" | "state_read_historical" | "merge_preview"
            ),
        ) {
            by_depth
                .entry((s.op, s.kind, bucket(d)))
                .or_default()
                .push(s.micros);
        }
    }
    let operations: Vec<OpStats> = by_op
        .into_iter()
        .map(|((cat, op), mut v)| stats(cat, op, "all", &mut v))
        .collect();
    let by_depth: Vec<OpStats> = by_depth
        .into_iter()
        .map(|((op, kind, b), mut v)| stats("api", op, &format!("{kind} · {b}"), &mut v))
        .collect();
    let ingest_steps = samples
        .iter()
        .filter(|s| matches!(s.op, "accept" | "branch_create" | "merge_apply"))
        .count();
    let mut throughput = BTreeMap::new();
    if let Some(ms) = phases_ms
        .get("ingest and per-step checks")
        .filter(|ms| **ms > 0)
    {
        throughput.insert(
            "ingest_steps_per_s (commits + branches + applied merges, with per-step checks)"
                .to_owned(),
            ingest_steps as f64 / (*ms as f64 / 1000.0),
        );
    }
    Performance {
        phases_ms,
        throughput,
        operations,
        by_depth,
        series: runner::series(samples),
    }
}

fn write_result(out: &Path, r: &BenchResult) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(r).map_err(|e| e.to_string())?;
    std::fs::write(out.join("result.json"), json + "\n").map_err(|e| e.to_string())?;
    std::fs::write(out.join("report.md"), markdown(r)).map_err(|e| e.to_string())
}

/// A dataset prepared for a run, with its manifest check. `ready` is the workload and its
/// computed manifest, or why the dataset cannot run (unprepared cache, mismatch).
struct Prepared {
    dataset: Box<dyn Dataset>,
    ready: Result<(Workload, Manifest), String>,
    generate_ms: u128,
}

fn validate(profile: &str, ctx: &Context) -> Result<Vec<Prepared>, String> {
    let datasets = dataset::profile(profile).ok_or("unknown profile")?;
    let mut out = Vec::new();
    for dataset in datasets {
        let started = Instant::now();
        let ready = dataset.prepare(ctx).and_then(|w| {
            let m = dataset.manifest(ctx, &w)?;
            verify_manifest(&ctx.manifests, &m)?;
            Ok((w, m))
        });
        out.push(Prepared {
            dataset,
            ready,
            generate_ms: started.elapsed().as_millis(),
        });
    }
    Ok(out)
}

async fn run(a: Args) -> ExitCode {
    if !a.allow_non_loopback && !is_loopback(&a.replica) {
        eprintln!(
            "refusing non-loopback replica {} without --allow-non-loopback (the harness writes data)",
            a.replica
        );
        return ExitCode::from(2);
    }
    // The owner connection provisions graphs: the same guard applies to its host.
    if !a.allow_non_loopback && !dsn_is_loopback(&a.owner_database_url) {
        eprintln!(
            "refusing a non-loopback owner database without --allow-non-loopback (the harness provisions graphs)"
        );
        return ExitCode::from(2);
    }
    let Ok(secret) = std::env::var(&a.secret_env) else {
        eprintln!("{} must hold the development HS256 secret", a.secret_env);
        return ExitCode::from(2);
    };
    if a.owner_database_url.is_empty() || a.out.as_os_str().is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let started_ms = unix_ms();
    let started = Instant::now();
    let ctx = Context {
        manifests: a.manifests.clone(),
        cache: a.cache.clone(),
    };
    let prepared = match validate(&a.profile, &ctx) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let cfg = Config {
        replica: a.replica.clone(),
        owner_database_url: a.owner_database_url.clone(),
        issuer: a.issuer.clone(),
        audience: a.audience.clone(),
        secret,
        run_id: format!("{}-{}", started_ms, std::process::id()),
    };
    let mut result = BenchResult {
        schema: RESULT_SCHEMA.into(),
        status: "pass".into(),
        run: RunInfo {
            profile: a.profile.clone(),
            harness_version: format!("ledger-bench {}", env!("CARGO_PKG_VERSION")),
            started_unix_ms: started_ms,
            meta: a.meta.clone(),
            ..RunInfo::default()
        },
        ..BenchResult::default()
    };
    let mut exit = 0u8;
    let mut postgres = "unknown".to_owned();
    for Prepared {
        dataset: d,
        ready,
        generate_ms,
    } in prepared
    {
        let manifest_path = a
            .manifests
            .join(format!("{}.json", d.id()))
            .display()
            .to_string();
        let committed_checksum = dataset::load_manifest(&a.manifests, d.id())
            .map(|m| m.output.workload_checksum)
            .unwrap_or_else(|e| format!("unavailable: {e}"));
        let (w, m) = match ready {
            Ok(ok) => ok,
            Err(e) => {
                eprintln!("INVALID DATASET: {}: {e}", d.id());
                result.status = "fail".into();
                exit = 3;
                result.datasets.push(DatasetResult {
                    id: d.id().into(),
                    dataset: DatasetInfo {
                        manifest_path,
                        manifest: serde_json::Value::Null,
                        committed_workload_checksum: committed_checksum,
                        checksum_ok: false,
                        problem: Some(e),
                    },
                    ..DatasetResult::default()
                });
                continue;
            }
        };
        let mut dr = DatasetResult {
            id: d.id().into(),
            dataset: DatasetInfo {
                manifest_path,
                manifest: serde_json::to_value(&m).expect("serializable manifest"),
                committed_workload_checksum: committed_checksum,
                checksum_ok: true,
                problem: None,
            },
            counts: counts(&w),
            ..DatasetResult::default()
        };
        eprintln!(
            "{}: {} commits generated in {generate_ms} ms; running against {}",
            d.id(),
            w.commit_count(),
            a.replica
        );
        let data = runner::run(&w, d.id(), &cfg).await;
        postgres = data.postgres_version.clone();
        let mut phases = data.phases_ms.clone();
        phases.insert("generate (oracle)".into(), generate_ms);
        dr.correctness = data.correctness;
        if let Some(reason) = &data.aborted {
            dr.correctness.failed += 1;
            dr.correctness.failures.push(ledger_bench::result::Failure {
                check: "run completed".into(),
                subject: d.id().into(),
                detail: reason.clone(),
            });
        }
        if dr.correctness.failed > 0 {
            result.status = "fail".into();
            exit = exit.max(1);
        }
        dr.performance = performance(&data.samples, phases);
        dr.resources = Resources {
            harness_peak_rss_kib: peak_rss_kib(),
            db_bytes_before: data.db_bytes_before,
            db_bytes_after: data.db_bytes_after,
            immutable_objects_bytes_after: data.immutable_objects_bytes_after,
            immutable_objects_logical_bytes_after: data.immutable_objects_logical_bytes_after,
            annotations: BTreeMap::new(),
        };
        eprintln!(
            "{}: {} assertions, {} failed",
            d.id(),
            dr.correctness.assertions,
            dr.correctness.failed
        );
        result.datasets.push(dr);
    }
    result.environment = environment(&a.replica, &postgres);
    result.run.finished_unix_ms = unix_ms();
    result.run.wall_ms = started.elapsed().as_millis();
    if let Err(e) = write_result(&a.out, &result) {
        eprintln!("writing results: {e}");
        return ExitCode::from(1);
    }
    println!("{}", a.out.join("result.json").display());
    println!("BENCHMARK {} ({})", result.status.to_uppercase(), a.profile);
    ExitCode::from(exit)
}

fn annotate(path: &Path, pairs: impl Iterator<Item = String>) -> Result<(), String> {
    // The result and its report are rewritten in place, so only `…/result.json` is accepted.
    if path.file_name().and_then(|n| n.to_str()) != Some("result.json") {
        return Err(format!("{}: annotate takes a result.json", path.display()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut r: BenchResult = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    for kv in pairs {
        let (k, v) = kv.split_once('=').ok_or("annotations are key=value")?;
        for d in &mut r.datasets {
            d.resources.annotations.insert(k.to_owned(), v.to_owned());
        }
    }
    write_result(path.parent().unwrap_or(Path::new(".")), &r)
}

/// A comma-separated list of counts; an empty value is an empty list.
fn list(v: &str) -> Result<Vec<usize>, String> {
    if v.trim().is_empty() {
        return Ok(Vec::new());
    }
    v.split(',')
        .map(|x| {
            x.trim()
                .parse::<usize>()
                .map_err(|_| format!("bad list {v:?}"))
        })
        .collect()
}

async fn recon_command(mut argv: impl Iterator<Item = String>) -> ExitCode {
    let mut cfg = recon::ReconConfig {
        replica: String::new(),
        owner_database_url: std::env::var("LEDGER_BENCH_OWNER_DATABASE_URL").unwrap_or_default(),
        secret: std::env::var("LEDGER_BENCH_HS256_SECRET").unwrap_or_default(),
        issuer: "https://dev-issuer.example/".into(),
        audience: "api://sculpin-ledger-dev".into(),
        states: vec![1, 1_000, 10_000],
        depths: vec![1, 10, 100, 500, 1_000, 2_500, 5_000],
        reps: 20,
        warmup: 3,
        preview_reps: 10,
        cold_depths: vec![100, 1_000, 5_000],
        cold_reps: 3,
        restart_cmd: None,
        server_cgroup: None,
        postgres_cgroup: None,
        depth_limit: ledger_store::ReconstructionLimits::DEVELOPMENT.max_depth,
        run_id: format!("{}-{}", unix_ms(), std::process::id()),
    };
    let mut out = PathBuf::new();
    let mut meta = BTreeMap::new();
    let parsed: Result<(), String> = (|| {
        while let Some(flag) = argv.next() {
            let mut value = || argv.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--replica" => cfg.replica = value()?.trim_end_matches('/').to_owned(),
                "--owner-database-url" => cfg.owner_database_url = value()?,
                "--out" => out = value()?.into(),
                "--states" => cfg.states = list(&value()?)?,
                "--depths" => cfg.depths = list(&value()?)?,
                "--reps" => cfg.reps = value()?.parse().map_err(|_| "--reps")?,
                "--warmup" => cfg.warmup = value()?.parse().map_err(|_| "--warmup")?,
                "--preview-reps" => {
                    cfg.preview_reps = value()?.parse().map_err(|_| "--preview-reps")?
                }
                "--cold-depths" => cfg.cold_depths = list(&value()?)?,
                "--cold-reps" => cfg.cold_reps = value()?.parse().map_err(|_| "--cold-reps")?,
                "--restart-cmd" => cfg.restart_cmd = Some(value()?),
                "--server-cgroup" => cfg.server_cgroup = Some(value()?.into()),
                "--postgres-cgroup" => cfg.postgres_cgroup = Some(value()?.into()),
                "--depth-limit" => {
                    cfg.depth_limit = value()?.parse().map_err(|_| "--depth-limit")?
                }
                "--meta" => {
                    let kv = value()?;
                    let (k, v) = kv.split_once('=').ok_or("--meta takes key=value")?;
                    if k == "official" {
                        return Err(
                            "official is decided by scripts/benchmark-recon.sh (LEDGER_BENCH_OFFICIAL), not by --meta".into(),
                        );
                    }
                    meta.insert(k.to_owned(), v.to_owned());
                }
                _ => return Err(USAGE.into()),
            }
        }
        Ok(())
    })();
    if let Err(e) = parsed {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    if !is_loopback(&cfg.replica)
        || !dsn_is_loopback(&cfg.owner_database_url)
        || cfg.secret.is_empty()
        || out.as_os_str().is_empty()
    {
        eprintln!(
            "recon needs a loopback --replica and owner database, LEDGER_BENCH_HS256_SECRET and --out"
        );
        return ExitCode::from(2);
    }
    if let Err(e) = cfg.validate() {
        eprintln!("invalid recon configuration: {e}");
        return ExitCode::from(2);
    }
    let started = Instant::now();
    let mut r = recon::run(&cfg).await;
    r.meta = meta;
    // Set by benchmark-recon.sh from the checkout state and the absence of custom arguments.
    r.meta.insert(
        "official".into(),
        std::env::var("LEDGER_BENCH_OFFICIAL")
            .unwrap_or_else(|_| "no (not run by scripts/benchmark-recon.sh)".into()),
    );
    r.meta.insert(
        "wall_s".into(),
        format!("{:.0}", started.elapsed().as_secs_f64()),
    );
    let env = environment(&cfg.replica, "see the meta postgres entry");
    r.environment = BTreeMap::from([
        ("os".into(), env.os),
        ("kernel".into(), env.kernel),
        ("cpu".into(), format!("{} × {}", env.cpus, env.cpu_model)),
        (
            "mem_total_mib".into(),
            (env.mem_total_kib / 1024).to_string(),
        ),
        ("states".into(), format!("{:?}", cfg.states)),
        ("depths".into(), format!("{:?}", cfg.depths)),
        (
            "reps".into(),
            format!(
                "{} (+{} warm-up); previews {}; cold {} at {:?}",
                cfg.reps, cfg.warmup, cfg.preview_reps, cfg.cold_reps, cfg.cold_depths
            ),
        ),
        (
            "cache_conditions".into(),
            format!(
                "warm: after warm-up on an active database; {}: PostgreSQL process and shared buffers restarted, then /ready, pool reconnection and statistics queries, then the measured first ledger reconstruction (OS page cache NOT dropped)",
                recon::FIRST_AFTER_RESTART
            ),
        ),
    ]);
    let write = || -> Result<(), String> {
        std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        std::fs::write(
            out.join("recon.json"),
            serde_json::to_string_pretty(&r).map_err(|e| e.to_string())? + "\n",
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(out.join("recon.md"), recon::markdown(&r)).map_err(|e| e.to_string())
    };
    if let Err(e) = write() {
        eprintln!("writing results: {e}");
        return ExitCode::from(1);
    }
    println!(
        "RECON {} ({} points, {} failures)",
        r.status.to_uppercase(),
        r.points.len(),
        r.failures.len()
    );
    if r.status == "pass" {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut argv = std::env::args().skip(1);
    let Some(cmd) = argv.next() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match cmd.as_str() {
        "list" => {
            for p in PROFILES {
                let ids: Vec<&str> = dataset::profile(p)
                    .unwrap()
                    .iter()
                    .map(|d| d.id())
                    .collect();
                println!("profile {p}: {}", ids.join(", "));
            }
            ExitCode::SUCCESS
        }
        "validate" | "manifest" => {
            let a = match parse(argv) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            };
            let ctx = Context {
                manifests: a.manifests.clone(),
                cache: a.cache.clone(),
            };
            let datasets = match dataset::profile(&a.profile) {
                Some(d) => d,
                None => {
                    eprintln!("unknown profile");
                    return ExitCode::from(2);
                }
            };
            let mut exit = 0;
            for d in datasets {
                let started = Instant::now();
                let computed = d.prepare(&ctx).and_then(|w| d.manifest(&ctx, &w));
                let ms = started.elapsed().as_millis();
                match (cmd.as_str(), computed) {
                    ("manifest", Ok(m)) => println!(
                        "{}",
                        serde_json::to_string_pretty(&m).expect("serializable")
                    ),
                    (_, Ok(m)) => match verify_manifest(&ctx.manifests, &m) {
                        Ok(()) => println!(
                            "{}: valid ({} commits, {}, prepared in {ms} ms)",
                            d.id(),
                            m.output.commits,
                            m.output.workload_checksum
                        ),
                        Err(e) => {
                            eprintln!("INVALID DATASET: {e}");
                            exit = 3;
                        }
                    },
                    (_, Err(e)) => {
                        eprintln!("INVALID DATASET: {}: {e}", d.id());
                        exit = 3;
                    }
                }
            }
            ExitCode::from(exit)
        }
        "recon" => recon_command(argv).await,
        "fetch" | "prepare" | "clean" => {
            let Some(id) = argv.next() else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            let mut ctx = Context {
                manifests: PathBuf::from("benchmark/datasets"),
                cache: PathBuf::from("target/benchmark-cache"),
            };
            let mut all = false;
            while let Some(flag) = argv.next() {
                match flag.as_str() {
                    "--all" if cmd == "clean" => all = true,
                    "--manifests" | "--cache" => {
                        let Some(v) = argv.next() else {
                            eprintln!("{USAGE}");
                            return ExitCode::from(2);
                        };
                        if flag == "--cache" {
                            ctx.cache = v.into();
                        } else {
                            ctx.manifests = v.into();
                        }
                    }
                    _ => {
                        eprintln!("{USAGE}");
                        return ExitCode::from(2);
                    }
                }
            }
            if id != "bear-b-ci" {
                eprintln!(
                    "{id}: only extracted datasets (bear-b-ci) have a fetch/prepare lifecycle"
                );
                return ExitCode::from(2);
            }
            let outcome = match cmd.as_str() {
                "fetch" => bear::fetch(&ctx, &id).await,
                "prepare" => bear::prepare(&ctx, &id).map(|(sha, bytes, x)| {
                    vec![
                        format!("prepared artifact: {bytes} bytes, sha256 {sha}"),
                        format!("range: {}", x.range),
                        format!("counts: {:?}", x.counts),
                    ]
                }),
                _ => bear::clean(&ctx, &id, all),
            };
            match outcome {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{cmd} {id}: {e}");
                    ExitCode::from(if cmd == "fetch" { 4 } else { 3 })
                }
            }
        }
        "run" => match parse(argv) {
            Ok(a) => run(a).await,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(2)
            }
        },
        "report" => {
            let Some(path) = argv.next() else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<BenchResult>(&t).map_err(|e| e.to_string()))
            {
                Ok(r) => {
                    print!("{}", markdown(&r));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{path}: {e}");
                    ExitCode::from(2)
                }
            }
        }
        "annotate" => {
            let Some(path) = argv.next() else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            match annotate(Path::new(&path), argv) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::from(2)
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_list_value_is_an_empty_list() {
        assert_eq!(list(""), Ok(vec![]));
        assert_eq!(list("1, 10,100"), Ok(vec![1, 10, 100]));
        assert!(list("1,,2").is_err());
    }

    #[test]
    fn only_loopback_replicas_are_accepted_by_default() {
        for ok in [
            "http://127.0.0.1:8080",
            "http://localhost:8080/",
            "http://[::1]:8080",
        ] {
            assert!(is_loopback(ok), "{ok}");
        }
        for bad in [
            "http://10.0.0.1:8080",
            "http://example.org",
            "http://127.0.0.1.evil:80",
        ] {
            assert!(!is_loopback(bad), "{bad}");
        }
    }

    #[test]
    fn only_loopback_owner_databases_are_accepted_by_default() {
        for ok in [
            "postgres://ledger:secret@127.0.0.1:55432/ledger?sslmode=disable",
            "postgres://u:p@localhost/db",
        ] {
            assert!(dsn_is_loopback(ok), "{ok}");
        }
        for bad in [
            "postgres://u:p@db.example.org:5432/ledger",
            "postgres://u:p@10.1.2.3/db",
            "postgres://u:127.0.0.1@evil.example/db",
        ] {
            assert!(!dsn_is_loopback(bad), "{bad}");
        }
    }

    #[test]
    fn annotate_only_rewrites_a_result_json() {
        let err = annotate(Path::new("/tmp/elsewhere/other.json"), std::iter::empty()).unwrap_err();
        assert!(err.contains("annotate takes a result.json"), "{err}");
    }

    #[test]
    fn depth_buckets_cover_every_depth() {
        assert_eq!(bucket(0), "depth 0-9");
        assert_eq!(bucket(250), "depth 200-499");
        assert_eq!(bucket(u32::MAX), "depth 1000+");
    }
}
