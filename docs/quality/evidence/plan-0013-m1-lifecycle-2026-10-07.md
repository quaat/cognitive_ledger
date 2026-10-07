# Plan 0013 M1 — request-lifecycle characterization (2026-10-07)

Measured against the request lifecycle of `d27e7f8`, which M1 leaves unchanged (no
timeout, admission, transaction or cancellation behaviour differs). M1 is not a
documentation-only change: it adds `test-hooks` code and tests, the F5/F6 harness fixes, the
start-up refusal of a `test-hooks` build in the three binaries, and one production-visible
error-classification fix — **F11**: a sqlx `Error::Protocol` raised while PostgreSQL is in
crash recovery is now a retryable `DEPENDENCY_UNAVAILABLE` (HTTP 503) instead of `INTERNAL`
(HTTP 500); see the fault-run section at the end. The containers are two throwaway
PostgreSQL servers on this workstation: PostgreSQL 17.2
(`postgres:17.2-bookworm`, `127.0.0.1:55433`) and PostgreSQL 15.19 (`postgres:15-bookworm`,
`127.0.0.1:55434`), both with a 1 GiB `/dev/shm`. The final figures below are from the runs
after the five independent reviews, at code revision `647be51`. Commands:

```bash
export PATH=$HOME/.cargo/bin:$PATH
export LEDGER_TEST_DATABASE_URL="postgres://ledger:ledger-development-only@127.0.0.1:<port>/ledger?sslmode=disable"
cargo test -p ledger-store --features postgres --test pg_lifecycle -- --ignored --nocapture --skip future_
cargo test -p ledger-store --features postgres --test pg_lifecycle -- --ignored --nocapture future_   # red today
cargo test -p ledger-api --test pg_api -- --ignored --nocapture p7a_ --skip future_
cargo test -p ledger-api --test pg_api -- --ignored --nocapture future_                                # red today
```

Classification (Plan 0013 §M1): **PRESERVATION** passes now and must keep passing;
**CHARACTERIZATION** pins today's undesirable behaviour with its envelope; **FUTURE
ACCEPTANCE** (`future_` prefix) asserts the ADR-0026 contract, is excluded from the suite runs
with `--skip future_`, and was run once for the red evidence below. Every observed store has
its own `application_name`; "locks released" is read from `pg_locks`, "idle" and "active" from
`pg_stat_activity`. Timings are client-observed single samples unless stated.

## `crates/ledger-store/tests/pg_lifecycle.rs`

| test | class | 17.2 | 15.19 | measured |
|---|---|---|---|---|
| `a_write_dropped_before_commit_rolls_back_leaves_the_key_unused_and_the_retry_runs_fresh` (prepare, accept, reject, branch create / delete / restore, merge propose / apply) | PRESERVATION | pass | pass | locks released **0.6–1.6 ms** (17) / **0.6–2.2 ms** (15) after the drop; session idle; no row in 11 graph-scoped tables; fresh retry 2–12 ms / 3–14 ms; the retry's row deltas equal a clean run's; a second retry replays |
| `a_write_dropped_after_commit_is_durable_and_the_retry_replays_exactly` (same eight paths) | PRESERVATION | pass | pass | rows durable, session idle, no locks, verifier clean; the replay names the committed `decision_id` / `proposal_id` / `event_id` and writes nothing |
| `every_heavy_store_path_completes_on_a_one_connection_pool` | PRESERVATION (admission-model premise) | pass | pass | eight write paths, merge preview, branch from a historical commit, state read, history walk on `max_connections = 1` |
| `an_abandoned_active_statement_pins_its_connection_until_postgres_ends_it` | CHARACTERIZATION (F1) | pass | pass | 1.5 s statement / `statement_timeout` 10 s: session active, locks held, pool empty; connection released **1.502 s** / **1.502 s** after the statement was sent; 10 s statement / `statement_timeout` 700 ms: released **0.809 s** / **0.807 s** after it was sent (≈ 0.1 s after the limit: cancel + return to the pool); then idle, no locks, nothing durable, retry fresh |
| `a_late_cancel_addressed_by_pid_hits_the_next_borrower_of_the_pooled_session` | CHARACTERIZATION (hazard) | pass | pass | A observed active, abandoned; the pool lends the same backend pid to B; a late `pg_cancel_backend(pid)` cancels B's statement (57014); the session survives |
| `pg17_transaction_timeout_terminates_the_session_and_pg15_lacks_it` | CHARACTERIZATION (ADR-0026 §2 input) | pass | pass | 17.2: `SET transaction_timeout = '300ms'` fires after **300 ms** with FATAL **25P04** "terminating connection due to transaction timeout", classified `Storage` today, the backend is gone and the pool reconnects (new pid); 15.19: `42704 unrecognized configuration parameter` |
| `a_statement_timeout_inside_an_accept_is_a_dependency_timeout_and_the_retry_succeeds` | PRESERVATION (57014 through a real write) | pass | pass | `DependencyTimeout` whose message names the statement timeout, well inside the 3 s sleep (300 ms limit); no key; retry fresh |
| `a_lock_timeout_on_the_ref_row_during_accept_is_a_dependency_timeout_and_the_retry_succeeds` | PRESERVATION (55P03 through a real write) | pass | pass | ≈ `lock_timeout` (300 ms); no key; retry fresh after the holder |
| `deadlock_and_serialization_failures_classify_as_dependency_timeouts_at_the_boundary` | PRESERVATION (40P01, 40001 from raw transactions; production lock order untouched) | pass | pass | both `DependencyTimeout` through the production classifier |
| `concurrent_same_key_accepts_on_a_non_genesis_head_execute_once_and_replay` | PRESERVATION (ADR-0013; forced overlap) | pass | pass | winner paused before COMMIT, 3 followers observed waiting on a lock, then 1 execution + 3 replays; same key other candidate → `IdempotencyConflict`; other key → `HeadChanged` |
| `concurrent_same_key_merge_applies_execute_once_and_replay` | PRESERVATION (ADR-0013; forced overlap) | pass | pass | 1 apply + 3 replays, target moved once; other proposal same key → conflict |
| `future_a_transaction_exceeding_its_bound_does_not_commit_late` | FUTURE ACCEPTANCE (M2, ADR-0026 §2) | **fails as expected**: commits after 1.221 s | **fails as expected**: 1.214 s | three 400 ms statements under `statement_timeout` 1 s past a 500 ms bound: no pre-COMMIT check exists today |
| `future_a_transaction_paused_past_its_bound_rolls_back_before_commit` | FUTURE ACCEPTANCE (M2, ADR-0026 §2) | **fails as expected**: commits | **fails as expected** | paused 0.8 s before COMMIT with real statements only: commits today |

