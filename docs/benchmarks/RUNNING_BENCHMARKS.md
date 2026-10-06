# Running benchmarks

## Prerequisites
Docker with Compose, a Rust toolchain (1.89), `curl` and `git`. The production-shaped stack
from `compose.yaml` (PostgreSQL, owner migration, the least-privilege runtime server) is
started by the script on loopback ports 8080 and 55432. Run one qualification script at a
time, because the integration, stress and benchmark harnesses share those ports.

## Commands

```bash
./scripts/benchmark.sh ci       # the PR profile: synthetic-ledger-ci
./scripts/benchmark.sh local    # deeper baseline: synthetic-ledger-local (~10–20 min)
```

The script:
1. builds `ledger-bench` (release);
2. **validates the datasets of every profile (`ci` and `local`) against their committed
   manifests**, failing fast with exit 3 on any difference, so a drifting `local` manifest
   also fails the PR job;
3. builds and starts the stack;
4. runs the profile;
5. records container peak memory;
6. runs `ledger-admin verify`, which must print `VERIFY OK`.

Output goes to `target/benchmark/<UTC>-<profile>/`:

| file | content |
|---|---|
| `result.json` | the authoritative machine-readable result |
| `report.md` | rendered from `result.json` |
| `phases.txt` | build, start, run and verify durations |
| `run.log`, `verify.log`, `server.log` | logs |

The harness on its own:

```bash
cargo run --release -p ledger-bench -- list
cargo run --release -p ledger-bench -- validate --profile ci       # offline, no stack
cargo run --release -p ledger-bench -- manifest --profile ci       # print the computed manifest
cargo run --release -p ledger-bench -- report target/benchmark/<run>/result.json
```

`run` needs `--replica` (loopback only, unless `--allow-non-loopback`), `--out`, the owner
database URL (`--owner-database-url` or `LEDGER_BENCH_OWNER_DATABASE_URL`) and the
development HS256 secret in `LEDGER_BENCH_HS256_SECRET`. The script supplies all of them.

## Exit status

| code | meaning |
|---|---|
| 0 | every dataset valid; every correctness assertion held; `VERIFY OK` |
| 1 | a correctness assertion failed, a step was refused, the run aborted, or verify failed |
| 2 | usage or configuration error |
| 3 | invalid dataset: the computed manifest differs from the committed one |

## Changing a dataset
Changing the generator, its parameters or the seed changes the manifest. Do it deliberately:

1. Bump `GENERATOR_VERSION` when the generation logic changes.
2. Run `ledger-bench manifest --profile <p>`.
3. Review the difference, and commit the new manifest together with the change.

A manifest is reviewed like a golden vector.

## CI
`.github/workflows/ci-benchmark.yml` runs `scripts/benchmark.sh ci` on every PR and on
pushes to `main`. It uploads `result.json`, `report.md`, the phases and the logs as the
`benchmark-ci` artifact, and appends the report to the job summary. The job fails on an
invalid dataset, a correctness failure or a failed verify. It never fails on timing (see
[METRICS.md](METRICS.md#regression-policy)). Image and crate downloads happen before the
benchmark run; the run itself only talks to loopback.
