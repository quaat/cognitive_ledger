# Benchmark data

Committed, reviewed benchmark inputs (no code). The harness is `apps/ledger-bench`, the
documentation is [`docs/benchmarks/`](../docs/benchmarks/README.md), and the runner is
`scripts/benchmark.sh`.

- `datasets/<dataset-id>.json`: the manifest of each dataset (source, version, licence,
  generator version, seed, parameters, output checksum). The harness refuses to run a
  dataset whose computed manifest differs. Update a manifest only deliberately, with
  `ledger-bench manifest --profile <p>`, reviewed like a golden vector.

Raw third-party datasets are never committed here (see `docs/benchmarks/DATASETS.md`).
