# Plan 0010: Phase 6A — benchmark foundation before reconstruction optimization

Status: **in progress** (started 2026-10-06). Branch `claude/p6a-benchmark-foundation` from
`main` at `c7a94d57a573f6cbf650f39a5a779c3aeebf6b1e`. That commit is the PR #10 merge on top of
the PR #11 Phase-5 merge `319367f`; see
[Plan 0009](../completed/0009-phase5-diff-and-merge.md). It differs from the reviewed Phase-5
head `fb9d0d8` only by the two-line `taiki-e/install-action` pin bump in `ci-fuzz.yml` and
`ci-security.yml`. No gate is reported as passed until it is executable and has run.

## Goal
Phase 6 ("Reconstruction scalability", `product_development_plan.md` §24 and Phase 6) must
justify every optimization by measurement. This plan builds the measurement foundation: a
reproducible benchmark subsystem whose first dataset, `synthetic-ledger-ci`, is a
**correctness oracle** for the production API path, plus a pre-optimization baseline of the
unchanged Phase-5 implementation. Checkpoint design (a later ADR) starts from these numbers.

Requirements source: the *Cognitive Ledger Benchmark Integration and Validation Plan*,
committed as [`docs/benchmarks/INTEGRATION_PLAN.md`](../../benchmarks/INTEGRATION_PLAN.md).
This plan covers its first development pass only: M0, M1, M2, the CI benchmark
architecture, and extraction plans for BEAR-B, `tkgl-smallpedia` and `thgl-software`.

## Scope
1. **M0.** Repository and benchmark architecture assessment, written up as
   [`docs/benchmarks/BENCHMARK_ARCHITECTURE.md`](../../benchmarks/BENCHMARK_ARCHITECTURE.md)
   and independently reviewed.
2. **M1.** A common deterministic harness, `apps/ledger-bench`, with:
   - a dataset lifecycle boundary;
   - a JSON result schema, with the report derived from it;
   - run provenance;
   - a non-zero exit on correctness, dataset or harness failure.
3. **M2.** `synthetic-ledger-ci`, a seeded generator plus an independent oracle exercised
   through the public HTTP API of the production-shaped stack.
4. **CI.** A dedicated `ci-benchmark` workflow (profile `ci`) that uploads the JSON result and
   uses no network during the run.
5. **Baseline.** Runs of the unchanged Phase-5 implementation (profiles `ci` and `local`),
   kept as machine-readable artifacts, plus a qualitative comparison with the Plan 0005
   linear-depth baseline.
6. **Extraction plans.** Reviewed, not implemented, for BEAR-B, `tkgl-smallpedia` and
   `thgl-software`.

## Non-goals
None of the following is in this plan:
- checkpoints, reconstruction caches, change indexes, checkpoint tables, migrations or
  formats;
- ancestry or reconstruction rewrites;
- new production APIs (no diff or commit endpoints);
- GNN or ML frameworks;
- external dataset downloads;
- BEAR, TGB, OGB, Software Heritage or LDBC integration (M3 and later);
- hard performance-regression gates;
- SHACL, pySHACL, Jena or reasoning in the harness;
- any change to production semantics, canonical bytes, identities, limits or migrations.

## Invariants
- Every Phase 0–5 invariant and identity is unchanged, and no production crate gains
  benchmark behaviour.
- The benchmark measures the production stack (distroless image, owner migration, runtime
  identity) through its public HTTP API.
- Owner-identity reads serve two roles only, and are labelled as such:
  - provisioning, the operator path `ledger-admin` also uses;
  - persisted-state assertions (commit parents and provenance through
    `ImmutableStore::get_commit`, and database size).
- The oracle derives expectations from the generator's own construction (set algebra over
  generated statements; merge outcomes for designed conflicts). It never calls
  `ledger-rdf`, `ledger-dag` or `ledger-merge` to compute an expectation. The single
  exception is labelled: the state digest (`sculpin-rdf-state/v1`, a frozen protocol
  function) is used to compare preview digests with expected states.
- Correctness assertions fail the run. Timing is an observation, never a gate, in this plan.

## Persistent data changes
None. No migration, and no canonical-format or identity change.

## Affected packages
- New: `apps/ledger-bench`. It is a workspace member and qualification tooling, never
  shipped (like `ledger-stress`).
- Docs: `docs/benchmarks/`, `docs/quality/performance-testing.md`,
  `product_development_plan.md` (roadmap reconciliation), `benchmark/` (manifests and
  profiles).
