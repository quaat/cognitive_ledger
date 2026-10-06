# Performance testing

No throughput target exists yet. The first measured numbers are the Plan 0005 concurrency runs: [1,000 writers over two replicas](evidence/stress-1000-writers-2026-09-26.md) (p50/p95/p99 per operation, throughput, error classes, session utilisation, hardware) and the [fault-injection run](evidence/fault-injection-2026-09-26.md); reproduce them with `scripts/stress.sh` and `scripts/fault.sh`. The Plan 0005 §10 depth benchmark (prepare, accept, ref read, state read at depth 1/100/1,000/10,000) is recorded in [performance-baselines.md](performance-baselines.md) together with the checkpoint policy proposal. Baselines vary state size, commit count, patch size, branch count, concurrent writers and DAG shape; report p50/p95/p99, throughput, CPU, RSS and storage, and gate regressions against Cognitive Ledger's own accepted baseline. Fluree timing remains informational.

## Benchmark subsystem (Phase 6A, Plan 0010)
Phase 6 optimizations are gated by measurement through the benchmark subsystem in
[`docs/benchmarks/`](../benchmarks/README.md): `apps/ledger-bench` with
`scripts/benchmark.sh <profile>`. Every dataset there is also a **correctness oracle**: the
generated `synthetic-ledger-ci` checks every commit's reconstructed state, merge
classifications and results, diffs, parents and provenance. The `ci` profile runs on every
PR (`ci-benchmark`). Results are machine-readable (`result.json`, with the report generated
from it). Timings are observations until the regression policy in
[METRICS.md](../benchmarks/METRICS.md#regression-policy) defines tolerances. The Plan 0005
numbers below and in `performance-baselines.md` remain the historical linear-depth baseline
(`scripts/bench.sh`), and are not reinterpreted.

