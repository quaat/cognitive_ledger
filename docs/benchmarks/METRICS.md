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
| dataset validity | the computed manifest equals the committed manifest (exit 3 otherwise) |
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

## Methodology notes
- **Everything is warm.** Each head read follows the prepare/accept that just folded the
  same chain, and PostgreSQL and OS caches are hot. A cold-cache arm is a listed next
  measurement.
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
