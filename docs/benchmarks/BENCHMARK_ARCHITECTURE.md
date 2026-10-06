# Benchmark architecture (Phase 6A, M0)

Status: accepted for Plan 0010 ([execution plan](../exec-plans/completed/0010-phase6a-benchmark-foundation.md)).
Requirements: [INTEGRATION_PLAN.md](INTEGRATION_PLAN.md) (the supplied benchmark programme).
This document records the M0 assessment and the boundaries every later benchmark milestone
must keep.

## 1. Assessment of the current implementation (main at `c7a94d5`)

| concern | production path | what a benchmark can reach |
|---|---|---|
| Immutable commits and patches | `ledger-core` (`CommitV2`, `AnyCommit`, `PatchId`); `ledger-store` `PostgresImmutableStore` (`immutable_objects`, `commit_index`, `commit_parents`) | written only through prepare/accept or merge propose; read through `ImmutableStore::get_commit` (hash re-verified). The public API has no commit endpoint |
| Reconstruction | `WorkflowRepository::state_at_on`: walks the parent-0 chain from the head to genesis, fetching and re-hashing each commit and patch object (one row read each), then folds the patches; bounded by `ReconstructionLimits` | `GET …/commits/{c}/state` (and implicitly every prepare, which reconstructs the parent) |
| DAG traversal | `ledger-dag`: `first_parent_history`, `is_ancestor`, `analyze_with_ancestries` (merge base, ahead/behind; bounded visits and deadline) | `GET …/branches/log` (first-parent history), merge preview (base, ahead/behind) |
| Branches | `postgres_branches.rs`, migration 0012 (`branches`, `branch_events`) | `POST/GET …/branches*` |
| Merges | `ledger-merge` (`structural-slot/v1`, the preview token), `postgres_merge.rs` (preview, propose, apply), migration 0013 | `POST …/merges/{preview,propose,apply}` |
| State digest | `ledger_rdf::state_digest` (`sculpin-rdf-state/v1`, golden-pinned plus Python reference) | inside merge preview, validation and projection |
| Diff | `ledger_rdf::diff` (pure set difference over canonical quads) | not exposed by the API; merge preview exposes delta *summaries* |
| Provenance and history | commit envelope (activity, message, evidence, actor, recorded_at), `ref_events`, `branch_events`, `decisions` | envelope through `get_commit`; branch history API |
| Persistence | PostgreSQL migrations 0001–0013; owner, runtime and projector identities (ADR-0016) | the compose stack (`compose.yaml`) is the production-shaped topology |
| Existing performance tooling | `apps/ledger-stress` (`stress`, `fault`, `branches`, `bench` modes); `scripts/bench.sh` (Plan 0005 §10 linear-depth baseline); evidence in [performance-baselines.md](../quality/performance-baselines.md) | concurrency and depth measurements; no dataset or oracle abstraction |
| CI | `ci-fast` (fmt, clippy, tests, goldens, architecture), `ci-integration` (compose stack, PG suites, Fuseki), `ci-security`, `ci-fuzz`, `ci-differential` (scheduled) | Docker is available on hosted runners; `ci-integration` builds the image in about 9 minutes in total |
| Test infrastructure | `ledger-testkit` (patch builders); per-crate PostgreSQL suites | not a benchmark harness |
| Languages and ML | Rust workspace; Python only for golden reference encoders and harness glue; no ML framework | — |

Findings that shape the design:
1. **The measured path must be the public API on the production-shaped stack.** It is the
   same topology the Plan 0005 baseline used. The workloads differ, though, so the numbers
   are not directly comparable. Plan 0005 used a 1–10,000-quad linear history with a 1-quad
   control; the synthetic profiles use a 6–13k-quad genesis and branched histories.
2. **No public diff, commit or parents endpoint exists.** Adding one only for benchmarks is
   out of scope (Phase 6 "history lookup APIs" may add one later on its own merits). Diff
   correctness is therefore checked by running the production `ledger_rdf::diff` on
   ledger-materialized states. Parents and provenance are checked through the production
   `ImmutableStore::get_commit`, under the owner identity. Both categories are labelled.
