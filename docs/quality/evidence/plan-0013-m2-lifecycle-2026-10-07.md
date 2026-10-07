# Plan 0013 M2 — request lifecycle versus transaction lifecycle (2026-10-07)

Measured on the M2 code of branch `claude/p7a-m2-request-lifecycle` (stacked on the PR #15
head `9796770`; final figures at code revision `5e82e4c`)
against the two throwaway containers of the M1 evidence: PostgreSQL 17.2 (`127.0.0.1:55433`)
and PostgreSQL 15.19 (`127.0.0.1:55434`). Commands (`RUST_TEST_THREADS=4` for the suites):

```bash
export PATH=$HOME/.cargo/bin:$PATH
export LEDGER_TEST_DATABASE_URL="postgres://ledger:ledger-development-only@127.0.0.1:<port>/ledger?sslmode=disable"
cargo test -p ledger-store --features postgres,test-hooks --test pg_lifecycle -- --ignored --nocapture --skip future_
cargo test -p ledger-api --features ledger-store/postgres,ledger-store/test-hooks --test pg_api -- --ignored --nocapture --skip future_
cargo test -p ledger-api --features ledger-store/postgres,ledger-store/test-hooks --test pg_api -- --ignored future_   # M3: red by design
cargo test -p ledger-store --features postgres --test pg_least_privilege -- --ignored --test-threads=1
cargo test -p ledger-api --lib lifecycle; cargo test -p ledger-store --features postgres --lib lifecycle
```

