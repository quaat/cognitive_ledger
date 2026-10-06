# Metrics

## Correctness assertions
These are gates: any failure makes the run fail. They are reported per family in
`result.json` (`correctness.checks`).

| family | assertion |
|---|---|
| state after commit | `ledger_materialized_state(C) == expected_state(C)` right after each commit (count plus the oracle's SHA-256 over sorted lines; full set difference in the failure detail when the state is retained) |
| state after merge | `reconstruct(I) == expected merged state` for every applied integration commit |
| historical reconstruction | every commit (`ci`), or every retained commit (`local`), reconstructed again after the whole history exists |
| diff(A, B) == expected | `ledger_rdf::diff` on two ledger-materialized states equals the oracle's set difference, for selected pairs (`algorithm` category; no public diff endpoint) |
| merge classification, base, ahead/behind | each preview against the brute-force ancestry reference and the designed scenario |
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
  (`main`, `feature`, `growth`, `churn`, `merge`), parent-0 depth and state size.
- **Categories:**
  - `api`: client-observed latency of the public HTTP API, including JSON encoding;
  - `persisted`: owner-identity reads of the production store;
  - `algorithm`: an infrastructure-free crate on ledger-materialized states.
- **Aggregates:**
  - count, p50, p95, p99, max and mean, by nearest rank;
  - "serial ops/s", which is samples divided by the summed latency of one serial client,
    not a server throughput;
  - the same per operation × history kind × depth bucket;
  - raw series `(op, kind, depth, quads, ms)` for plotting depth against latency.
- **Phases:** oracle generation, ingest with per-step checks, historical checks, diff
  checks and persisted checks. The script adds harness build, dataset validation,
  stack build and start, run, and verify.
- **Resources:**
  - harness peak RSS (`VmHWM`);
  - database size before and after (`pg_database_size`) and `immutable_objects` total
    size (owner identity);
  - container peak memory (cgroup v2 `memory.peak`, else `memory.current` sampled every
    0.5 s, else `unavailable`; page cache included).
- **Not captured in Phase 6A:** server CPU time, cold-versus-warm cache separation, and
  PostgreSQL query counts and bytes read. These need `pg_stat_statements` or `track_io_timing`
  in a dedicated configuration, and are listed as the next measurements in Plan 0010.

## Methodology notes
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