- CI: `.github/workflows/ci-benchmark.yml`. Scripts: `scripts/benchmark.sh`.

## Runtime budgets
| profile | budget | notes |
|---|---|---|
| `ci` (PR) | run phase < 5 min preferred, < 10 min hard | image build, harness build and run are reported separately |
| `local` | minutes, workstation | deeper history, for baselines |

The CI budget is measured, not assumed; any size adjustment is recorded with its evidence.

## Quality gates
- `check-fast`: fmt, clippy `-D warnings` (all targets and features), workspace tests, the
  architecture check, doc links, and the golden/reference vectors.
- `check-supply-chain`: no new external dependency is planned; if any appears, justify it
  and run the gate.
- `ledger-bench` unit tests: generator determinism, manifest checksum, oracle self-checks,
  and result schema and report.
- `scripts/benchmark.sh ci` locally, plus the `local` profile for the baseline.
- `test-integration.sh`. No production code changes are planned; it runs anyway because
  the PR touches CI.
- PostgreSQL 15/17 suites only if database-facing code changes (none planned).
- Hosted CI, including the new `ci-benchmark` job.
- Independent reviews: architecture/separation, RDF correctness and oracle validity,
  performance methodology, CI/reproducibility, security/supply chain.

## Work
- [x] M0 assessment and `BENCHMARK_ARCHITECTURE.md`; reviewed (architecture review, no P0/P1)
- [x] M1 harness (`apps/ledger-bench`): dataset lifecycle, client, result schema, report, provenance
- [x] M2 generator, oracle and runner; unit tests; local `ci` run green; mutation-tested
- [x] CI workflow `ci-benchmark`: hosted run green, with its duration recorded (below)
- [x] Baselines: `ci`, `local` and the Plan 0005 constant-state control on the unchanged implementation
- [x] Extraction plans (`DATASETS.md`): BEAR-B, `tkgl-smallpedia`, `thgl-software`; reviewed (security review)
- [x] Docs: `docs/benchmarks/*`, performance-testing, roadmap reconciliation
- [x] Reviews (5, read-only, Opus); every confirmed finding fixed or recorded below
- [x] Gates and evidence; **stopped for review**: no M3 and no checkpoint work

## Decisions
1. **The harness is a new workspace application, `apps/ledger-bench`.** It is not an
   extension of `ledger-stress`, which is a concurrency and fault tool with no dataset or
   oracle abstraction. It is not Python either: the repository is Rust-first and the ledger
   client, provisioning and persisted reads are Rust. About 40 lines of token and
   percentile code are duplicated rather than coupling the two tools.
2. **The measured path is the production-shaped Compose stack through the public API.**
   This is the same topology as `scripts/bench.sh`. Owner-identity reads (provisioning,
   `get_commit`, sizes) and the `algorithm` category (`ledger_rdf::diff` on
   ledger-materialized states) are labelled. No production API was added; the public API
   has no diff or commit endpoint.
3. **The oracle is independent.** It uses set algebra, a brute-force ancestry reference and
   merge outcomes by design, with design-premise assertions. The only ledger function used
   on the oracle side is the frozen `sculpin-rdf-state/v1` digest, and that use is
   labelled.
4. **`benchmark/` (data) is kept and evolved, and no `benchmarks/` tree is created.** Code
   is under `apps/`, docs under `docs/benchmarks/`.
5. **Dataset validity is a gate.** A committed manifest per dataset, compared field by field
   and strictly (exit 3). Every profile's manifest is checked in CI.
6. **Timings are observations.** No regression gate exists until repeated-run variance is
   known (`METRICS.md` regression policy).
7. **Size adjustments.** The `ci` profile keeps the planned sizes: about 1,000 entities,
   6,100 initial quads and 205 commits. Its measured run phase is about 30 s locally. The
   9 branches are the 4–8 work branches plus one helper branch for the criss-cross.

## Discoveries
- **Request limit.** The first `local` run stopped at its genesis with
  `413 RESOURCE_LIMIT (10,000 operations per request)`. The initial load is now split into
  bulk commits of at most 5,000 statements; limits are never raised.
- **The ledger refused an unreachable branch point**, a generator design error: `crisscross`
  was created from `main` at a commit `main` cannot reach (`422 BRANCH_POINT_UNREACHABLE`).
  The generator now names the right source and asserts reachability itself.
