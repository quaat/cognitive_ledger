# Benchmarks

Phase 6 (reconstruction scalability) starts with measurement: every optimization must be
justified by numbers from this subsystem (`product_development_plan.md` §24 and the
roadmap reconciliation). Plan 0010 (Phase 6A) builds the foundation.

| document | content |
|---|---|
| [BENCHMARK_ARCHITECTURE.md](BENCHMARK_ARCHITECTURE.md) | M0 assessment, boundaries, oracle independence, profiles, result schema |
| [DATASETS.md](DATASETS.md) | each dataset's purpose, the implemented synthetic dataset, and the extraction plans for BEAR-B, `tkgl-smallpedia` and `thgl-software` |
| [RUNNING_BENCHMARKS.md](RUNNING_BENCHMARKS.md) | commands, profiles, exit codes, CI job |
| [METRICS.md](METRICS.md) | correctness assertions, timing and resource metrics, methodology, regression policy |
| [REPRODUCIBILITY.md](REPRODUCIBILITY.md) | provenance recorded per run; manifests; what makes a result official |
| [INTEGRATION_PLAN.md](INTEGRATION_PLAN.md) | the supplied requirements document (verbatim; not a status record) |

Code: `apps/ledger-bench`. Manifests: `benchmark/datasets/`. Runner: `scripts/benchmark.sh`.
Results are machine-readable (`result.json`) and the reports are generated from them; no
benchmark number is maintained by hand in these documents. The historical Plan 0005
linear-depth baseline remains in
[performance-baselines.md](../quality/performance-baselines.md), produced by
`scripts/bench.sh`.
