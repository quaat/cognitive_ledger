# Metrics

## Correctness assertions
These are gates: any failure makes the run fail. They are reported per family in
`result.json` (`correctness.checks`).

| family | assertion |
|---|---|
| state after commit | `ledger_materialized_state(C) == expected_state(C)` right after each commit, on the raw API list: strings only, strictly ascending (canonical order, no duplicates), then the count and the oracle's SHA-256 over sorted lines. The failure detail includes the set difference when the state is retained |
| state after merge | `reconstruct(I) == expected merged state` for every applied integration commit |
| historical reconstruction | every commit (`ci`), or every retained commit (`local`), reconstructed again after the whole history exists |
| diff(A, B) == expected | `ledger_rdf::diff` on two ledger-materialized states equals the oracle's set difference, for selected pairs (`algorithm` category; no public diff endpoint) |
| merge classification, base, ahead/behind | each preview against the brute-force ancestry reference and the designed scenario, including a base reachable only through a second parent and an explicit base resolving a criss-cross |
| merge base candidates | for `ambiguous_merge_base`: the listed best common ancestors, ascending, and the exact count |
| merge conflict count and keys | the exact designed `(graph, subject, predicate)` slots, ascending |
| merge delta summary | adds, deletes and affected keys of base → target and base → source |
| merge preview token presence | a token exactly for candidate classes |
| merged state digest | the preview's `merged_state_digest` equals `sculpin-rdf-state/v1` of the oracle's expected merged state (labelled use of the frozen protocol function) |
| merge apply moves the target | the apply response head is the proposed candidate |
| branch head | `GET …/refs` per branch equals the oracle's final head |
| first-parent history | `GET …/branches/log` equals the oracle's parent-0 chain |
| commit parents | the persisted envelope's parents (`ImmutableStore::get_commit`, owner identity) equal the oracle's DAG; integration commits are `[target, source]` |
| commit provenance | the persisted activity, message, evidence references and source system equal what the dataset attached |
| history fact (appear / disappear / reappear) | for `bear-b-ci`: source statements' membership at version boundaries (absent, present, absent again, present again), checked in ledger-materialized states |
| dataset validity | the computed manifest equals the committed manifest (exit 3 otherwise); for extracted datasets also the pinned source SHA-256 and size and the prepared artifact's SHA-256 |
| verify | `ledger-admin verify` reports `VERIFY OK` after the run (script) |

## Performance observations
These are **not gates in Phase 6A.**

- **Timed operations.** Every timed operation records category, operation, history kind
  (`bulk` for the initial load commits, `main`, `feature`, `growth`, `churn`, `merge`),
  parent-0 depth, state size, folded patch operations along the parent-0 chain
  (`fold_ops`, the logical work of a reconstruction) and response bytes. A merge preview is
  attributed to the target head, although it folds base, target and source.
- **Categories:**
  - `api`: client-observed latency of the public HTTP API, from sending the request
    (already serialized) to the response body being received and decoded into a JSON value.
    This includes server work, server JSON encoding, transfer and client decoding;
  - `persisted`: owner-identity reads of the production store;
  - `algorithm`: an infrastructure-free crate on ledger-materialized states.
- **Aggregates:**
  - count, p50, p95, p99, max and mean, by nearest rank;
  - the report omits p95/p99 for groups of fewer than 20 samples, where they would equal
    the maximum; the JSON keeps them;
  - ingest throughput: commits, branch creations and applied merges per second over the
    serial ingest phase, including the per-step state checks. This is not server capacity;
  - the same per operation × history kind × depth bucket;
  - raw series `(op, kind, depth, quads, ms)` for plotting depth against latency.
- **Phases:** oracle generation, ingest with per-step checks, historical checks, diff
  checks and persisted checks. The script adds harness build, dataset validation,
  stack build and start, run, and verify.
- **Resources:**
  - harness peak RSS (`VmHWM`);
  - database size before and after (`pg_database_size`), the on-disk size of
    `immutable_objects` (`pg_total_relation_size`, TOAST-compressed), and its logical bytes
    (`sum(octet_length(bytes))`, what hashing and decoding read), all read under the owner
    identity;
  - container peak memory (cgroup v2 `memory.peak`, else `memory.current` sampled every
    0.5 s, else `unavailable`; page cache included).
- **Not captured in Phase 6A:** server CPU time, cold-versus-warm cache separation, and
  PostgreSQL query counts and bytes read. These need `pg_stat_statements` or `track_io_timing`
  in a dedicated configuration, and are listed as the next measurements in Plan 0010.

## Reconstruction characterization
`ledger-bench recon` (`sculpin-ledger-bench-recon/v1`; outside PR CI) builds
constant-state linear histories, with 1, 1,000 and 10,000 quads by default, to depth 5,000
through the API. It then measures each (state size, depth) point serially, on a quiet stack.

| op (category) | isolates |
|---|---|
| `state_read` (`api`) | the full public read: HTTP, the server's reconstruction, JSON encoding, transfer and client decoding |
| `store_reconstruct` (`persisted`) | the same production fold (`WorkflowRepository::reconstruct`) in-process, without HTTP |
| `fold_cpu` (`algorithm`) | the fold's CPU work only (SHA-256 re-hash, decoding, set application) on prefetched objects. An estimate of the non-I/O share; not the production code path |
| `prepare` (`api`) | prepare at that depth (a branch at the commit), which reconstructs the parent |
| `merge_preview_contained` (`api`) | a preview that returns after the ancestry walk of both sides, with no reconstruction: merge ancestry traversal alone |
| `merge_preview_divergent` (`api`) | ancestry plus three reconstructions at about that depth |