Suite result: 11 passed (17.2, 4.3 s) / 11 passed (15.19, 4.1 s), `future_` filtered.

## `crates/ledger-api/tests/pg_api.rs` (`p7a_*`)

| test | class | 17.2 | 15.19 | measured |
|---|---|---|---|---|
| `p7a_pool_exhaustion_waits_the_acquire_timeout_then_fails_every_request_class_alike` | CHARACTERIZATION (F3, F4) | pass | pass | both pool connections held: read, `/ready`, prepare, accept all 503 `DEPENDENCY_UNAVAILABLE` after **10.004 s** / **10.003 s**; the key-less read is told to "retry with the same idempotency key"; nothing written; all answer once a connection is free |
| `p7a_heavy_writes_without_admission_take_every_connection_and_cheap_reads_starve` | CHARACTERIZATION (F3) | pass | pass | three accepts blocked on `main`'s row lock fill a pool of 3; the read (not blocked by the row lock) starves **10.012 s** / **10.001 s** then 503, `/ready` 503; after the blocker: accepts one 200, two 409 |
| `p7a_an_edge_timeout_after_commit_is_reported_today_like_an_admission_refusal` | CHARACTERIZATION (F4) + PRESERVATION (durability, replay) | pass | pass | accept sent only after it provably reached COMMIT: 503 `RESOURCE_LIMIT` "request exceeded the configured time limit" after 1.002 s, no replay guidance; durable (1 ref event) and the retry replays |
| `p7a_every_heavy_route_completes_on_a_one_connection_pool` | PRESERVATION (admission-model premise, HTTP level) | pass | pass | refs, state, history, log, branch from a historical commit, status, prepare/accept on a branch, merge preview / propose / apply, delete, restore, `/ready` on `max_connections = 1` |
| `future_p7a_reserved_headroom_keeps_reads_and_readiness_answering_while_heavy_writes_saturate` | FUTURE ACCEPTANCE (M3, ADR-0026 §5) | **fails as expected** | **fails as expected** | one accept admitted and blocked; with `graphs` locked, followers (accept, prepare, state read) must be refused `RESOURCE_LIMIT` at once — today they block on `graphs` and answer `DEPENDENCY_TIMEOUT` after `lock_timeout` |
| `future_p7a_an_edge_timeout_after_commit_reports_the_outcome_as_unknown_and_the_key_replays` | FUTURE ACCEPTANCE (M2, ADR-0026 §4) | **fails as expected**: code `RESOURCE_LIMIT` | **fails as expected** | must be 503 `REQUEST_TIMEOUT` with "unknown" / "idempotency key" guidance |

Suite result: 4 passed (17.2, 20.7 s) / 4 passed (15.19, 20.7 s), `future_` filtered.

## F5 / F6
- F5: `FailPoint`, `with_failpoint`, `with_validation_failpoint`, every `fail_at` call site and the
  projector's crash windows compile only with the `test-hooks` feature; `ledger-server`,
  `ledger-projector` and `ledger-admin` refuse to start when a build carries it;
  `scripts/check-architecture.py` proves no app's normal build graph enables it (ledger-store
  and ledger-projector) and compiles a scratch crate twice — without the feature each of the
  five test-only symbols is reported unresolved by its full path, with it they resolve — so
  the proof cannot pass vacuously.
- F6: `ledger-stress fault` now fails the run on `503 DEPENDENCY_TIMEOUT` (`fault_unexpected`,
  unit-tested; the pair-verdict helper `is_expected_failure` is unchanged and still used for
  one-sided comparisons); the two merge crash tests assert the injected `Storage("injected
  failure at …")` error; the deterministic post-COMMIT lost-response proof now exists at the
  store level (`a_write_dropped_after_commit_is_durable_and_the_retry_replays_exactly`) and over
  HTTP (`p7a_an_edge_timeout_after_commit_is_reported_today_like_an_admission_refusal`); the
  HTTP-level SIGKILL observation in `scripts/fault.sh` stays a by-chance count.
- `scripts/fault.sh` under the new policy: the first run of the day (`4 2 100 10`) produced
  zero `DEPENDENCY_TIMEOUT` but failed on 165 × `500 INTERNAL` during the PostgreSQL kills;
  a second run with the servers' warnings captured showed every one to be sqlx
  `Error::Protocol("Postgres protocol error (reading Authentication) …")` — a connection
  attempt during crash recovery — classified `Storage` (Plan 0013 F11, fixed: `Protocol` is a
  retryable `DependencyUnavailable`). The run after the fix is recorded in the Plan 0013
  Evidence table.
