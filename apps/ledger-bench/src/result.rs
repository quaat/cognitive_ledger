//! The machine-readable result (`sculpin-ledger-bench-result/v1`) and the Markdown report
//! rendered from it. The JSON is authoritative: the report is regenerated from it
//! (`ledger-bench report <result.json>`) and never maintained separately.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const RESULT_SCHEMA: &str = "sculpin-ledger-bench-result/v1";

/// Below this many samples the report shows no p95/p99 (they would equal the maximum).
pub const MIN_TAIL_SAMPLES: usize = 20;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BenchResult {
    pub schema: String,
    /// `pass` only when every dataset's checksum and every correctness assertion held.
    pub status: String,
    pub run: RunInfo,
    pub environment: Environment,
    pub datasets: Vec<DatasetResult>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunInfo {
    pub profile: String,
    pub harness_version: String,
    pub started_unix_ms: u128,
    pub finished_unix_ms: u128,
    pub wall_ms: u128,
    /// `--meta key=value` pairs from the invoking script: build revision, dirty flag,
    /// rustc, server image, ...
    pub meta: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Environment {
    pub os: String,
    pub kernel: String,
    pub cpu_model: String,
    pub cpus: usize,
    pub mem_total_kib: u64,
    pub postgres_version: String,
    pub server: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DatasetResult {
    pub id: String,
    pub dataset: DatasetInfo,
    pub counts: Counts,
    pub correctness: Correctness,
    /// Observations only: never a pass/fail criterion in Phase 6A.
    pub performance: Performance,
    pub resources: Resources,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DatasetInfo {
    pub manifest_path: String,
    /// The computed manifest (`sculpin-ledger-bench-manifest/v2`); equal to the committed one
    /// when `checksum_ok`. `null` when the dataset could not be prepared.
    pub manifest: serde_json::Value,
    pub committed_workload_checksum: String,
    pub checksum_ok: bool,
    /// Why the dataset could not run (unprepared cache, manifest mismatch).
    pub problem: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Counts {
    pub commits: usize,
    pub integration_commits: usize,
    pub branches: usize,
    pub merge_previews: usize,
    pub merges_applied: usize,
    pub designed_conflicts: usize,
    pub genesis_quads: usize,
    pub final_main_quads: usize,
    pub max_depth: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Correctness {
    pub assertions: u64,
    pub failed: u64,
    /// Every assertion family exercised, with its count.
    pub checks: BTreeMap<String, u64>,
    /// The first failures (bounded), with enough detail to start an investigation.
    pub failures: Vec<Failure>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Failure {
    pub check: String,
    pub subject: String,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Performance {
    /// Ingest and verification phases, wall time.
    pub phases_ms: BTreeMap<String, u128>,
    /// Throughput of the serial ingest phase, for example `commits_per_s` (commits, branch
    /// creations and merges, with their per-step state checks).
    pub throughput: BTreeMap<String, f64>,
    /// Per (category, operation): percentiles over all samples.
    pub operations: Vec<OpStats>,
    /// Per (operation, history kind, depth bucket) for the depth-sensitive operations.
    pub by_depth: Vec<OpStats>,
    /// Raw observations of the depth-sensitive operations.
    pub series: Vec<SeriesPoint>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpStats {
    /// `api` (public HTTP API), `persisted` (owner-identity read of the production store) or
    /// `algorithm` (an infrastructure-free ledger crate on ledger-materialized states).
    pub category: String,
    pub op: String,
    pub group: String,
    pub count: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SeriesPoint {
    pub op: String,
    pub kind: String,
    pub depth: u32,
    pub quads: usize,
    /// Patch operations along the parent-0 chain (the logical work of a reconstruction).
    pub fold_ops: u64,
    /// Response body bytes.
    pub bytes: Option<usize>,
    pub ms: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Resources {
    /// Peak resident set of the harness process (`VmHWM`), KiB.
    pub harness_peak_rss_kib: Option<u64>,
    pub db_bytes_before: Option<i64>,
    pub db_bytes_after: Option<i64>,
    /// On-disk size of `immutable_objects` (commits and patches, TOAST-compressed) after
    /// the run: `pg_total_relation_size`.
    pub immutable_objects_bytes_after: Option<i64>,
    /// Logical bytes of every object in `immutable_objects` (what hashing and decoding
    /// read), whole database.
    pub immutable_objects_logical_bytes_after: Option<i64>,
    /// Annotations added by the invoking script after the run (for example the server
    /// container's peak memory); `unavailable` when it could not be read reliably.
    pub annotations: BTreeMap<String, String>,
}

/// Percentiles by nearest rank over microsecond samples.
pub fn stats(category: &str, op: &str, group: &str, micros: &mut [u64]) -> OpStats {
    micros.sort_unstable();
    let pick = |q: f64| -> f64 {
        if micros.is_empty() {
            return 0.0;
        }
        let rank = ((micros.len() as f64) * q).ceil() as usize;
        micros[rank.clamp(1, micros.len()) - 1] as f64 / 1000.0
    };
    let total: u64 = micros.iter().sum();
    OpStats {
        category: category.into(),
        op: op.into(),
        group: group.into(),
        count: micros.len(),
        p50_ms: pick(0.50),
        p95_ms: pick(0.95),
        p99_ms: pick(0.99),
        max_ms: pick(1.0),
        mean_ms: if micros.is_empty() {
            0.0
        } else {
            total as f64 / micros.len() as f64 / 1000.0
        },
    }
}

/// The human-readable report, rendered only from the JSON result.
pub fn markdown(r: &BenchResult) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Benchmark run — profile `{}`: **{}**\n",
        r.run.profile, r.status
    );
    let _ = writeln!(
        out,
        "Generated from `result.json` ({}); the JSON is authoritative.\n",
        r.schema
    );
    let _ = writeln!(out, "| run | |\n|---|---|");
    let _ = writeln!(out, "| harness | {} |", r.run.harness_version);
    for (k, v) in &r.run.meta {
        let _ = writeln!(out, "| {k} | `{v}` |");
    }
    let _ = writeln!(
        out,
        "| wall time | {:.1} s |",
        r.run.wall_ms as f64 / 1000.0
    );
    let e = &r.environment;
    let _ = writeln!(
        out,
        "| host | {} · kernel {} · {} × {} · {} MiB RAM |",
        e.os,
        e.kernel,
        e.cpus,
        e.cpu_model,
        e.mem_total_kib / 1024
    );
    let _ = writeln!(out, "| PostgreSQL | {} |", e.postgres_version);
    let _ = writeln!(out, "| server | {} |\n", e.server);
    for d in &r.datasets {
        let _ = writeln!(out, "## Dataset `{}`\n", d.id);
        let i = &d.dataset;
        let m = &i.manifest;
        let text = |v: &serde_json::Value| v.as_str().unwrap_or("?").to_owned();
        let origin = if m["kind"] == "extracted" {
            format!(
                "Extracted from {} ({}, {}); license: {}; attribution: {}; modified: normalized to canonical N-Quads and windowed by `{}`; redistribution: {}; range: {}; artifact sha256 `{}`",
                text(&m["source"]["title"]),
                text(&m["source"]["publisher"]),
                text(&m["source"]["source_version"]),
                text(&m["source"]["license"]),
                text(&m["source"]["attribution"]),
                text(&m["extraction"]["version"]),
                text(&m["source"]["redistribution"]),
                text(&m["extraction"]["range"]),
                text(&m["output"]["artifact_sha256"])
            )
        } else {
            format!(
                "Generator `{}` `{}`, seed `{}`; license: {}",
                text(&m["generator"]["name"]),
                text(&m["generator"]["version"]),
                text(&m["generator"]["seed"]),
                text(&m["generator"]["license"])
            )
        };
        let _ = writeln!(
            out,
            "{origin}. Workload checksum `{}` ({}).\n",
            text(&m["output"]["workload_checksum"]),
            if i.checksum_ok {
                "matches the manifest".to_owned()
            } else {
                format!(
                    "**invalid**: {}",
                    i.problem
                        .as_deref()
                        .unwrap_or("does not match the manifest")
                )
            }
        );
        let c = &d.counts;
        let _ = writeln!(
            out,
            "{} commits ({} integration), {} branches, {} merge previews, {} merges applied, {} designed conflicts; genesis {} quads, final `main` {} quads; max parent-0 depth {}.\n",
            c.commits,
            c.integration_commits,
            c.branches,
            c.merge_previews,
            c.merges_applied,
            c.designed_conflicts,
            c.genesis_quads,
            c.final_main_quads,
            c.max_depth
        );
        let k = &d.correctness;
        let _ = writeln!(
            out,
            "### Correctness: {} assertions, {} failed\n\n| check | assertions |\n|---|---:|",
            k.assertions, k.failed
        );
        for (name, n) in &k.checks {
            let _ = writeln!(out, "| {name} | {n} |");
        }
        if !k.failures.is_empty() {
            let _ = writeln!(out, "\n**Failures** (first {}):\n", k.failures.len());
            for f in &k.failures {
                let _ = writeln!(out, "- `{}` {}: {}", f.check, f.subject, f.detail);
            }
        }
        let p = &d.performance;
        let _ = writeln!(
            out,
            "\n### Observations (not gates)\n\n| phase | ms |\n|---|---:|"
        );
        for (phase, ms) in &p.phases_ms {
            let _ = writeln!(out, "| {phase} | {ms} |");
        }
        for (name, v) in &p.throughput {
            let _ = writeln!(out, "| throughput: {name} | {v:.2} |");
        }
        let table = |out: &mut String, title: &str, rows: &[OpStats]| {
            let _ = writeln!(
                out,
                "\n{title} (p95/p99 shown only for n ≥ {MIN_TAIL_SAMPLES})\n\n| category | operation | group | n | p50 ms | p95 ms | p99 ms | max ms | mean ms |\n|---|---|---|---:|---:|---:|---:|---:|---:|"
            );
            for s in rows {
                let tail = |v: f64| {
                    if s.count >= MIN_TAIL_SAMPLES {
                        format!("{v:.1}")
                    } else {
                        "—".to_owned()
                    }
                };
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {:.1} | {} | {} | {:.1} | {:.1} |",
                    s.category,
                    s.op,
                    s.group,
                    s.count,
                    s.p50_ms,
                    tail(s.p95_ms),
                    tail(s.p99_ms),
                    s.max_ms,
                    s.mean_ms
                );
            }
        };
        table(&mut out, "All samples:", &p.operations);
        table(&mut out, "By history kind and parent-0 depth:", &p.by_depth);
        let res = &d.resources;
        let show = |v: Option<i64>| {
            v.map_or("unavailable".to_owned(), |b| {
                format!("{:.1} MiB", b as f64 / 1048576.0)
            })
        };
        let _ = writeln!(
            out,
            "\n### Resources\n\n| resource | value |\n|---|---|\n| harness peak RSS (includes oracle generation) | {} |\n| database before | {} |\n| database after | {} |\n| `immutable_objects` on disk after | {} |\n| `immutable_objects` logical bytes after | {} |",
            res.harness_peak_rss_kib
                .map_or("unavailable".to_owned(), |k| format!(
                    "{:.1} MiB",
                    k as f64 / 1024.0
                )),
            show(res.db_bytes_before),
            show(res.db_bytes_after),
            show(res.immutable_objects_bytes_after),
            show(res.immutable_objects_logical_bytes_after),
        );
        for (k, v) in &res.annotations {
            let _ = writeln!(out, "| {k} | {v} |");
        }
        let _ = writeln!(out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut v: Vec<u64> = (1..=100).map(|i| i * 1000).collect();
        let s = stats("api", "x", "all", &mut v);
        assert_eq!(
            (s.p50_ms, s.p95_ms, s.p99_ms, s.max_ms),
            (50.0, 95.0, 99.0, 100.0)
        );
        assert_eq!(s.count, 100);
        let empty = stats("api", "x", "all", &mut []);
        assert_eq!((empty.count, empty.p50_ms), (0, 0.0));
    }

    #[test]
    fn the_report_round_trips_through_json() {
        let mut ops: Vec<u64> = (1..=30).map(|i| i * 1000).collect();
        let r = BenchResult {
            schema: RESULT_SCHEMA.into(),
            status: "fail".into(),
            datasets: vec![DatasetResult {
                id: "synthetic-ledger-ci".into(),
                correctness: Correctness {
                    assertions: 3,
                    failed: 1,
                    checks: BTreeMap::from([("merge base".to_owned(), 3)]),
                    failures: vec![Failure {
                        check: "merge base".into(),
                        subject: "merge#1".into(),
                        detail: "ledger x, oracle y".into(),
                    }],
                },
                performance: Performance {
                    operations: vec![
                        stats("api", "prepare", "all", &mut ops),
                        stats("api", "rare", "all", &mut [5000]),
                    ],
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: BenchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(markdown(&back), markdown(&r));
        let md = markdown(&r);
        assert!(md.contains("synthetic-ledger-ci") && md.contains("**fail**"));
        assert!(md.contains("`merge base` merge#1: ledger x, oracle y"));
        // p95/p99 only for n ≥ 20.
        assert!(md.contains("| api | prepare | all | 30 | 15.0 | 29.0 | 30.0 | 30.0 | 15.5 |"));
        assert!(md.contains("| api | rare | all | 1 | 5.0 | — | — | 5.0 | 5.0 |"));
    }
}