3. **Reconstruction cost is per parent-0 ancestor** (two object reads and two re-hashes
   each). Shallow-versus-deep and growing-versus-constant state are the axes that matter,
   so the synthetic dataset includes both.
4. **`ledger-stress` is a concurrency and fault tool.** It has no dataset lifecycle,
   oracle or result schema. Extending its 4,000-line binary would mix the two concerns, so
   the benchmark harness is a separate application. It duplicates about 40 lines of
   token minting and percentile code instead of coupling the two tools.

## 2. Subsystem boundary

```text
benchmark/                         data (no code): manifests, later profiles/configs
  datasets/<dataset-id>.json       committed manifest per dataset (checksum, seed, license, ...)
apps/ledger-bench/                 the harness (Rust workspace application, never shipped)
  src/dataset.rs                   dataset lifecycle trait, profiles, manifest verification
  src/synthetic.rs                 synthetic-ledger-* generator and independent oracle
  src/workload.rs                  dataset-independent workload (steps + expectations)
  src/runner.rs                    execution against the running stack; assertions; timing
  src/result.rs                    result schema (JSON) and the report rendered from it
scripts/benchmark.sh <profile>     brings up the compose stack, runs, annotates, verifies
docs/benchmarks/                   this documentation
.github/workflows/ci-benchmark.yml PR job for the `ci` profile
```

- **No production crate depends on `ledger-bench`, and no production code changed for it.**
  `scripts/check-architecture.py` enforces that no workspace member depends on
  `ledger-bench` or `ledger-stress`. The Dockerfile builds only `ledger-server` and
  `ledger-projector`.
  The harness depends on the ledger crates only for:
  - provisioning (`PgGraphs`, the operator path);
  - persisted-state reads (`get_commit`);
  - the labelled `algorithm` category (`ledger_rdf::diff`);
  - one labelled oracle-side protocol function (`ledger_rdf::state_digest`, frozen and
    golden-pinned).
- The existing `benchmark/` directory is kept and evolves: it was a placeholder README.
  It now holds dataset manifests. No competing `benchmarks/` tree is created; code lives
  under `apps/`, like every other application.
- `scripts/bench.sh` and `ledger-stress bench` stay as the **historical linear-depth
  baseline** (Plan 0005 §10). The numbers they produced are preserved unchanged in
  `performance-baselines.md`. New profiles use `scripts/benchmark.sh`.

## 3. Correctness-oracle boundary

The oracle lives in `synthetic.rs` and shares no logic with the ledger:
- **States** are set algebra over the generator's own statements.
- **Ancestry, merge base and ahead/behind** are a brute-force reference over the symbolic
  DAG (ancestor sets and their maximal common elements): the ADR-0024 definition, not
  `ledger-dag`.
- **Merge outcomes** come from construction. Scenarios are designed so that target and
  source touch disjoint slots apart from deliberately conflicting or convergent ones.
  - Outside the designed conflicts, the expected merged state is `(T − (B − S)) ∪ (S − B)`.
  - Inside them, it follows the strategy's definition.
  - The premise is asserted whenever a merge is generated: an undesigned conflict is a
    generator bug and panics. It is never absorbed into the expectation.
- **State fingerprints** use the oracle's own SHA-256 over sorted lines, not
  `sculpin-rdf-state/v1`.

What the oracle deliberately does **not** do: re-implement reconstruction, the per-slot
merge algorithm or the DAG walk. If it did, it would share their blind spots. Semantic
validity (SHACL, reasoning) is Sculpin's. The harness contains no validator, and this pass
exercises no validation; the compose stack runs with the development
unvalidated-acceptance switch, as `scripts/bench.sh` does.

## 4. Dataset lifecycle and adapters

```rust
trait Dataset {
    fn id(&self) -> &'static str;
    fn prepare(&self) -> Result<Workload, String>;   // deterministic, offline; fails on a missing or unverified cache
    fn manifest(&self, w: &Workload) -> Manifest;    // must equal the committed manifest
}
```

A `Workload` is ordered steps: commit (adds/deletes as canonical N-Quads, with provenance),
branch (at a commit), and merge (previews with expectations, optional apply). Expectations
are keyed by symbolic labels, because commit ids are content-addressed and only known after
the ledger creates them. The format suits each kind of dataset without a fake universal
schema:

