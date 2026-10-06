# Running benchmarks

## Prerequisites
Docker with Compose, a Rust toolchain (1.89), `curl` and `git`. The production-shaped stack
from `compose.yaml` (PostgreSQL, owner migration, the least-privilege runtime server) is
started by the script on loopback ports 8080 and 55432. Run one qualification script at a
time, because the integration, stress and benchmark harnesses share those ports.

## Commands

```bash
./scripts/benchmark.sh ci       # the PR profile: synthetic-ledger-ci + bear-b-ci
./scripts/benchmark.sh local    # deeper baseline: synthetic-ledger-local (~10–20 min)
./scripts/benchmark.sh bear     # bear-b-ci alone
./scripts/benchmark-recon.sh    # reconstruction characterization (≥ 1 h; outside PR CI)
```

### Extracted datasets: fetch → prepare → run
`scripts/benchmark.sh` runs these steps itself for the `ci` and `bear` profiles. By hand:

```bash
cargo run --release -p ledger-bench -- fetch bear-b-ci     # network: pinned SHA-256 and size
cargo run --release -p ledger-bench -- prepare bear-b-ci   # offline extraction and cross-checks
cargo run --release -p ledger-bench -- validate --profile bear
cargo run --release -p ledger-bench -- clean bear-b-ci [--all]   # artifact; --all also the sources
```

- The cache is `target/benchmark-cache/<dataset>/{source,prepared}` (`--cache` changes it).
- `validate` and `run` never download. A missing, stale or mismatching cache is exit 3.
- `fetch` re-verifies cached files and skips them when they match.
- In CI, `actions/cache` keeps the sources keyed by the manifest; `fetch` and `prepare` verify
  them again after a restore.

### Reconstruction characterization
`scripts/benchmark-recon.sh` layers `benchmark/compose.instrumented.yaml` over the stack.
That benchmark-only override preloads `pg_stat_statements`, enables `track_io_timing` and
creates the extension in schema `bench_stats`; production defaults are unchanged. The
script then runs `ledger-bench recon` and ends with `ledger-admin verify`. Output:
`target/benchmark/<UTC>-recon/{recon.json,recon.md}`. See [METRICS.md](METRICS.md#reconstruction-characterization)
for what each operation isolates.

- **Invalid configurations are refused before anything runs** (exit 2):
  - empty `--states` or `--depths`, or a state size of 0;
  - `--reps` or `--preview-reps` of 0;
  - `--cold-depths` without `--restart-cmd`, with `--cold-reps 0`, or with a depth that is
    not in `--depths`;
  - a depth whose depth + 1 exceeds the server's reconstruction limit (`--depth-limit`,
    which the script reads from the compose configuration).
- **Official runs come from a clean checkout only.** The result records `build_rev`,
  `tracked_changes`, `untracked_files`, the input hash and the images, and `official=yes`
  only when there are no tracked changes and no untracked files. `recon.md` marks every
  other run **NON-OFFICIAL**. For architecture evidence, run from a detached worktree:

  ```sh
  git worktree add --detach target/recon-tree <revision>
  target/recon-tree/scripts/benchmark-recon.sh
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