- **Mutation evidence** (local only; production code restored and unchanged). Each
  mutation fails `scripts/benchmark.sh ci`:
  - a delete-honouring `union` gives 82 failures, starting at the keep-and-add vs delete
    slots;
  - a reconstruction fold that drops named-graph deletes gives 286 failures.

  `ledger-admin verify` stays `VERIFY OK` under both, because it recomputes with the same
  code. The benchmark's independent oracle catches what the production verifier cannot.
- **Reviews of `34b17cc`** (architecture/separation, RDF correctness/oracle validity,
  performance methodology, CI/reproducibility, security/supply chain; read-only, Opus): no
  P0. The P1s and the correctness-relevant P2s were fixed in `a68865e`:
  - **oracle coverage gaps that would let a wrong ledger pass:** union vs delete-honouring
    merge, NO_CHANGE in one direction only, a base reachable only through a second parent,
    criss-cross, reverse delete-vs-modify, multi-valued add/add, duplicate or unordered
    state lists, and an optional expected digest;
  - **checks:** pure, with negative tests;
  - **manifests:** the `local` manifest is now validated in CI, the checksum covers
    provenance, and manifests are strict;
  - **provenance:** untracked files, input digests and acceptance mode are now recorded;
  - **runner and script:** verify's exit code is checked, a failed `annotate` no longer
    skips verify, and the samplers are stopped on teardown;
  - **guards:** owner-DSN loopback, no proxy, and `annotate` path handling;
  - **methodology:** bulk commits are labelled; `fold_ops`, response bytes and logical
    object bytes are recorded; there are no tail percentiles below 20 samples;
    comparability wording is corrected; the warm-only and depth-versus-state confounds are
    documented;
  - **architecture:** no workspace member may depend on `ledger-bench` or `ledger-stress`,
    and `Dataset::prepare` is fallible.
- **Not fixed (recorded):**
  - The performance P1s "depth signal below noise in `ci`" and "growth vs churn do not
    separate depth from state size" are properties of a correctness-first CI profile. The
    `local` profile and the Plan 0005 constant-state control provide the depth signal; a
    dedicated depth and state sweep is the next measurement (below).
  - Cold-cache, server-side time, PostgreSQL I/O and CPU measurements need instrumentation
    or a dedicated configuration (next measurements).

## Findings: pre-optimization baseline of Phase 5
These are observations, not gates. The authoritative numbers are in
[`docs/quality/evidence/benchmarks/2026-10-06-phase5-pre-optimization/`](../../quality/evidence/benchmarks/2026-10-06-phase5-pre-optimization/README.md).

- **Reconstruction is linear in parent-0 depth, and Phase 5 did not change its cost.**
  - The constant-state control (one-quad state) gives state reads of 0.9, 20.4 and
    187.8 ms p50 at depths 1, 100 and 1,000. Plan 0005 measured 0.8, 31.0 and 201.2 ms.
    Both are ≈0.19 ms per ancestor.
  - On the `local` synthetic history (12k–13.7k quads), head state reads rise from ≈38 ms
    p50 at depth < 10 to ≈156 ms at depth 500–999, with `prepare` tracking them. Over a
    state-size intercept of ≈38 ms (the 12k-quad state as JSON plus folding the bulk
    load), that is ≈0.2 ms per ancestor.
  - Growing-state (`growth`) and constant-state (`churn`) histories of equal depth are
    indistinguishable. At these sizes depth dominates, not the state-size change.
- **Merges cost about three reconstructions.** At parent-0 depths of 200–600, merge preview
  p50 is ≈598 ms and propose ≈657 ms; apply is ≈4 ms (stored values only, ADR-0024). In
  `ci` (depth ≤ 64) preview is ≈140 ms. Accept and ref reads are flat (≈4 ms and ≈1 ms).
- **Ingest.** About 5 ingest steps per second on the serial `local` history (each step
  includes its state check), and about 11.5 per second in `ci`.
- **Storage.** In `local`, `immutable_objects` holds 1.76 MB logical and 2.0 MB on disk.
  The database grew 12.2 MB in total. That growth is not broken down by table; per-table sizes are a listed next measurement.
- **Memory.** The server's peak sampled `memory.current` was 84 MiB (`local`) and 45 MiB
  (`ci`); the harness peaked at 157 MiB (`local`), mostly the oracle.
- **Comparison with the old linear-depth benchmark.** The per-ancestor slope agrees. The
  intercepts differ by design: Plan 0005 used a 1-quad state, the synthetic profiles start
  from 6–12k quads. The synthetic profiles add branches, merges and historical reads that
  the old benchmark never covered.

