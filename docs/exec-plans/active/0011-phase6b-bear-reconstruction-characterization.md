# Plan 0011: Phase 6B — BEAR-B, reconstruction characterization, qualification hygiene

Status: **in progress** (started 2026-10-06). Branch `claude/p6b-bear-reconstruction` from
`main` at `646b0291c2dcd0f50089939ab2622f02dc47d538`. That commit is the PR #12 Phase-6A merge;
its tree is identical to the reviewed head `9f593d2`, see
[Plan 0010](../completed/0010-phase6a-benchmark-foundation.md). No gate is reported as passed
until it is executable and has run.

## Primary question
**What dominates historical reconstruction cost as ancestry depth and RDF state size grow,
and therefore which Phase-6 optimization comes first?** The candidates are batching the
ancestor/object fetch, checkpoints, both in some order, a cache, or another measured
bottleneck. The evidence decides; checkpoints are not assumed.

## Measurement questions
1. How does a single reconstruction split between the following?
   - ancestry and object fetch: PostgreSQL round trips, calls, rows, shared block hits and
     reads, block read time;
   - hash verification and decoding;
   - folding patch operations;
   - state materialization;
   - HTTP serialization, transfer and client decoding.
2. How does each part scale with **ancestry depth at a fixed state size** (1, 1,000 and
   10,000 quads; depths 1–5,000), and with **state size at a fixed depth**?
3. How do warm and **database-restart cold** reconstructions differ? OS page-cache cold is
   claimed only if the host cache is actually dropped, which this workstation does not do.
4. How much server and PostgreSQL CPU, and how much memory, does each part use?
5. How much of a merge preview is ancestry traversal, and how much is its three
   reconstructions?
6. On genuine RDF evolution (BEAR-B), are reconstruction and diff exact, and what do they
   cost?

## Scope
1. **Qualification hygiene, done first.**
   - Fix the runbook's schema-version drift, plus a doc-consistency check.
   - Fix the generator-version drift.
   - Bound `ci-integration` with a timeout.
   - Run expected-failure migrations on a dedicated connection (the pg_graphs_migration
     hang).
   - Add `ci-benchmark` to the required-checks recommendation.
2. **M3: `bear-b-ci`.**
   - Verify the source, license and checksums.
   - Add a typed, versioned external-dataset manifest.
   - Build an offline lifecycle: fetch → verify → prepare → verify → run, with safe
     archive handling.
   - Extract deterministically, cross-checking the full versions against the change sets.
   - Use the source versions as the oracle.
   - Add it to the `ci` profile only within the CI envelope.
3. **Reconstruction diagnostic profile (`recon`, outside PR CI).** Depth × state sweeps.
   The API path is measured against the direct store path (`persisted`), with prepare,
   merge preview and ancestry traversal separately. It uses a benchmark-only PostgreSQL
   configuration with `pg_stat_statements` and `track_io_timing`, cgroup CPU and memory, and
   warm versus database-restart cold.
4. **Production-qualification matrix**: a read-only categorization of the remaining
   blockers.
5. **A recommendation** for the first Phase-6 optimization, plus the inputs to the
   checkpoint ADR if checkpoints are recommended.

## Non-goals
None of the following is in this plan:
- checkpoint tables, objects, selection or interval policy;
- a reconstruction cache, generation numbers, ancestor batching or a persistent change
  index;
- new history or diff APIs, merge CPU rewrites, or `spawn_blocking`;
- `tkgl-smallpedia`, `thgl-software`, OGB or GNN work;
- raising API limits;
- committing raw or extracted DBpedia data before redistribution is reviewed;
- changing production database defaults for instrumentation;
- benchmark-only production endpoints.

## Invariants
- Every Phase 0–6A identity, limit and semantic is unchanged, and there is no migration.
- `run` stays offline. A missing or invalid cache is a clear failure, never a download.
- Oracle independence:
  - BEAR expectations are the normalized source versions;
  - the synthetic generator keeps its own set algebra;
  - category labels (`api`, `persisted`, `algorithm`) stay honest.
- The instrumented PostgreSQL configuration is recorded in every result that uses it.
- Timings never gate.

## Dataset provenance requirements
External manifest (`sculpin-ledger-bench-manifest/v2`): publisher, version, URLs, license
and attribution, source SHA-256 and size (pinned on the first reviewed download), retrieval
date, extraction algorithm and version, parameters, version range, counts, blank-node
skolemization count, output SHA-256 and artifact size. Strict parsing. Synthetic manifests
stay deterministic and valid.

## Gates
- `check-fast`: fmt, clippy, tests, architecture, doc links, the new doc-consistency check,
  goldens.
- `ledger-bench` unit tests.
- Benchmark runs: synthetic `ci` and `local`, `bear-b-ci`, the `recon` profile, and
  `ledger-admin verify`.
- PostgreSQL 15 and 17 suites, with the pg_graphs_migration suite repeated for
  non-recurrence.
- Integration, upgrade 0012 → 0013, supply chain if dependencies change.
- All six hosted workflows.
- Independent reviews: BEAR/RDF temporal correctness, oracle validity, performance
  methodology, PostgreSQL measurement, archive and dataset security, licensing and
  provenance, CI reliability, production-qualification gaps.

## Stop condition
Every item of the Phase-6B gate in the task holds, the recommendation is written, and work
**stops**: no checkpoint implementation.

## Work
- [ ] Hygiene: runbook, generator version, doc-consistency check, CI timeout, dedicated-connection migration tests, required checks
- [ ] BEAR-B source verification and first reviewed download (SHA-256, size, date)
- [ ] External manifest v2 with negative tests
- [ ] Lifecycle commands (fetch, verify, prepare, verify, clean) with safe archive handling
- [ ] Extraction with the CB cross-check; oracle; `bear-b-ci` runs
- [ ] `recon` profile; PostgreSQL instrumentation override; cold arm; CPU and memory
- [ ] `bear-b-ci` in `ci` if within budget
- [ ] Production-qualification matrix
- [ ] Reviews; gates; evidence; recommendation; stop

## Decisions

## Discoveries

## Evidence