| dataset kind | adapter output |
|---|---|
| generated (synthetic) | full DAG with designed merges |
| BEAR versions | a linear chain `V0 → … → Vn`; expected state = the source version |
| TGB temporal facts and events | time-ordered commits of rendered facts; the expected state is a deterministic replay of the source events (see [DATASETS.md](DATASETS.md)) |

External datasets (`bear-b-ci`, Plan 0011) add `fetch` (download plus pinned SHA-256 and
size, outside the run) and `prepare` (offline extraction and cross-checks into a hash-pinned
artifact). The run never touches the network. Manifests are
`sculpin-ledger-bench-manifest/v2` ([REPRODUCIBILITY.md](REPRODUCIBILITY.md#manifests)).
`ledger-bench recon` is a separate diagnostic mode outside PR CI
([METRICS.md](METRICS.md#reconstruction-characterization)).

## 5. Profiles and execution model

| profile | datasets | where | budget |
|---|---|---|---|
| `ci` | `synthetic-ledger-ci`, `bear-b-ci` (later plus `tkgl-smallpedia-ci`, `thgl-software-ci`) | every PR (`ci-benchmark`) | run < 5 min preferred, < 10 min hard |
| `bear` | `bear-b-ci` | workstation | about a minute |
| `recon` (`scripts/benchmark-recon.sh`) | constant-state depth × state-size histories | workstation | ≥ 1 h |
| `local` | `synthetic-ledger-local` (deeper history, for baselines) | workstation | minutes |
| `nightly` and `scale` | later milestones | dedicated | — |

Flow (`scripts/benchmark.sh`):
1. Build the harness.
2. Validate the dataset against its manifest; this fails fast, before any stack exists.
3. `docker compose up --build` the ledger and PostgreSQL (owner migration, runtime identity).
4. Run the profile through the API.
5. Annotate the container peak memory from the host cgroup.
6. Run `ledger-admin verify` (must be `VERIFY OK`).

The exit status is non-zero on an invalid dataset (3), any correctness failure or aborted
run (1), or a failed verify.

## 6. Result schema

`result.json` uses `sculpin-ledger-bench-result/v1` ([METRICS.md](METRICS.md)): run
provenance, environment, and per dataset:
- the manifest and checksum;
- counts;
- correctness: assertion families with counts, and bounded failures;
- performance: phases, per-operation percentiles by category, the same split by history
  kind and depth, and raw series;
- resources: harness peak RSS, database size, `immutable_objects` size, and container peak
  memory annotations.

`report.md` is rendered only from the JSON (`ledger-bench report`). Numbers are never
maintained by hand in the documentation.

## 7. Reproducibility and artifacts

See [REPRODUCIBILITY.md](REPRODUCIBILITY.md). The ingredients:
- the build revision and tracked-change count;
- the rustc version and server image id;
- the seed, generator version and dataset checksum;
- the host OS, kernel, CPU and memory, and the PostgreSQL version.

Generated datasets are regenerated on every run and verified against the manifest; nothing
is cached. Extracted datasets will be cached as CI artifacts, keyed by their manifest
checksum (see [DATASETS.md](DATASETS.md)).

## 8. Future BEAR/TGB boundary

Adapters produce workloads; they never call the ledger themselves. Temporal ML/GNN code
(later milestones) consumes ledger exports, never ledger internals. It lives outside the
Rust harness: a separate Python package with its own lockfile, reviewed when it is
introduced.

## 9. Risks and open questions

- **Timing noise on shared hosted runners.** Phase 6A records observations only; a
  regression envelope needs repeated runs (see [METRICS.md](METRICS.md)).
- **The runtime identity cannot read sizes.** Database sizes are read under the owner
  identity, which is labelled.
- **No public diff or history-lookup API.** The diff checks are therefore in the
  `algorithm` category. If Phase 6 adds history lookup APIs, the checks should move to
  `api`.
- **Oracle design premise.** It holds by construction for synthetic data only. Extracted
  datasets need source-replay oracles (BEAR: versions are given; TGB: replayed event state).
- **Container peak memory** needs the host cgroup. On hosts where neither cgroup v2
  `memory.peak` nor v1 `max_usage_in_bytes` is readable it is recorded as `unavailable`.
