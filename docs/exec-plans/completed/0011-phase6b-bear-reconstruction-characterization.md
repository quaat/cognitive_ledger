# Plan 0011: Phase 6B — BEAR-B, reconstruction characterization, qualification hygiene

Status: **complete** (2026-10-07; PR #13, awaiting the owner's merge). Branch `claude/p6b-bear-reconstruction` from
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
3. How do warm and **post-restart (`db-restart-first-ledger-op`)** reconstructions differ? OS page-cache cold is
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
   - Extract deterministically from the TB/CB lineage: TB (time-annotated versions) is the
     oracle, and every CB step is cross-checked against it. IC's divergent lineage is a
     checked, counted relation (IC ⊆ TB), not the oracle; see Decisions.
   - Use the source versions as the oracle.
   - Add it to the `ci` profile only within the CI envelope.
3. **Reconstruction diagnostic profile (`recon`, outside PR CI).** Depth × state sweeps.
   The API path is measured against the direct store path (`persisted`), with prepare,
   merge preview and ancestry traversal separately. It uses a benchmark-only PostgreSQL
   configuration with `pg_stat_statements` and `track_io_timing`, cgroup CPU and memory, and
   warm versus `db-restart-first-ledger-op`.
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
- [x] Hygiene: runbook, generator version, doc-consistency check, CI timeout, dedicated-connection migration tests, required checks
- [x] BEAR-B source verification and first reviewed download (SHA-256, size, date)
- [x] External manifest v2 with negative tests
- [x] Lifecycle commands (fetch, verify, prepare, verify, clean) with safe archive handling
- [x] Extraction with the CB cross-check; oracle; `bear-b-ci` runs
- [x] `recon` profile; PostgreSQL instrumentation override; cold arm; CPU and memory
- [x] `bear-b-ci` in `ci` if within budget (hosted `benchmark-ci` 4m55s–7m36s per job, benchmark run 32–44 s)
- [x] Production-qualification matrix ([production-qualification.md](../../quality/production-qualification.md))
- [x] Reviews; gates; evidence; recommendation; stop (no Phase-6C work on this branch)

## Decisions
1. **The BEAR-B oracle is the TB/CB lineage, not IC** (owner review requested).
   - The three BEAR-B day encodings do not describe one history. IC(1) plus the cumulative
     CB changesets equals TB at all 89 versions (88 steps). From version 2 on, the IC files
     also drop stale values that no changeset deletes.
   - IC ⊆ TB holds at every one of the 89 versions. The extractor checks all of them
     (`bear-b-day-extract/2`) and fails on any violation.
   - The extractor takes TB as the oracle and hard-checks two things:
     - anchor TB(v0) == IC(1);
     - at every step, CB's net change == TB's version difference. Changeset no-op churn is
       allowed only where both versions contain it.
   - IC's divergence inside the window is pinned as a manifest count
     (`ic_lineage_divergence_in_window`); the divergence over all versions is pinned as
     `ic_lineage_divergence_all_versions`.
   - TB and CB are two encodings of one lineage (BEAR likely derived TB from the
     changesets). Their agreement shows that the extraction reads both consistently, not
     that the lineage is "true".
2. **Window**: the 12 consecutive day versions with the most adds plus deletes, lowest start
   on ties. That is v22..=v33: 36,645 → 41,316 triples, 9,384 adds, 4,713 deletes, 199
   reappearances. The ingest is 19 commits (≤ 5,000 operations and ≤ 1.4 MB each).
3. **Archive handling in memory** (gzip capped at 512 MiB, minimal ustar reader, regular
   files only, no archive path ever written). A streaming design was not needed at 34 MB
   compressed.
4. **No third-party data in the repository or in uploaded results.**
   - Only the manifest (hashes, counts) is committed.
   - Failure details name BEAR statements by `stmt:<sha256 prefix>` (`redact_statements`).
   - The prepared artifact lives in the local or CI cache only.
5. **Instrumentation is benchmark-only.** `benchmark/compose.instrumented.yaml` enables
   `pg_stat_statements` (schema `bench_stats`) and `track_io_timing`. Every recon result
   records that override. Production defaults are unchanged.
6. **The post-restart condition is `db-restart-first-ledger-op`** (`docker restart` of
   PostgreSQL). It restarts the PostgreSQL processes and shared buffers. Before the measured
   operation, the server's `/ready` probe, the reconnecting pool and the window's
   statistics queries run, so the measured reconstruction is the first *ledger*
   reconstruction, not the first database operation. The OS page cache is warm, and no
   OS-cold claim is made.
7. **The pg_graphs_migration hang: cause unknown, the fix is defensive** (tech-debt entry).
8. **IC ⊆ TB is enforced for all 89 versions** (option A of the PR-#13 review), not only
   for the window; it costs about 10 s of release `prepare`.
9. **Required fuzz status:** the matrix job is `fuzz-sanitizer`, and a stable aggregate
   `fuzz` job (`if: always()`, succeeds only when every sanitizer job succeeded) is the
   required check (option A). `check-doc-consistency.py` models matrix expansion and
   self-tests the rule on fixtures.
10. **Official reconstruction evidence comes only from a clean checkout** of a recorded
    revision, with the default sweep (`official=yes`, set by `benchmark-recon.sh`, never by
    an argument). Both official runs were made from detached worktrees.
11. **Owner decision pending:** the unmodified public BEAR source archives are kept in a
    GitHub Actions cache entry that pull requests (including forks) can restore. This is
    recorded in the manifest's `redistribution` field and in `DATASETS.md`, for the owner to
    accept or reject.

## Discoveries
- **BEAR-B lineage.** Encoding facts:
  - IC and CB/TB diverge as described in Decisions 1.
  - 13 TB triples are written across 733 annotation lines with disjoint version lists.
    They are merged, and overlapping lists fail.
  - Normalization rewrote 17,451 source lines to canonical N-Quads. The count covers TB,
    CB and all 89 IC files, each parsed once, with 0 collisions. The count bounds
    what the shared canonicalizer could mask.
- **A failed sqlx migration keeps its advisory lock on the connection it ran on.** sqlx
  0.8 does not unlock on error. A pooled connection carries the lock into later use;
  `a_failed_migration_keeps_its_advisory_lock_on_a_pooled_connection_only` pins this.
- **Required status checks are job names**, not workflow names. The tech-debt
  recommendation and `check-doc-consistency.py` were corrected.
- **Stale documentation found by review:**
  - `deployment.md` contradicted ADR-0017 on PITR;
  - tech-debt claimed that zero DB timeouts were accepted, but the server rejects them.

## Recommendation (primary question)

**A. Batch ancestor and object retrieval first.** This choice comes from the measurements,
not from architectural preference. Two official runs (clean worktrees `4eb4d28` and
`e353b6c`), 144 points each, are in [run 1](../../quality/evidence/benchmarks/2026-10-07-phase6b-recon-official/recon.json)
and [run 2](../../quality/evidence/benchmarks/2026-10-07-phase6b-recon-official-run2/recon.json).
The run-2 windows are free of healthcheck statements: API calls equal store calls + 2 at
every point. Slopes are least-squares fits over depths 100–5,000; "run 1 / run 2" ranges
span S = 1, 1,000 and 10,000.

| Quantity | Run 1 | Run 2 |
|---|---|---|
| API state-read latency per ancestry level | 189–225 µs | 206–213 µs |
| PostgreSQL statements per ancestry level (state read, store, prepare) | 2.00 | 2.00 |
| PostgreSQL execution time ÷ end-to-end latency, depth 5,000 | 14–15 % | 14–15 % |
| Execution time per ancestry level | 28–32 µs | ≈ 30 µs |
| Prefetched fold CPU per level (lower bound) | 10.3–11.0 µs | 10.3–11.0 µs |
| Prefetched fold ÷ API latency, depth 5,000 | 4.6–7.7 % | 4.9–7.6 % |
| API state read p50, depth 5,000 (S = 1 / 1k / 10k) | 1,129 / 956 / 1,022 ms | 1,063 / 1,058 / 1,049 ms |
| State size at depth 1 (S = 1 / 1k / 10k) | 1.1 / 3.9 / 34 ms | 0.9 / 10.5 / 30 ms |
| Merge ancestry walk (contained preview), per level | 324–331 µs, 4 statements | 328–334 µs, 4 statements |
| Divergent merge preview, per level | 947–972 µs, 10 statements | same |
| Divergent − contained at depth 5,000 | 2.8–3.4 × one state read | 2.9–3.0 × |
| Ancestry share of a divergent preview | 33–34 % | 35 % |
| After a database restart vs warm, depth 5,000 | within noise (n = 3); ≈ 1,200 block reads, 13–15 ms read time | same |

**Reading:**
- **Statements grow exactly linearly.** Reconstruction issues two PostgreSQL statements per
  ancestor (`state_at_on`: one for the commit object, one for the patch). The merge
  ancestry walk issues two per commit per side.
- **Neither query execution nor the fold explains the per-level cost.** Execution is about
  30 µs per level and the fold about 10 µs (a lower bound), while latency is about 200 µs
  per level. The remaining ≈ 85 % is per-statement round-trip overhead: protocol, driver,
  scheduling and network, on both the server and PostgreSQL sides. The container CPU
  figures (server ≈ 120 µs/level, PostgreSQL ≈ 130 µs/level) agree, but they are
  whole-container and not a latency breakdown.
- **The fold is not the problem.** Optimizing the fold alone could save at most about 5 % at
  depth.
- **State size matters only at shallow depth.** At depth 1 it costs up to ≈ 30 ms (fold
  ≈ 24 ms and JSON at 10,000 quads). At depth 5,000 it has no systematic effect.
- **Storage I/O is not the cost here.** A post-restart read adds ≈ 1,200 block reads
  (13–15 ms in total) and is otherwise within noise of warm. The OS page cache was not
  dropped.
- **A merge preview is about one third ancestry walk and two thirds three reconstructions.**
  Both are the same per-statement pattern, so batching addresses both.
- **What transfers:** the scaling shape (statements per ancestor, linearity) transfers to
  other deployments. The absolute per-round-trip cost (≈ 100 µs on this host's container
  network) does not. Over a real network or with TLS, round trips are expected to cost
  more, which strengthens the case for batching.

**What batching must preserve** (Phase-6C inputs):
- **Same states and same identities.** Reconstructed states, state digests, commit and patch
  bytes, reconstruction limits, v1/v2 readability, merge reconstruction behaviour and the
  error taxonomy all stay unchanged.
- **Every fetched object is still verified.** Its SHA-256 is checked against its id, and it
  is decoded by the production canonical decoders.
- **Indexes are only hints.** The first-parent chain can be discovered with a bounded
  recursive query over `commit_parents`, but each decoded commit's `parents[0]` must equal
  the next id in that chain, and any mismatch is corruption.
- **Memory stays bounded.** Fetches happen in windows of k ancestors, never as one
  load-everything query.
- **Ancestry walks are batched too**, both the merge-base and the contained-check walks.

**Not supported yet:**
- **Checkpoints.** The linear cost that remains after batching (fold ≥ 10–11 µs per level,
  plus batched transfer and verification) has not been measured.
- **A reconstruction cache.** It was never measured.
- **Fold micro-optimization.** The fold is ≤ 8 % of the cost.
- **PostgreSQL tuning.** Execution is ≈ 15 % and warm block reads are 0.

**CHECKPOINT-ADR-READY: NO.** The missing evidence:
- the per-level residual of reconstruction and merge after batching, measured with the same
  `recon` profile;
- a target depth and latency budget for production histories. Today `max_depth` is 10,000
  and doubles as the write ceiling: see the production-qualification matrix.

Checkpoints become justified if the post-batching residual is still material at the target
depth. A rough projection from the fold lower bound puts it at ≥ 0.1 s at depth 10,000 and
≥ 1 s at 100,000, but this is a hypothesis until it is measured.

## Reviews (independent, bounded sub-agents)
| Review | Result | Resolution |
|---|---|---|
| BEAR / temporal RDF + oracle independence | no P0/P1 | Every timed recon result is now verified exactly. API timing includes decoding. The oracle's ledger dependencies are labelled, and the "independent" wording is gone. The invalid-line count and the IC-1 double count are fixed. Accepted: meaning-changing canonicalization without a collision is undetected (labelled). |
| Benchmark architecture + CI reliability | no P0/P1; P2 `official` overridable | `official` is set only by the script, and custom arguments make a run non-official. The cache key covers the source section, with a verified fallback. The per-attempt fetch timeout is 300 s, and fetch exits with 4. Empty lists and `clean` flag order are fixed. Aggregate detection is tightened. Accepted: a single source host (P2, recorded); tokio deadlines do not bound blocking hangs (tech-debt). |
| Archive / hostile input + licensing | P1: ledger error bodies could put BEAR statements into uploaded results | Fixed: replies are reduced to code + hash, and parse errors and TB tokens are never quoted. Also: pinned-size regular-file reads, `.part` written with `create_new`, artifact size cap, tar buffers dropped, an unverified LGPL claim removed, a modification notice in the report, and the CI-cache distribution recorded for the owner. |
| Production-qualification gaps | no P0; missing P1 (write ceiling at depth 10,000) | The matrix was re-verified at `795028e` and corrected. New rows: write ceiling (P1), limits pairing, Sculpin `invocation_id` deduplication (P1), runtime write authority, per-release qualification (P1). |
| Performance + PostgreSQL measurement methodology | no P0; P1: healthcheck statements inside windows; store path is not "same fold without HTTP"; CPU is not a breakdown; absolute numbers are topology-specific | Healthchecks are quiet after start-up in the instrumented override (run 2 is clean: API = store + 2 everywhere). The database is settled before measuring. p99 only from n ≥ 100. Post-restart store reads use a fresh pool. The limits are documented in `METRICS.md` and `recon.rs`. The recommendation rests on shape and ratios, not absolute latency. |
| Bottleneck inference without the preferred answer | — | Independently concluded: about two round trips per ancestor dominate (execution ≈ 14 %, fold ≈ 5 %), so make the ancestry walk set-based first. Checkpoints, caches, fold work and PostgreSQL tuning are not yet supported. |
| Codex review of PR #13 (automated, `aabdd11`) | two P2 | Fixed (2026-10-07): `recon` refuses a state size that is still being built in `BULK` chunks at the smallest requested depth (such a point would be measured against a partial state and labelled with the full size), and a failed `VACUUM (ANALYZE)` or `CHECKPOINT` settle now aborts the run instead of being noted in `environment` while the result could still say `pass`. |

## Evidence

Revisions:
- **Code head** `e353b6c` is the last commit that changes code, scripts or CI.
- **Docs head** `93a7f90` and the final PR head add only docs and evidence:
  `git diff --stat e353b6c..<final head>` touches `docs/` only.
- **Hosted CI** for `pull_request` runs on GitHub's test-merge commit, not on the branch
  head. For the code head that is `c4abd3d` (merge of `e353b6c` into `646b029`). The final
  head's runs are listed in the PR description.

| Gate | Revision | Result |
|---|---|---|
| `check-fast`: fmt, clippy, tests, architecture, doc links, doc consistency with matrix-aware required checks (self-tested), goldens | `e353b6c` | pass, 253 tests, 0 failed |
| `ledger-bench` unit tests | `e353b6c` | 36 + 5 pass, 1 ignored (the slow debug-build `local` generation test; covered by `validate --profile local` in release) |
| Supply chain (`check-supply-chain.sh`: advisories, bans, licenses, sources, SBOM) | `e353b6c` | pass |
| PostgreSQL 17 suites (`ledger-store --features postgres`, `ledger-api`; `--ignored`) | `4eb4d28` (no crate, migration, Dockerfile, compose or lockfile change since) | 124 + 36 pass, 0 failed |
| PostgreSQL 15 suites | `4eb4d28` (same) | 124 + 36 pass, 0 failed |
| `pg_graphs_migration` repeated: 10 × PG17 + 10 × PG15 | `4eb4d28` (same) | 20 / 20 runs pass (160 test executions), each 2–9 s; no hang |
| Integration (`test-integration.sh`) | `e353b6c` | `INTEGRATION OK` |
| Upgrade 0012 → 0013 (`upgrade-p5.sh`) | `e353b6c` | `UPGRADE-P5 OK`, previous `5216bce` schema 12 → 13; clean and upgraded schemas converge (1,406 DDL/grant lines, 100 owned objects) |
| Benchmark `ci` (`benchmark.sh ci`, local) | `93a7f90` (code = `e353b6c`), tracked changes 0 | synthetic-ledger-ci 1,115 and bear-b-ci 185 assertions, 0 failed; `VERIFY OK` |
| Benchmark `local` | `93a7f90` | synthetic-ledger-local 4,945 assertions, 0 failed; `VERIFY OK` |
| Benchmark `bear` | `93a7f90` | bear-b-ci 185 assertions, 0 failed; `VERIFY OK` |
| Reconstruction characterization, official run 1 | clean worktree `4eb4d28` (`official=yes`) | `RECON PASS`, 144 points, 186 exact checks, 0 failures; `VERIFY OK`; 69 min |
| Reconstruction characterization, official run 2 | clean worktree `e353b6c` (`official=yes`; quiet healthchecks, settled database) | `RECON PASS`, 144 points, 186 exact checks, 0 failures; `VERIFY OK`; API calls = store + 2 in every window |
| Hosted CI, test-merge `c4abd3d` (code head `e353b6c`) | ci-fast 37552533912, ci-integration 37552533888, ci-security 37552533866, ci-fuzz 37552533864, ci-benchmark 37552533925 | all success: fast; docker; container, dependency-review, supply-chain; fuzz-sanitizer (none), fuzz-sanitizer (address), aggregate `fuzz`; benchmark-ci |
| Hosted `benchmark-ci` envelope (`c4abd3d`) | run 37552533925 | job 4m55s (harness build 23 s, cached sources verified, fetch+prepare 14 s, stack 190 s, **benchmark run 32 s**: synthetic 1,115 + BEAR 185 assertions, 0 failed; `VERIFY OK`) |
| Hosted `benchmark-ci` envelope, earlier heads | `097f517` / `4eb4d28` | 5m36s (first download of the sources, 33 s; run 44 s) / 7m36s (cold rust cache: harness build 141 s; run 40 s) |
| Backup/restore | — | not applicable: no production, storage or migration code changed in this PR (`git diff 646b029.. -- crates apps/ledger-server apps/ledger-projector migrations` touches only three test files) |