Per point it records:
- p50 (p95/p99 only with n ≥ 20) and the mean;
- response bytes;
- fold operations and canonical state bytes;
- per operation from `pg_stat_statements`, for the role that ran the batch (statistics
  queries excluded): calls, rows, shared block hits and reads, temporary blocks, block read
  time (`track_io_timing`) and execution time. These are PostgreSQL's 8 KiB buffer
  statistics. They are **not physical I/O bytes**: a block "read" may be served by the OS
  page cache. The window is reset per batch.
- per operation: cgroup v2 `cpu.stat usage_usec` of the server and PostgreSQL containers.
  This is whole-container CPU, so it includes PostgreSQL background workers that run during
  the window. `memory.current` is read after the batch and includes the page cache.

**Window order** (`recon.rs`, tested):
1. Reset the statement statistics and read the baseline totals.
2. Read the baseline CPU.
3. Run the measured operations.
4. Read the ending CPU immediately.
5. Read the ending statement totals.

The statistics queries therefore fall outside the CPU window.

**Statement-count gate (Plan 0012).** `PG calls` per operation is the deterministic
measure of retrieval scaling. Before Plan 0012 a state read was `2 × depth` statements
(+ 2 constants on the API path) and a contained merge preview `2 × depth` per side. With
windowed retrieval a state read is `2 × ceil(depth / 256)` and a linear ancestry walk
`4 + ceil((visited − 85) / 256)` per side (ramp 1, 4, 16, 64, then 256; merge-heavy
histories need more windows, each bounded); the `recon` histories are linear. A result
whose calls grow with depth at about 2 per ancestor is a regression of the retrieval
path, whatever its latency.

**Correctness** is exact at every point and untimed. Each measured path's result (API
reply, direct store reconstruction, prefetched fold, and the post-restart reads) must match
the oracle digest of that commit's state. Equal cardinality is not enough.

**Cache conditions:**
- `warm`: measured after warm-up repetitions.
- `db-restart-first-ledger-op`: PostgreSQL was restarted, which restarts its processes and
  shared buffers. Before the measured operation, the server's `/ready` probe, the reconnecting
  pool and the window's statistics queries run. The measured operation is therefore the
  first ledger reconstruction after the restart, not the first PostgreSQL operation, and
  shared buffers are not untouched.
- The OS page cache is never dropped, so no OS-cold condition is claimed.

**Before measuring:**
- the database is settled after the concurrent write build with `VACUUM (ANALYZE)` and then
  `CHECKPOINT`;
- the instrumented override slows the containers' healthchecks to once a day after
  start-up, so no probe statement (`ledger-admin probe /ready` runs `schema::verify` as the
  runtime role) lands in a measurement window.

**Percentiles:** p95 is reported from n ≥ 20, where it is the second-largest of 20. p99 is
reported only from n ≥ 100, because below that the nearest-rank p99 is the maximum.

**What these figures cannot support:**
- **API minus store is not the cost of HTTP and JSON.** `store_reconstruct` runs in the
  benchmark process on the host and reaches PostgreSQL through the published port. That path
  goes through Docker's port mapping, and the result records whether the userland proxy was
  running. The server reaches PostgreSQL over the container network instead. The benchmark
  process's own CPU is not measured.
- **`fold_cpu` is a lower bound.** It hashes each object once on cache-hot memory, while
  production hashes commits and patches twice and checks limits per patch.
- **No per-part latency breakdown.** Container CPU is whole-container. Server plus
  PostgreSQL CPU can exceed latency, because both sides work around each round trip, so it
  cannot be split into parts of the latency.
- **PostgreSQL CPU does not transfer.** It is measured with `pg_stat_statements.track=all`
  and `track_io_timing`, which production does not run.
- **Absolute latency does not transfer.** The per-round-trip cost is specific to the host and
  container topology. The scaling shape transfers: round trips per ancestor and linear
  growth.
- **Memory is not a peak.** It is `memory.current` after the batch and includes page cache.
  Kernel 5.10 has no `memory.peak`, so the memory of individual parts is not measured.

Results record that PostgreSQL ran the benchmark-only instrumentation configuration. A
result is official only from a clean checkout (`official=yes`: no tracked changes and no
untracked files). `recon.md` marks every other run **NON-OFFICIAL**.

## Methodology notes
- **Everything is warm in the `run` profiles.** Each head read follows the prepare/accept
  that just folded the same chain, and PostgreSQL and OS caches are hot. The `recon` profile
  adds the `db-restart-first-ledger-op` condition.
- **The synthetic profiles do not isolate depth from state size.** Every history starts
  from a 6,100-quad (`ci`) or 12,200-quad (`local`) genesis, and `growth` and `churn`
  differ by only a few percent in state size. The depth-isolating control remains the Plan
  0005 constant-state run (`scripts/bench.sh … --constant-state`, a 1-quad state).
- One serial client and one replica, with no background load. Numbers characterize
  single-request latency on the measured host and are comparable only with the same
  profile, host class and build.
- Each operation runs once per commit in the history, so per-bucket sample counts are small
  in `ci`. The `local` profile gives larger buckets and deeper histories.
- Hosted runners are shared and noisy. A CI timing is an observation of that runner, not a
  baseline for gating.

## Regression policy
Phase 6A records history only. The following conditions must hold before any timing gate
exists:
1. The same profile has been repeated on the same host class enough times to estimate
   variance; at least 5 runs are recommended.
2. A tolerance is defined per operation, for example a p95 change greater than the
   observed spread confirmed over repeated runs.
3. The rule and its noise analysis are documented here.

Correctness never has a tolerance.