**Measured bottleneck.** Each reconstruction walks every parent-0 ancestor, with one object
read and one re-hash per commit and per patch, from genesis. A merge does this three times
(base, target, source) and walks both ancestries in full. At depth about 600 a single read
is ≈150 ms and a merge preview ≈0.6 s. At the development `max_depth` of 10,000, linear
extrapolation (a hypothesis, not measured here) gives ≈2 s per read, which matches Plan
0005's measured 1.98 s, and ≈6 s per preview.

**Recommended next experiment (not implemented here):**
1. Integrate `bear-b-ci` unchanged (M3).
2. Add a **depth × state-size sweep**: depths 1–5,000 at fixed states of 1, 1k and 10k quads,
   about 20 repeats, recording `fold_ops`, bytes and server time.
3. Add a **cold-cache arm**: PostgreSQL restarted, OS cache dropped.
4. Capture **PostgreSQL I/O per operation**: `pg_stat_statements` and `track_io_timing` in a
   benchmark-only database configuration.
5. Capture **server CPU** from cgroup `cpu.stat` deltas.

Together these separate per-ancestor round trips from per-quad folding and decoding, which
is the input the checkpoint ADR needs: checkpoint interval from depth and p95, and the
expected benefit of batching the ancestor fetch versus snapshotting.

## Evidence
All runs on 2026-10-06, on the qualification workstation (12 × i7-4930K, 15 GiB,
Linux 5.10, Docker; Compose PostgreSQL 17.2). Production code is identical to `c7a94d5`
(`git diff c7a94d5 -- crates apps/ledger-server apps/ledger-projector` is empty).

| gate | result |
|---|---|
| `check-fast` on `a68865e` (fmt, clippy `-D warnings` all targets and features, workspace tests, architecture incl. the new qualification-tool rule, doc links, goldens) | exit 0; 230 passed, 0 failed (181 ignored: the PostgreSQL suites and the slow `local` generation test, which `validate --profile local` covers) |
| Golden and reference vectors | 18 commit v2, 26 request, 6 merge preview-token, 42 validation and 3 state-digest vectors match; no fixture changed |
| `ledger-bench` unit tests | 18 passed, 1 ignored (generation determinism, shape, oracle self-consistency, independent slot-by-slot merge reading, manifest strictness, checksum sensitivity, negative state and preview checks, report rendering, loopback guards) |
| `scripts/benchmark.sh ci` on `a68865e` | `BENCHMARK PASS`: 1,115 assertions, 0 failed; `VERIFY OK`; run phase 27 s; dataset validation (both profiles) 14 s |
| `scripts/benchmark.sh local` on `a68865e` | `BENCHMARK PASS`: 4,945 assertions, 0 failed; `VERIFY OK`; run phase 329 s |
| `scripts/bench.sh 1,100,1000 20 --constant-state` on `a68865e` | complete (`linear-depth-constant-state/` in the evidence folder) |
| Mutation runs (local, not committed) | delete-honouring union: 82 failures; fold dropping named-graph deletes: 286 failures (exit 1 both) |
| `check-supply-chain` | not required: no new external crate (`Cargo.lock` gains only the `ledger-bench` package). It runs again in hosted `ci-security` |
| PostgreSQL 15/17 suites | not run: no database-facing production code changed |
| `test-integration.sh` | runs in hosted `ci-integration`; no production path changed |
| Hosted CI, PR #12 head `6b1902d` | all green: ci-fast 37522776889, ci-security 37522776971 (dependency-review, supply-chain, container), ci-integration 37522777076 (docker), ci-fuzz 37522777014 (address, none), **ci-benchmark 37522777454** |
| **Official `ci` result** (hosted, clean checkout of the PR merge ref `b12b0f1`; `tracked_changes=0`, `untracked_files=0`) | `BENCHMARK PASS`: 1,115 assertions, 0 failed; `VERIFY OK`. Both manifests validated, with checksums identical to the workstation's (cross-machine determinism). Job 5 min 4 s: harness build 90 s, validation 5 s, stack build and start 174 s, **run 11 s**, verify 2 s. Server peak 42 MiB and PostgreSQL 124 MiB (cgroup `memory.peak`). Host: 4 × AMD EPYC 9V45, 16 GiB, kernel 6.17. The result is the `benchmark-ci` artifact of run 37522777454 |

Explicitly not done in Phase 6A:
- checkpoints, caches, change index, migrations;
- M3 (BEAR) or any external download;
- GNN work;
- semantic validation in the benchmark. The stack runs with the development
  unvalidated-acceptance switch, recorded as `acceptance_mode`.
