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
| `edge_timeout_slow_loris_and_body_boundary_are_bounded` | ACCEPTED (edge envelope) | pass | pass | read under a slow database: `REQUEST_TIMEOUT` with read guidance; slow-loris write: `REQUEST_TIMEOUT` with key guidance at request_timeout + the edge grace (a tenth of the request timeout, at most 1 s; ≥ 1.0 s, < 5 s with a 1 s request timeout — the `9c699cc` review's P2) |
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

## Hosted GitHub Actions
`48e323b` (the PR #17 head the hosted Codex review examined): every stable check green —
benchmark-ci, container, dependency-review, docker, fast, fuzz (aggregate; sanitizer jobs
`address` and `none`), supply-chain (Actions runs 37656694982, 37656694984, 37656694986,
37656695032, 37656695042). That run precedes the post-review P1 fixes; the corrected head's run
is recorded below.

## Post-review fixes (hosted Codex review of `48e323b`; fix revision `178895a`)

| test | class | 17.2 | 15.19 | measured |
|---|---|---|---|---|
| `a_request_budget_that_ends_before_begin_returns_the_connection_without_a_transaction` | ACCEPTED (new; the `BeforeBegin` hook did not exist, so no red run against the pre-fix code) | pass | pass | the budget ends while the operation holds the acquired connection: `DependencyUnavailable` "before the transaction began"; the backend is `idle` with `xact_start IS NULL`, zero locks, nothing written, key unused; the retry begins and commits on the **same backend pid** |
| `a_caller_dropped_during_begin_never_returns_an_open_transaction_to_the_pool` | ACCEPTED (new) | pass | pass | the caller's task is aborted while `BEGIN; SELECT pg_sleep(0.8)` is in flight; the begin task completes and rolls back; the backend returns `idle` (never `idle in transaction`), zero locks; the retry commits on the same pid. **Red** against the pre-fix shape (begin awaited inline): the test timed out after 30 s waiting for the connection to leave the transaction — the backend stayed `idle in transaction` after the drop (17.2) |
| `statements_cannot_keep_starting_after_the_transaction_deadline_without_a_backstop` | ACCEPTED (new; PostgreSQL 15 is the target) | pass | pass | six real 250 ms statements through the production accessor under a 300 ms bound stop at the first that would start past the deadline: **501.7 ms** (17.2) / **501.1 ms** (15.19) from the first statement — the deadline plus one tail, never 1.5 s; 17.2 reports `transaction_timeout 2300ms` (never reached), 15.19 `absent`. **Red** with the accessor's check removed: all six statements ran, 1.504 s, caught only by the pre-`COMMIT` check ("1514 ms elapsed before COMMIT", 17.2) |

Suite after the fixes: `pg_lifecycle` 21 passed on 17.2 (20.3 s) and 21 passed on 15.19
(18.2 s); every other PostgreSQL suite re-run on both versions at `178895a` (Plan 0013
Evidence table). A defect found by the requalification itself: at `178895a` the production
feature set (`postgres` without `test-hooks`) did not compile — the container build of the
integration and fault gates caught what every unified-feature local check and the hosted
`fast` job had passed; `dadb1f2` fixes the import and hardens the architecture probe so a
production feature set that does not compile now fails `check-fast` (the probe was shown to
reject the broken build). `check-fast`, `test-integration.sh` (INTEGRATION OK) and
`fault.sh 4 2 100 10` (FAULT GATE OK, 4,608 commits, 993 in-doubt replays, 0 inconsistent,
verifier clean, zero unexpected error classes) passed at `dadb1f2`. Hosted GitHub Actions on
`dadb1f2`: benchmark-ci, container, dependency-review, docker, fast, fuzz (sanitizer jobs `address` 10m15s and `none` 8m59s), supply-chain all **pass** (Actions runs 37845727788, 37845727836, 37845727871, 37845727876, 37845728101).

## Post-review fixes, round 2 (hosted Codex review of `dadb1f2`; fix revision `9bfc8db`)

| test | class | 17.2 | 15.19 | measured |
|---|---|---|---|---|
| `the_readiness_probe_stops_at_its_first_catalog_statement_after_the_request_deadline` | ACCEPTED (new) | pass | pass | `_sqlx_migrations` locked by the owner past a 300 ms request deadline: the probe's first catalog statement waits on the lock, returns after the deadline, and the second (the guard trigger lookup) is refused — `DependencyTimeout` "time budget ended before the guard trigger lookup" — within the 500 ms the test allows of the lock release; the connection returns idle; a fresh probe passes. **Red** against the pre-fix shape (`schema::verify(&pool)`): the probe completed with `Ok(())` after the lock release (every catalog statement ran past the deadline; 17.2) |

Suites re-run at `9bfc8db` on both versions: `pg_lifecycle` 22 passed / 22 passed (24.0 s / 21.7 s; the per-statement acceptance 501 ms on both, the 17 backstop 2.29 s); `pg_least_privilege` 19 / 19; `pg_projection` 7 / 7; `pg_workflow` 14 / 14; `pg_api` 20 / 20 (`future_` excluded); `pg_validation_api` 23 / 23; lint clean.

Hosted: hosted GitHub Actions on `7958214` (the evidence commit on top of `9bfc8db`): benchmark-ci, container, dependency-review, docker, fast, fuzz (sanitizer jobs `address` 10m38s, `none` 9m36s), supply-chain all **pass** (Actions runs 37849053018, 37849053037, 37849053068, 37849053099, 37849053121); the four review threads of `48e323b` and `dadb1f2` are answered with their fix commits and tests and resolved; a fresh Codex review of the final head was requested.

## Post-review fixes, round 3 (hosted Codex review of `9c699cc`: two P2s; fix revision `4c1ac0b`)
Edge grace = a tenth of the request timeout, at most 1 s (`edge_timeout_slow_loris_…` now
asserts ≥ the 1 s request timeout and < 5 s: the cut lands at ≈ 1.1 s); the transaction
bound's clock starts at acquisition, before `BEGIN`. `pg_lifecycle` 22 / 22 and `pg_api`
20 / 20 on 17.2 and 15.19; INTEGRATION OK; FAULT GATE OK (4,643 commits, 0 inconsistent,
verifier clean). The unit envelope test's elapsed-time assertion still encoded the fixed
1 s grace and failed at `4c1ac0b` (local `check-fast` and the hosted `fast` job); `83b5103`
corrects it — `ledger-api` unit tests 27 passed, `check-fast` passed.

## Post-review fixes, round 4 (hosted Codex review of `cc693de`: one P2; fix revision `482c673`)
An empty `LEDGER_VALIDATOR_URL` no longer counts as a configured validator for the headroom
relation (the same normalization as `validator_settings`); unit test in `ledger-server`
(15 passed); `check-fast` passed. Hosted Actions on `cc693de`: all green (runs
37857286399, 37857286426, 37857286485, 37857286520, 37857286558).

## Post-review fixes, round 5 (second review of `cc693de`: one P2; fix revision `18a6839`)
Deadline checks after each reconstruction window's CPU work (`lifecycle::check`); suites at
`18a6839` on 17.2 and 15.19: `pg_lifecycle` 22, `pg_workflow` 14, `pg_validation` 9, `pg_merge` 27,
`pg_retrieval` 12, `pg_branches` 18, `pg_api` 20 — all passed; `check-fast` passed. Hosted
Actions on `b70620d` (round 4 head): all green (runs 37862632512, 37862632515, 37862632536,
37862632537, 37862632570).
