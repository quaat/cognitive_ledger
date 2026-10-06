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
- [ ] M0 assessment and `BENCHMARK_ARCHITECTURE.md`; review
- [ ] M1 harness (`apps/ledger-bench`): dataset lifecycle, client, result schema, report, provenance
- [ ] M2 generator + oracle + runner; unit tests; local `ci` run green
- [ ] CI workflow `ci-benchmark`; hosted run green, with duration recorded
- [ ] Baselines: `ci` and `local` profiles on the unchanged implementation
- [ ] Extraction plans (`DATASETS.md`): BEAR-B, `tkgl-smallpedia`, `thgl-software`
- [ ] Docs: `docs/benchmarks/*`, performance-testing, roadmap reconciliation
- [ ] Reviews; fix confirmed findings
- [ ] Gates; evidence; stop for review (no M3 and no checkpoint work)

## Decisions
(Recorded as made; see `BENCHMARK_ARCHITECTURE.md` for the rationale.)

## Discoveries

## Evidence