Classification (Plan 0013): **PRESERVATION** (held before and after M2), **CHARACTERIZATION**
(a documented contract or a measured envelope), **ACCEPTED** (asserts the ADR-0026 behaviour
M2 implemented). The three tests promoted from `future_*` carry their M1 red evidence
([M1 evidence](plan-0013-m1-lifecycle-2026-10-07.md)); the tests new in M2 could not run red
against the pre-M2 code (the hooks, settings and error variant they use did not exist) and
say so in the plan. Every observed store has its own `application_name`; "locks released"
is read from `pg_locks`, session state from `pg_stat_activity`. Timings are client-observed
single samples unless stated; no test relies on a sleep where a hook can synchronise
(`reached_or_failed` selects the hook against the operation's join handle).

## Defaults and relations (ADR-0026 §2; `ledger_api::lifecycle::validate_lifecycle`)

| setting | default | env |
|---|---|---|
| `lock_timeout` | 5 s | `LEDGER_DB_LOCK_TIMEOUT_MS` |
| `statement_timeout` | 10 s | `LEDGER_DB_STATEMENT_TIMEOUT_MS` |
| transaction bound | 20 s | `LEDGER_DB_TRANSACTION_TIMEOUT_MS` |
| `idle_in_transaction_session_timeout` | 30 s | `LEDGER_DB_IDLE_IN_TRANSACTION_TIMEOUT_MS` |
| pool acquire | 5 s | `LEDGER_DB_ACQUIRE_TIMEOUT_MS` |
| request | 30 s | `LEDGER_LIMIT_REQUEST_SECONDS` |
| validator | 15 s | `LEDGER_LIMIT_VALIDATOR_SECONDS` |
| drain | 40 s | `LEDGER_DRAIN_TIMEOUT_MS` |
| PostgreSQL 17 `transaction_timeout` backstop | bound + statement = 30 s (absent on 15) | derived |

Unit evidence: `ledger_api::lifecycle::tests` (defaults valid; every relation violated once
with the message naming the variables; tracker register/close/`wait_idle`; a panicking
operation deregisters) and `ledger_store::lifecycle::tests` (scoped request deadline and the
acquire budget; the deadline check names the phase and the request-deadline cap; only
unavailable/timeout errors from `COMMIT` are `CommitOutcomeUnknown`) — all pass on both
versions (no database).

## `crates/ledger-store/tests/pg_lifecycle.rs` — 18 tests, `future_` filtered (none remain)

| test | class | 17.2 | 15.19 | measured |
|---|---|---|---|---|
| `a_write_dropped_before_commit_…` (now 9 paths: + validation record) | PRESERVATION | pass | pass | locks released 0.3–1.2 ms (17) / 0.4–1.9 ms (15) after the drop; no row in 13 graph-scoped tables; fresh retry 2–10 ms / 2–12 ms; replay on the second retry |
| `a_write_dropped_after_commit_…` (9 paths) | PRESERVATION | pass | pass | durable, session idle, no locks, verifier clean; replay names the committed ids |
| `every_heavy_store_path_completes_on_a_one_connection_pool` (+ validation begin/record) | PRESERVATION | pass | pass | `max_connections = 1` |
| `an_abandoned_active_statement_pins_its_connection_until_postgres_ends_it` | CHARACTERIZATION (F1, unchanged by design: M2 detaches instead of dropping) | pass | pass | released 1.502 s / 1.502 s after a 1.5 s statement; 0.740 s / 0.804 s with `statement_timeout` 700 ms |
| `a_late_cancel_addressed_by_pid_hits_the_next_borrower_of_the_pooled_session` | CHARACTERIZATION (hazard pin; **unchanged**) | pass | pass | no active cancellation in M2 |
| `pg17_transaction_timeout_terminates_the_session_and_pg15_lacks_it` | ACCEPTED (25P04 → `DependencyTimeout`) | pass | pass | 17: fired after 300.06 ms, SQLSTATE 25P04, `DependencyTimeout("… terminating connection due to transaction timeout")`, session replaced (new pid); 15: 42704 |
| `a_statement_timeout_inside_an_accept_…`, `a_lock_timeout_on_the_ref_row_…`, `deadlock_and_serialization_failures_…` | PRESERVATION | pass | pass | |
| `concurrent_same_key_accepts_…`, `concurrent_same_key_merge_applies_…` | PRESERVATION (ADR-0013, forced overlap) | pass | pass | 1 execution + 3 replays |
| `a_transaction_exceeding_its_bound_does_not_commit_late` | **ACCEPTED (promoted**; M1 red: committed 1.221 s / 1.214 s past a 500 ms bound) | pass | pass | bound 1.5 s, three injected 700 ms statements before `COMMIT`: `DependencyTimeout` "exceeded its bound … before COMMIT", measured from the hook ≤ 2.1 s + 0.4 s; nothing durable; key unused; retry fresh |
| `a_transaction_paused_past_its_bound_rolls_back_before_commit` | **ACCEPTED (promoted**; M1 red: committed) | pass | pass | real statements only, paused past the bound: rolled back at the pre-`COMMIT` check |
| `a_transaction_paused_past_its_bound_after_the_replay_check_rolls_back_at_the_first_phase` | ACCEPTED (new; no red run) | pass | pass | all 9 paths paused after the replay lookup past a 300 ms bound: each fails at its **first** phase check, the error naming that phase exactly; nothing written; key unused |
| `the_pg17_transaction_timeout_backstop_fires_only_past_the_bound_plus_one_statement` | ACCEPTED (new) | pass | pass | bound 300 ms, statement 2 s, two injected 1.5 s statements that skip the ledger's checks (a simulated check-skipping bug): 17 — `transaction_timeout` shows **2300ms** through the production pool, the session is terminated at **2.29 s** (25P04 → `DependencyTimeout`), the next acquire + statement costs **47 ms** (pool reconnect); 15 — setting absent, the pre-`COMMIT` check ends it after **3.006 s**, the next acquire + statement 0.3 ms (same connection) |
| `a_connection_idle_in_the_pool_longer_than_the_bound_is_borrowed_normally` | ACCEPTED (new) | pass | pass | statement 200 ms / lock 100 ms / idle 400 ms / bound 300 ms (17 backstop 500 ms); idle 1.5 s: same backend pid and `backend_start`, `SHOW` values unchanged, a full accept then runs |
| `a_commit_on_a_terminated_connection_is_outcome_unknown_and_the_retry_runs_fresh` | ACCEPTED (new) | pass | pass | paused before `COMMIT`, backend terminated by the owner session, resumed: `CommitOutcomeUnknown(DependencyUnavailable)`; nothing durable; key unused; retry fresh |
| `an_identical_object_published_by_an_uncommitted_transaction_makes_the_second_prepare_wait` | CHARACTERIZATION (contract) | pass | pass | byte-identical content from an uncommitted transaction: the second prepare waits on the unique index until `lock_timeout` and surfaces `DEPENDENCY_TIMEOUT` after **305 ms** / **305 ms** (`lock_timeout` 300 ms); the retry succeeds |

Suite: 18 passed on 17.2 (14.2 s) and 18 passed on 15.19 (14.0 s).

## `crates/ledger-api/tests/pg_api.rs` — 20 tests, `future_` filtered (1 remains, M3)

| test | class | 17.2 | 15.19 | measured |
|---|---|---|---|---|
| `p7a_pool_exhaustion_fails_every_class_after_the_acquire_timeout_with_class_guidance` | ACCEPTED (rewritten M1 characterization) | pass | pass | both connections held, acquire timeout 2 s: read, `/ready`, prepare, accept all 503 `DEPENDENCY_UNAVAILABLE` after **2.002 s** / **2.002 s**; the read and `/ready` are never told to use a key; nothing written |
| `p7a_heavy_writes_without_admission_take_every_connection_and_cheap_reads_starve` | CHARACTERIZATION (F3; M3 fixes it) | pass | pass | read starves the 3 s acquire timeout (**3.001 s** / **3.002 s**); accepts [409, 200, 409] after the blocker |
| `p7a_an_edge_timeout_after_commit_reports_the_outcome_as_unknown_and_the_key_replays` | **ACCEPTED (promoted**; M1 red: `RESOURCE_LIMIT`) | pass | pass | 503 `REQUEST_TIMEOUT` with the idempotency-key guidance while the operation is paused after `COMMIT` (`detached().active() == 1`); durable; the retry replays |
| `p7a_a_request_timing_out_at_its_graph_lookup_keeps_its_operation_registered_until_it_ends` | ACCEPTED (new; ADR §8 ownership) | pass | pass | `graphs` table-locked in a throwaway database: read and write both `REQUEST_TIMEOUT` by class, both operations still registered (`active() == 2`); after the lock is released the write stops at its budget-spent acquisition (no transaction, `ref_events` unchanged, no key), the retry executes afresh |
| `p7a_a_client_timing_out_mid_transaction_never_gets_a_late_commit` | ACCEPTED (new; ADR §2 cap) | pass | pass | 2 s statement under a 1 s request timeout: `REQUEST_TIMEOUT` at ≈ 1 s, operation registered while the statement runs, rolled back at the pre-`COMMIT` check (deadline capped by the request); `ref_events` unchanged; no key; the retry executes afresh |
| `p7a_graceful_shutdown_waits_for_a_detached_write_and_the_key_replays_on_another_replica` | ACCEPTED (new; ADR §8 shutdown) | pass | pass | tracker closed → new database-bearing request 503 `DEPENDENCY_UNAVAILABLE` "shutting down"; `wait_idle` pending while the operation runs, resolved inside the drain once it ends; the key replays on a second replica over the same database |
| `p7a_every_heavy_route_completes_on_a_one_connection_pool` | PRESERVATION | pass | pass | caught the nested acquisition introduced by a review fix (503 after 5 s on `branches/history`), fixed |
| `edge_timeout_slow_loris_and_body_boundary_are_bounded` | ACCEPTED (edge envelope) | pass | pass | read under a slow database: `REQUEST_TIMEOUT` with read guidance; slow-loris write: `REQUEST_TIMEOUT` with key guidance at request_timeout + the 1 s edge grace (≥ 1.9 s, < 5 s) |
| `future_p7a_reserved_headroom_keeps_reads_and_readiness_answering_while_heavy_writes_saturate` | FUTURE ACCEPTANCE (M3) | **red** | **red** | followers answer `DEPENDENCY_TIMEOUT` after `lock_timeout` instead of `RESOURCE_LIMIT` at once (no admission budget yet) |

Suite: 20 passed on 17.2 (9.9 s) and 20 passed on 15.19 (10.1 s); `future_` run once: 1 failed as intended on both.

## Other suites

| suite | 17.2 | 15.19 |
|---|---|---|
| `pg_least_privilege` (session settings now 10 s / 5 s / 30 s; backstop 30 s on 17, absent on 15; `--test-threads=1`) | 19 passed (48.8 s) | 19 passed (57.5 s) |
| `pg_workflow` 14, `pg_merge` 27, `pg_validation` 9, `pg_branches` 18, `pg_retrieval` 12, `pg_projection` 7 | all passed | all passed |
| `pg_validation_api` 23 | passed | passed |
| `ledger-api` unit tests (27, incl. the envelope-by-class, COMMIT mapping and lifecycle units) | passed | — |

The Plan 0013 Evidence table records the integration, fault and fast gates with their revisions.
