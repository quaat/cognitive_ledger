# Plan 0013: Phase 7A — runtime resource governance and PostgreSQL resilience

Status: **M0 complete and corrected (2026-10-07 review: admission order, idempotency invariant, COMMIT terminology, cancellation race); M1 complete — characterization and acceptance tests on PostgreSQL 17.2 and 15.19, F5/F6 fixed, red `future_*` evidence recorded; ADR-0026 drafted for owner/architecture review; production behaviour changed only by F11 (sqlx `Error::Protocol` during PostgreSQL crash recovery is a retryable `DEPENDENCY_UNAVAILABLE` / HTTP 503 instead of `INTERNAL` / HTTP 500) and by the start-up refusal of a `test-hooks` build in the three binaries — the request lifecycle, admission, timeouts and cancellation are unchanged; M2/M3 not started** (started 2026-10-07). Branch `claude/p7a-resource-governance` from `main` at `d27e7f8` (the PR #14 Phase 6C merge). Continues [Plan 0012](../completed/0012-phase6c-batched-retrieval.md); the first bounded slice of the roadmap's Phase 7 ("Production security and resilience").

**Why this slice first.** Phase 6C removed the measured reconstruction bottleneck and left no evidence that checkpoints are needed. The production-qualification matrix's two code-owned P2 items that remain closest to a correctness risk are *abandoned database work after an HTTP timeout* and *write paths without admission control*; both are about the same thing — what one request may hold (a connection, a lock, a permit, a transaction) and for how long — so they are planned together.

**Migration impact:** none expected (no schema change; if the admission budget or a cancellation registry ever needs a table, that path stops for an ADR). **Affected crates:** `ledger-api` (edge, admission, handlers), `ledger-store` (pool settings, transaction bounds, test hooks), `apps/ledger-server` (configuration validation), `apps/ledger-projector` (configuration validation), tests and qualification scripts. `ledger-core`, protocol and golden files are untouched.

## Goal
Give every request a deliberate, bounded lifecycle in the database — a connection, a transaction, a lock and an admission permit are held only as long as the request can still be answered, and never longer than configured bounds that are validated against each other — without changing any identity, atomicity, idempotency or error-taxonomy guarantee, and with every behaviour pinned by a test written before the behaviour changes.

## Scope
1. **M0 — inventory and model** (this document): every database-backed entry point and transaction, its permits, locks and lock order, timeouts and what happens on cancellation; the concurrency/resource model; the test strategy.
2. **M1 — acceptance tests first**: deterministic PostgreSQL tests (and in-process tests) for the invariants below, written against the *current* code so they document current behaviour where it is correct and fail where the plan intends a change (kept `#[ignore]`d or asserting the current behaviour with a `// Plan 0013 M2 changes this` marker until the change lands — never a placeholder reported as passing).
3. **M2 — request lifecycle vs transaction lifecycle** (ADR first): a validated timeout hierarchy; a bounded transaction duration; cancellation of abandoned database work; the error envelope distinguishing "nothing happened" from "outcome unknown, retry with the same key".
4. **M3 — shared admission budget**: every write path under a budget; permits that are never held without the resource they protect; the budget validated against the pool at startup; cheap paths and readiness keep headroom.
5. **M4 — qualification**: fault-injection and stress runs with the new behaviour, the production-qualification matrix updated, deployment documentation.

## Non-goals
No rate limiting per tenant, no metrics/OpenTelemetry (Phase 7B candidates), no connection pooler, no change to idempotency scoping, lock order, merge semantics, outbox or projection protocol, no new storage backend, no API shape change beyond the error envelope's `message`/`code` for the two timeout classes, no checkpoints.

## Invariants (must hold before and after every milestone)
```text
idempotency            same key + same digest → the original result; different digest → IDEMPOTENCY_CONFLICT
CAS semantics          a ref moves only through the predicated UPDATE inside the acceptance transaction
transaction atomicity  prepare / accept / reject / merge propose / merge apply / branch writes / validation record
                       commit all their rows or none (ADR-0013)
merge locking order    I → G → refs (sorted by branch name) → branches (same order) → P (postgres_merge.rs:841-868)
four-eyes policy       unchanged (ADR-0022 policy enforcement inside prepare/accept)
outbox atomicity       the projection_outbox row commits with the ref move (ADR-0013); delivery is fenced (ADR-0021)
tenant isolation       unchanged; admission is global: a refusal reveals aggregate load only, never a graph,
                       tenant or other caller's identity (it is not per-tenant fairness either)
stable error taxonomy  codes unchanged; a message may become more precise; a status may not change class
                       (4xx stays 4xx, 5xx stays 5xx)
no acknowledged write lost because a client disconnected
                       COMMIT is the durable-outcome boundary: before COMMIT no durable workflow result
                       exists; after a successful COMMIT the result exists and is replayable by key; the
                       HTTP acknowledgement happens later and may be lost. A success returned to the
                       client is already durable; losing a response after COMMIT never makes the
                       outcome ambiguous once the caller retries with the same key. Work the server did
                       not commit is rolled back and leaves no idempotency row
```
**HTTP cancellation ≠ transaction rollback.** The two are related only through COMMIT: a request abandoned *before* COMMIT must end in rollback (today: eventually, after the running statement finishes — M1 measured within milliseconds between statements, and up to `statement_timeout` + ≈ 90 ms during one); a request abandoned *after* COMMIT has a durable, replayable outcome and only its response is lost; the client is told the outcome is unknown and replays by key. No design in this plan may assume that dropping a future rolls anything back or infer commit status from a dropped future; the M1 tests prove which side of COMMIT a drop landed on.

## Assumptions
- sqlx 0.8.6 (`Cargo.lock`), verified in the vendored source: `sqlx-postgres` exposes **no cancellation API** (no `cancel_token`, `CancelRequest` appears only in a doc comment); when an executor future is dropped mid-statement the connection's `pending_ready_for_query_count` stays positive and `return_to_pool` → `ping` → `wait_until_ready` **blocks until PostgreSQL finishes the abandoned statement** (or `statement_timeout` cancels it), only then releasing the connection; a dropped `Transaction` *queues* `ROLLBACK` (`queue_simple_query`) that is flushed at that point, so **locks are held until the abandoned statement ends**. `pg_api::edge_timeout_slow_loris_and_body_boundary_are_bounded` already observes the abandoned query draining.
- PostgreSQL 17.2 is the compose/qualification server; PostgreSQL 15 is a tested target. `transaction_timeout` exists only from 17, so it can be a conditional hardening, not the primary bound.
- `pg_cancel_backend(pid)` is permitted for sessions of the same role without superuser privilege — but it addresses the *session*, which the pool lends to the next request as soon as the abandoned statement ends; M1 demonstrated a late cancel hitting that next request (`a_late_cancel_addressed_by_pid_hits_the_next_borrower_of_the_pooled_session`). Phase 7A therefore issues no active cancellation (ADR-0026 §3).

## M0 — inventory (read-only, 2026-10-07; file:line as of `d27e7f8`)

### M0.1 Database-backed entry points, permits and transactions
Every authenticated route first runs `authorized_graph` → one pooled query (`ledger-api/src/lib.rs:890`) **before** any permit. Permits are `try_acquire` (`lib.rs:1189, 1545, 1562, 2288, 2419, 2798`): a saturated semaphore answers 503 `RESOURCE_LIMIT` at once; nothing waits for a permit; nothing checks permit counts against the pool.

| entry point | permit | transaction | reconstructs | holds while waiting for a connection |
|---|---|---|---|---|
| `POST …/proposals` (prepare, `lib.rs:1166`) | expensive ×1, taken before the replay check | `begin_scoped` (`postgres_workflow.rs:686`) | yes, inside the transaction (`:1246`) | the permit (slot without connection, risk 7 below) |
| `POST …/accept` (`lib.rs:1353`), `…/reject` (`:1425`), `POST …/merges/apply` (`:2464`) | **none** | yes | no | — |
| `POST …/validations` (`lib.rs:1496`) | expensive ×1 only around the read transaction (dropped when it returns), then validations ×1 across the validator call and `record` | two (`postgres_validation.rs:181, 255`) | yes, holding G | validations permit across the HTTP call (no connection held) |
| `GET …/commits/{c}/state` (`lib.rs:2779`) | expensive ×1 | no (`pool.acquire`) | yes | the permit |
| `POST …/merges/preview` (`:2267`), `…/propose` (`:2379`) | expensive ×min(3, total) | preview: none; propose: `begin_merge` | yes, 3 reconstructions on one connection | the permits |
| `POST …/branches` (`:2541`), `…/branches/delete|restore` (`:2594`) | **none** | `begin_scoped` (`postgres_branches.rs:659, 841`) | no; create walks ancestry on a pooled connection outside the transaction | — |
| reads: refs, branches, status, history, log, validation read, `/ready` | none | no | log walks ≤ 1,000 | — |
| projector worker (`ledger-projector/src/lib.rs:192-600`) | `LEDGER_PROJECTOR_CONCURRENCY` workers, own 8-connection pool (hard-coded, `main.rs:341`) | `acknowledge`/`fail` | `state_at` | none across Fuseki calls |
| `ledger-admin` commands | none; pools of 1–2 connections, no session limits (`ledger-admin.rs:214, 717`) | migrate, projection enable | fs→pg migration | — |

Configuration: `ApiLimits::default` 12 expensive + 4 validation slots (`lib.rs:101-104`), `DbSessionLimits::default` 16 connections (`postgres_workflow.rs:238`); both parsed independently (`ledger-server/src/main.rs:323-370`), never compared. The pool's `acquire_timeout` is a hard-coded 10 s (`postgres_workflow.rs:251`).

### M0.2 Transactions, locks and lock order
Every workflow transaction (`begin_scoped`, `postgres_workflow.rs:681-714`) is READ COMMITTED, takes the idempotency advisory lock **I**, then the shared graph-status advisory lock **G** (`:849-878`; a graph status change takes G exclusive through the migration 0009 trigger). Then, per path: prepare B(x) `FOR SHARE`; accept R(x) `FOR UPDATE`, B(x) `FOR SHARE`, proposal lock P; reject P; create-branch R(src) `FOR SHARE`, B(src) `FOR SHARE`, unique insert of the new ref; delete/restore B(x) `FOR UPDATE` only; merge propose B(target) `FOR SHARE`; merge apply both refs in name order (target `FOR UPDATE`, source `FOR SHARE`), both branches in the same order, P. **Order on every path: I → G → refs (sorted) → branches (sorted) → P.** No two paths take two locks in opposite order; the only unordered waits are unique-index inserts of content-addressed objects, which cannot cycle because an effective patch is a subset of the requested one. `40P01` and `55P03` map to `DependencyTimeout` (`ledger-store/src/lib.rs:284-289`), so PostgreSQL's deadlock detector and `lock_timeout` are both backstops.

Long work under locks: prepare reconstructs inside the transaction holding I, G, B(x) S (`postgres_workflow.rs:1246`); validation-begin reconstructs holding G (`postgres_validation.rs:202`); merge preview/propose reconstruct three states on a pooled connection with no locks (`postgres_merge.rs:384-386`); branch creation walks ancestry on a pooled connection before its transaction. **No path writes after COMMIT**; every response is built from local values or rows read before COMMIT, so a future dropped between COMMIT and the response loses only the response and a replay by key answers it. Exceptions without a key: `mark_superseded` (`postgres_workflow.rs:1965`; a retry gets `LineageMismatch`) and the raw CAS (bootstrap/import only). The validator HTTP call runs between two transactions with no connection held; a drop before `record` commits means a retry calls the validator again and gets a new validation id (recorded_at differs). The projector holds no connection across Fuseki calls; its lease fencing on `acknowledge`/`fail` checks owner and epoch but not `lease_until` (harmless: records an observation).

### M0.3 Timeouts and cancellation today
| setting | default | relation enforced? |
|---|---|---|
| `request_timeout` (edge, `tokio::time::timeout` around the whole handler, `lib.rs:462-472`) | 30 s → 503 `RESOURCE_LIMIT` "request exceeded the configured time limit"; the handler future is dropped | only `validator_timeout < request_timeout`, and only when a validator URL is configured (`main.rs:479-506`) |
| `statement_timeout` (per statement, set at connect) | 30 s | **equals** `request_timeout`; nothing enforces `<` |
| `lock_timeout` | 10 s | bounds advisory and row-lock waits |
| `idle_in_transaction_session_timeout` | 60 s | **above** `request_timeout`; also counts CPU work between statements (`effective_delta`) |
| pool `acquire_timeout` | 10 s hard-coded | `PoolTimedOut` → `DependencyUnavailable` → 503 "database is not available; retry with the same idempotency key" (also for reads, which have no key) |
| `transaction_timeout` (PG 17), `pg_cancel_backend`, cancel token | not used | — |
| server drain on shutdown | 30 s constant (`main.rs:24`) | not tied to `request_timeout` |
| projector target timeout and limits | `number()` accepts 0 (`projector/main.rs:45-52`) | a zero target timeout fails every call and never progresses (qualification matrix P3) |

Consequence today (`lib.rs:463` + sqlx semantics above): at the edge timeout the permit is released but the connection stays busy until the statement ends, up to `statement_timeout` = 30 s; repeated timeouts can occupy connections beyond the 12 expensive slots. `RESOURCE_LIMIT` with status 503 is used for three different situations (edge timeout: outcome unknown; admission refusal: nothing done) and with 413 for size limits; an accept that times out at the edge after COMMIT is reported like "nothing done".

### M0.4 Existing test coverage (what M1 can build on)
Covered: fail points before COMMIT in prepare/accept/reject/merge/validation with exactly-once retry (`pg_workflow.rs`, `pg_merge.rs`); sequential lost-response replay for every write; concurrent same-key prepare, genesis accept, propose, branch create; expensive-slot saturation for reads and prepare (`pg_api.rs:1921`), validations saturation (`pg_validation_api.rs:1651`); edge timeout on a slow body and an abandoned read (`pg_api.rs:2098`, which *observes* the abandoned query draining and documents it); raw `statement_timeout`/`idle_in_transaction` on a pool connection (`pg_least_privilege.rs:543-591`); `lock_timeout` on G during prepare; lock-order races in `pg_merge`; replica and PostgreSQL SIGKILL in `scripts/fault.sh` with in-doubt replay. **Not covered:** any drop of a write mid-transaction (before or after COMMIT); `statement_timeout`/deadlock/serialization errors *through* a write transaction and the retry by key; pool exhaustion actually reached; same-key concurrent accept on a non-genesis head and same-key merge apply; branch-write fail points (none exist); cancellation of abandoned work (nothing asserts the session returns to idle).

Hooks (as of M0): `FailPoint` (seven points, before COMMIT only, **compiled into release builds** — `with_failpoint` was public, `postgres_workflow.rs:638`); `test_hooks::PauseHook` (test-hooks only; two points, both in merge propose). M1 moved `FailPoint` under `test-hooks` and added `HookPoint::BeforeCommit` / `AfterCommit` on every workflow transaction plus a slow-statement action (`PauseHook::slow_statement`: `pg_sleep` on the transaction's own connection).

### M0.5 Findings and disposition
| # | finding | severity | disposition |
|---|---|---|---|
| F1 | Abandoned statements run to completion after the edge timeout; the connection is pinned up to `statement_timeout`; locks (I, G, R, B) held meanwhile | P2 (matrix item) | M2 |
| F2 | `statement_timeout` = `request_timeout` = 30 s; `idle_in_transaction` 60 s > request; no validated hierarchy; no transaction-level bound (prepare's bound is the reconstruction limits) | P2 (matrix item) | M2 |
| F3 | `accept`, `reject`, `merge_apply`, branch writes take no permit; 12 + 4 slots = 16 = pool; `/ready` and `authorized_graph` need a connection; prepare holds a slot while waiting for a connection; slots never checked against the pool | P2 (matrix item) | M3 |
| F4 | One `RESOURCE_LIMIT` code for "outcome unknown" (edge timeout), "nothing done" (admission) and 413 size; pool timeout message tells key-less reads to retry with a key | P2 | M2 (message/guidance precision; codes unchanged unless the ADR decides a new code) |
| F5 | `FailPoint` is reachable in release builds | P2 (hardening) | **Fixed in M1**: `FailPoint`, `with_failpoint`, `with_validation_failpoint`, every `fail_at` call site and the projector's crash windows compile only with `test-hooks`; `check-architecture.py` proves every app's build graph excludes the feature (ledger-store and ledger-projector) and compiles a scratch crate twice (symbols unresolved without the feature, resolved with it) |
| F6 | Fault gate (`ledger-stress/src/fault.rs:789-793`) does not fail on `DEPENDENCY_TIMEOUT` although `test-strategy.md` says it does; merge crash tests assert only `is_err()` (`pg_merge.rs:1625, 1654`); the fault evidence calls "lost response after COMMIT" deterministically proven, but no fail point fires at or after COMMIT | P2 (evidence accuracy) | **Fixed in M1**: fault mode gets its own policy `fault_unexpected` (DEPENDENCY_TIMEOUT fails the run, as in stress and branches; the shared pair-verdict helper `is_expected_failure` is unchanged — every caller checked); merge crash tests assert the injected error; the post-COMMIT pause exists and is tested on every write path and over HTTP; `test-strategy.md` / tech-debt wording corrected |
| F7 | Graph status change (G exclusive) queues behind long prepares/validation reads and blocks every new workflow on that graph until `lock_timeout` | P3 | recorded; M2 decides whether G is held across reconstruction |
| F8 | `mark_superseded` has no idempotency key | P3 | tech-debt (not this slice unless M1 shows a lost-response path reaches it) |
| F9 | Projector `number()` accepts 0; projector DB session limits not configurable; concurrency not checked against its 8-connection pool; `ledger-admin` pools have no session limits | P3 | M3 (configuration validation) |
| F10 | Server drain 30 s not tied to `request_timeout`; validator check ignores reconstruction + record time | P3 | M2 |
| F11 (found by the M1 fault run) | A connection attempt during PostgreSQL crash recovery fails inside sqlx's start-up handshake with `Error::Protocol("Postgres protocol error (reading Authentication) …")` — the 57P03 the server sent is lost — and `db_error` classified `Protocol` as `Storage`: **500 `INTERNAL`** instead of 503 `DEPENDENCY_UNAVAILABLE` (165 occurrences across two PostgreSQL kills in `fault.sh 4 2 100 10`, 9 in `2 1 60 6`; zero `DEPENDENCY_TIMEOUT` in both, so the F6 policy was not the cause; not seen in the 2026-09-26 run) | P1 (a 5xx class the stress/fault gates reject; a client sees a fault where it should retry) | **Fixed in M1**: `Protocol` joins the retryable driver failures (`DependencyUnavailable`; status class 5xx unchanged, code `DEPENDENCY_UNAVAILABLE`), unit test `driver_protocol_pool_and_io_failures_are_unavailable_not_faults`; `compose.stress.yaml` now logs the qualification servers at `info` so the next such failure is diagnosable from `containers.log` (this one needed a second run to see the cause) |

## Concurrency and resource model (corrected 2026-10-07; decided in ADR-0026, proposed)

The M0 model is superseded by the four corrections below and by [ADR-0026](../../decisions/ADR-0026-request-transaction-cancellation-and-admission-lifecycle.md); the ADR is the normative text, this section records what changed and why.

**Correction 1 — admission order and what each permit bounds.** M0 recommended taking a permit *after* the protected connection/transaction was acquired. That is incompatible with any reserved-headroom claim: heavy and write requests could occupy all `N` connections before a semaphore refused anything (M1 measured exactly this: three blocked accepts fill a three-connection pool and the next read and `/ready` starve for the 10 s acquire timeout). The resources are now distinguished and each has exact acquire/release points:
```text
pool          N physical connections
db_work       database-connection admission: bounds connections held by heavy/write requests;
              capacity N − reserved_cheap; try_acquire BEFORE authorized_graph (the request's first
              pooled query); released with the handler; one admitted request holds ≤ 1 connection
              at a time (M1: the eight write paths, merge preview, branch-from-history, state read and
              history walk complete on a one-connection pool, store- and HTTP-level)
expensive     logical-work admission: bounds reconstruction CPU/memory; weight 1, or 3 for merge
              preview/propose (three reconstructions on ONE connection) — a work cost, never 3
              connections; capacity ≤ db_work; try_acquire right after db_work
validations   external validator calls (no connection held during the call); capacity ≤ db_work
reserved_cheap ≥ 2 connections never held by heavy work: refs/branch/validation-record reads,
              /ready and the graph lookup of those routes
```
Option A of the review was chosen (the global `db_work` permit before `authorized_graph` for heavy/write routes); no second production pool. The derivable guarantee is one-directional and stated so: heavy requests never hold more than `N − reserved_cheap` connections; cheap traffic can still exhaust its own headroom (best effort, bounded by `statement_timeout`; rate limiting is Phase 7B). The M0 formula `expensive + write_budget + validation ≤ N − reserved_cheap` is withdrawn (it conflated work weights with connections); no separate write budget is introduced.

**Correction 2 — idempotency invariant.** M0 wrote that a different digest under the same key "is a conflict even if the first attempt was cancelled before COMMIT". ADR-0013 is authoritative: idempotency rows are completed results, not reservations, and a rolled-back attempt leaves no row. The Phase 7A invariant is exactly:
```text
if a durable idempotency result exists:
    same key + same digest      → replay
    same key + different digest → IDEMPOTENCY_CONFLICT
if the entire first attempt rolled back before producing a durable result:
    the key remains unused (the retry executes afresh)
```
Durable key reservations are not introduced in Phase 7A (that would change ADR-0013 and need its own ADR and migration). M1 proves the invariant on the eight idempotent workflow write paths (`a_write_dropped_before_commit_…`); the validation record transaction (same shape) gets its drop test before M2 is accepted.

**Correction 3 — COMMIT terminology.** "Acknowledgement boundary" is replaced by "durable-outcome boundary" (Invariants above): before COMMIT no durable result exists; after a successful COMMIT the result is replayable; the HTTP acknowledgement is downstream and may be lost.

**Correction 4 — cancellation is correctness-sensitive.** The M0 `pg_cancel_backend(pid)` drop guard is **not** implemented: `pg_cancel_backend` addresses the backend session, the pool lends that session to the next request as soon as the abandoned statement ends, and a late cancel then cancels the next request (M1 demonstrated it). ADR-0026 §3 decides timeout-only cancellation for Phase 7A (`statement_timeout < request_timeout` as the floor; no cancel message of any kind is ever sent, so nothing can be misdirected) **and that the edge detaches an admitted store operation instead of dropping it** (review finding: a permit released on drop while the connection stays busy would under-count heavy connections by the abandoned ones and eat the reserved headroom); it records the fencing conditions any future active cancellation must prove (exclusive ownership of the `PgConnection` until the cancel settles; a control path that cannot starve behind the pool; cleanup to a reusable state before reuse) and requires such a design to bring its own gate test through the production cancellation path — the M1 race test pins the hazard.

**Timeout model and the honest transaction bound (ADR-0026 §2).** Hierarchy validated at start-up, fail closed:
```text
lock_timeout 5 s < statement_timeout 10 s < transaction_bound 20 s < request_timeout 30 s ≤ drain
transaction_bound + statement_timeout ≤ request_timeout;  idle_in_transaction ≤ request_timeout
pool_acquire_timeout (5 s, configurable) ≤ remaining request budget;  validator 15 s + statement ≤ request
```
The application deadline checked before each statement and before sending COMMIT is **not** a hard bound by itself (a statement started at `T − ε` runs on). Declared bound, both servers: `transaction_bound + one statement tail (≤ statement_timeout)`, never beyond `request_timeout`. PostgreSQL 17's `transaction_timeout` **terminates the session** (FATAL 25P04; M1 measured it: fires at the set value, backend gone, pool reconnects, classified `Storage` today), so it is a server-side backstop at `transaction_bound + statement_timeout`, not the primary bound, and 25P04 joins the retryable SQLSTATEs in M2. Any error returned by COMMIT itself is "outcome unknown; retry with the same key", never "rolled back". The red tests `future_a_transaction_exceeding_its_bound_does_not_commit_late` and `future_a_transaction_paused_past_its_bound_rolls_back_before_commit` assert the pre-COMMIT check (today: both commit); the statement tail is proven by `a_statement_timeout_inside_an_accept_…`.

**Outcome model (ADR-0026 §4, Design A).** Admission refusal → 503 `RESOURCE_LIMIT`, nothing done. Edge timeout → 503 `REQUEST_TIMEOUT` (one new stable code): reads "nothing was written; retry", idempotent writes "outcome unknown; retry with the same key" whether or not execution began (conservative; the class is decided from the route and the `Idempotency-Key` header only, never from how far the handler got). Pre-commit store failures stay `DEPENDENCY_TIMEOUT` / `DEPENDENCY_UNAVAILABLE`; an error from COMMIT itself is "outcome unknown"; key-less reads are never told to retry with a key. Design B (request lifecycle state with `CommitStarted` set before awaiting COMMIT) is recorded and not adopted.

**Deadlock and starvation scenarios** (a)–(g) of M0 stand; (a) and (d) are now measured (`p7a_heavy_writes_…`, `an_abandoned_active_statement_…`), (b) is resolved by the acquisition order above, (d) additionally by detaching instead of dropping, (e) by the same-key tests, (f)/(g) are M3/M4 work. Added by the reviews: (h) a steady stream of weight-1 work can starve a weight-3 merge acquisition (accepted fairness limitation, recorded in the ADR); (i) `/ready` is unauthenticated and its schema verification is several catalog statements, so it gets single-flight + a 1 s cache (ADR §5); (j) a same-key retry during saturation is refused by admission rather than replayed on the headroom (accepted).

## Test strategy and M1 result (tests before behaviour)
Deterministic PostgreSQL tests (`#[ignore]`, run by `test-integration.sh` and by hand on 17.2 and 15.19), every one classified; `future_*` tests are excluded from suite runs (`--skip future_`) and run once for red evidence. Hooks are `test-hooks` only. Timing is observed through `pg_stat_activity` / `pg_locks` with each observed store under its own `application_name`, never through sleeps (except where the wait is the scenario). Full table with measurements: [evidence](../../quality/evidence/plan-0013-m1-lifecycle-2026-10-07.md).

| # | strategy item | test(s) | class | result (17.2 / 15.19) |
|---|---|---|---|---|
| 1 | drop before COMMIT: prepare, accept, reject, merge propose/apply, branch create/delete/restore — rollback, no key, no partial row in 11 graph-scoped tables, locks released (`pg_locks`), session idle, retry fresh | `pg_lifecycle::a_write_dropped_before_commit_…` | PRESERVATION | pass / pass; locks released 0.6–1.6 ms / 0.6–2.2 ms after the drop; fresh retry 2–12 ms / 3–14 ms |
| 2 | drop after COMMIT (new `AfterCommit` pause) — durable rows, session idle, no locks, replay names the committed row ids | `pg_lifecycle::a_write_dropped_after_commit_…`; over HTTP `pg_api::p7a_an_edge_timeout_after_commit_…` | PRESERVATION (+ CHARACTERIZATION of today's envelope) | pass / pass |
| 3 | abandoned active statement: session active, locks held, connection pinned, released when PostgreSQL ends it (by itself or `statement_timeout`), rollback, idle, retry fresh | `pg_lifecycle::an_abandoned_active_statement_…` | CHARACTERIZATION (F1) | pass / pass; released 1.502 s / 1.502 s after a 1.5 s statement was sent; 0.809 s / 0.807 s with `statement_timeout` 700 ms (client-observed, one sample) |
| 4 | backend-reuse cancellation race (§4 of the review): the next borrower of the session is cancelled by a late `pg_cancel_backend(pid)` | `pg_lifecycle::a_late_cancel_addressed_by_pid_…` | CHARACTERIZATION (hazard; a fenced design needs its own gate through the production path) | pass / pass |
| 5 | PostgreSQL 17 `transaction_timeout` terminates the session (25P04, classified `Storage` today, pool reconnects); PostgreSQL 15 lacks it (42704) | `pg_lifecycle::pg17_transaction_timeout_…` | CHARACTERIZATION (ADR-0026 §2 input) | pass (fired at 300 ms) / pass (42704) |
| 6 | SQLSTATE mapping: 57014 (message names the statement timeout) and 55P03 through a real accept; 40P01 and 40001 from raw transactions at the production classifier; lock order untouched | `pg_lifecycle::a_statement_timeout_…`, `a_lock_timeout_on_the_ref_row_…`, `deadlock_and_serialization_failures_…` | PRESERVATION | pass / pass |
| 7 | pool exhaustion: read, `/ready`, prepare, accept | `pg_api::p7a_pool_exhaustion_…` | CHARACTERIZATION (F3/F4) | pass / pass: all 503 `DEPENDENCY_UNAVAILABLE` after 10.00 s; read told to use a key |
| 8 | heavy writes without admission starve cheap reads (three accepts blocked on `main`'s row lock fill a pool of 3) | `pg_api::p7a_heavy_writes_without_admission_…` | CHARACTERIZATION (F3) | pass / pass: read starves 10.00 s |
| 9 | same-key concurrency with a *forced overlap* (winner paused before COMMIT, three followers observed waiting on its lock): non-genesis accept; merge apply | `pg_lifecycle::concurrent_same_key_accepts_…`, `concurrent_same_key_merge_applies_…` | PRESERVATION | pass / pass |
| 10 | one connection at a time per heavy path (admission premise): eight writes, merge preview, branch from history, state read, history walk; and every heavy HTTP route incl. handler steps | `pg_lifecycle::every_heavy_store_path_…`; `pg_api::p7a_every_heavy_route_…` | PRESERVATION | pass / pass |
| 11 | transaction bound (ADR-0026 §2): no late commit with injected work; the pre-COMMIT check with real statements only | `pg_lifecycle::future_a_transaction_exceeding_its_bound_does_not_commit_late`, `future_a_transaction_paused_past_its_bound_…` | FUTURE ACCEPTANCE (M2) | **red** both: commit after 1.221 s / 1.214 s; the paused one commits |
| 12 | reserved headroom and acquisition order (ADR-0026 §5): one admitted accept blocks; with `graphs` locked, three follower classes (accept, prepare, state read) must be refused by admission without touching the pool; then read and `/ready` 200 on the headroom | `pg_api::future_p7a_reserved_headroom_…` | FUTURE ACCEPTANCE (M3) | **red**: followers answer `DEPENDENCY_TIMEOUT` after `lock_timeout` instead of `RESOURCE_LIMIT` at once |
| 13 | outcome classification (ADR-0026 §4) | `pg_api::future_p7a_an_edge_timeout_after_commit_reports_…` | FUTURE ACCEPTANCE (M2) | **red**: `RESOURCE_LIMIT` instead of `REQUEST_TIMEOUT` |
| 14 | F5 compile-time absence (feature graph + per-symbol compile probe for ledger-store and ledger-projector); projector and admin refuse a `test-hooks` build at start-up; F6 gate and assertions | `check-architecture.py`; `ledger-stress` `fault::tests::a_dependency_timeout_fails_the_fault_run_…`; `pg_merge::a_crash_at_any_merge_stage_…` | PRESERVATION | pass |

Not in M1 (red first, at the start of the milestone that needs them): hierarchy-validation unit tests on `apps/ledger-server` configuration and the projector's zero-value refusal (M2: need the configuration surface); the validation record transaction's drop-before/after-COMMIT test and its one-connection proof (needs the validation fixtures; **before M2 acceptance**, P2); admission slot accounting for merges / validate and the unauthenticated-flood test (M3); an idle-in-pool-longer-than-the-bound borrow test and the PostgreSQL 17 backstop reconnect cost (M2); the `pg_cancel_backend` storm and dropped-response proxy of `scripts/fault.sh` (M4).

## Work
- [x] M0: inventory of entry points, permits, transactions, locks and order, timeouts, cancellation semantics (sqlx source verified), existing coverage; findings F1–F10 with disposition; concurrency/resource model and test strategy (this document)
- [x] M0 corrections (2026-10-07 review): admission order and resource model, idempotency invariant restored to ADR-0013, COMMIT terminology, cancellation treated as correctness-sensitive (backend-reuse race), transaction bound made honest, outcome classification made implementable
- [x] M1: lifecycle tests 1–14 above on PostgreSQL 17.2 and 15.19, hooks under `test-hooks` only (`BeforeCommit`, `AfterCommit`, slow statement), F5 and F6 fixed; every test classified and its pre-change result recorded (Evidence); five bounded independent reviews run and every P1 resolved (below)
- [x] ADR-0026 drafted from the M1 evidence (timeout hierarchy and declared bound, timeout-only cancellation with the reuse-race proof, Design-A outcome model, `db_work` admission before `authorized_graph`, ADR-0013 idempotency preserved, compatibility/escalation rule) — **proposed; owner/architecture review is the gate before any M2 code**
- [ ] M2: hierarchy validation at startup; transaction deadline; cancellation on drop; precise timeout envelope; configurable pool acquire timeout; drain tied to the request timeout; validator headroom check
- [ ] M3: write budget and reserved headroom; permit-after-acquisition ordering; startup validation against the pool; projector and admin configuration validation
- [ ] M4: `scripts/fault.sh` and `scripts/stress.sh` on the new behaviour; `test-integration.sh`; PG 17 + 15 suites; upgrade qualification; hosted CI; qualification matrix and `deployment.md` updated
- [x] M1 independent reviews (cancellation/session reuse, admission/starvation, idempotency/transaction, test determinism/evidence, security/tenant leakage) — findings and dispositions below
- [ ] Independent reviews for M2–M4; completion report

## Decisions
1. **Tests before behaviour.** No production change lands in M2/M3 without an M1 test that failed (or was explicitly marked) against the pre-change code.
2. **COMMIT is the durable-outcome boundary** (ADR-0013 restated): the plan's cancellation design must never make a committed write unreplayable, and must never let an uncommitted one leave a key behind; the HTTP acknowledgement is downstream of COMMIT and may be lost.
3. **Cancellation is defence in depth, not a correctness mechanism, and in Phase 7A it is timeout-only**: `statement_timeout < request_timeout` is the floor; no `pg_cancel_backend` or protocol cancel is issued because a late cancel addressed by pid can hit the next borrower of the pooled session (M1); any future active cancellation needs connection fencing and must pass the backend-reuse race test.
4. **Admission is global and connection-shaped**: the `db_work` permit is taken before the request's first pooled query, its capacity is `N − reserved_cheap`, and `expensive`/`validations` are work and external-call budgets validated against it; per-tenant fairness stays a non-goal.
5. **Tests are classified** (PRESERVATION / CHARACTERIZATION / FUTURE ACCEPTANCE `future_*`); a suite is green only with `future_*` excluded by name, never silently ignored, and every future test's red run is recorded before the behaviour changes.

## Discoveries
- sqlx 0.8.6 semantics (Assumptions): the connection is pinned, not closed, by a dropped statement; `ROLLBACK` is queued, not sent.
- `accept`'s replay answer is built from the outbox join (`postgres_workflow.rs:1792`), so "no write after COMMIT" holds for it too.
- The validation path takes and drops its expensive permit inside a match arm (`lib.rs:1545`), so the permit covers the read transaction only.

## Risks
- ~~`pg_cancel_backend` may cancel a statement that has already sent COMMIT (race) — harmless~~ Not harmless: it can cancel the *next* request on the same pooled session (M1). Resolved by not issuing active cancellation in Phase 7A (ADR-0026 §3).
- The PostgreSQL 15 transaction bound overshoots by up to one `statement_timeout`; the hierarchy relation `transaction_bound + statement_timeout ≤ request_timeout` keeps it inside the request timeout, and PostgreSQL 17 removes the tail. Stated, not hidden.
- A transaction deadline that fires between statements must roll back without racing the in-flight statement's own `statement_timeout`; the mapping must stay `DependencyTimeout`.
- Reducing `statement_timeout` below 30 s can fail legitimate long statements (a 256-window chain statement is ≈ 2–8 ms; the fs→pg migration and `ledger-admin verify` run on their own pools and keep their own limits) — the ADR fixes defaults from the Plan 0011/0012 measurements.
- Permit-after-connection ordering changes the observable refusal order under saturation; tests 5–6 pin it.

## Gates
`check-fast`; `ledger-store --features postgres` and `ledger-api` PostgreSQL suites on 17 and 15; `test-integration.sh`; `scripts/fault.sh`; `scripts/stress.sh`; `upgrade-p5.sh`; fuzz; supply chain; container/security; hosted CI on the PR; independent reviews.

## Evidence
| Gate | Revision | Result |
|---|---|---|
| M0 inventories (four bounded read-only sub-agents: endpoints/permits, transactions/locks, timeouts/cancellation, test coverage) and the main session's verification of sqlx 0.8.6 source | `d27e7f8` | recorded above; no production code changed |
| `pg_lifecycle` (11 tests, `--skip future_`) on PostgreSQL 17.2 and 15.19 — `cargo test -p ledger-store --features postgres --test pg_lifecycle -- --ignored --nocapture --skip future_` | `647be51` | 11 passed / 11 passed; envelopes in the [evidence file](../../quality/evidence/plan-0013-m1-lifecycle-2026-10-07.md) |
| `pg_lifecycle` `future_` (2 tests) run once on 17.2 and 15.19 | `647be51` | both fail as intended on both (commit after 1.221 s / 1.214 s past a 500 ms bound; the paused transaction commits) |
| `pg_api` `p7a_*` (4 tests, `--skip future_`) on 17.2 and 15.19 — `cargo test -p ledger-api --test pg_api -- --ignored --nocapture p7a_ --skip future_` | `647be51` | 4 passed / 4 passed (pool exhaustion 10.002 s / 10.003 s; saturation read starved 10.002 s / 10.001 s; edge timeout after COMMIT replays; every heavy route on one connection) |
| `pg_api` `future_` (2 tests) run once on 17.2 and 15.19 | `647be51` | both fail as intended on both (followers `DEPENDENCY_TIMEOUT` instead of `RESOURCE_LIMIT`; `RESOURCE_LIMIT` instead of `REQUEST_TIMEOUT`) |
| Affected existing suites on 17.2 (`RUST_TEST_THREADS=4`): `pg_workflow` 14, `pg_merge` 27, `pg_validation` 9, `pg_branches` 18, `pg_retrieval` 11, `pg_validation_api` 23, `pg_api` 16 (`future_` filtered) | `647be51` (run before the review-driven test fixes; the M1 suites were re-run after them, rows above) | all passed |
| The same affected suites on 15.19 (`RUST_TEST_THREADS=4`): `pg_workflow` 14, `pg_merge` 27, `pg_validation` 9, `pg_branches` 18, `pg_validation_api` 23, `pg_api` 16 | `647be51` (run before the review-driven test fixes; the M1 suites were re-run after them, rows above) | all passed |
| `./scripts/test-integration.sh` (`RUST_TEST_THREADS=4`; compose 17.2; includes `pg_lifecycle`, `pg_api` with `future_` excluded, `fuseki_projection` 21 with the gated crash windows, the containerised scenario) | `647be51` (run before the review-driven test fixes; the M1 suites were re-run after them, rows above) | INTEGRATION OK |
| `./scripts/check-fast.sh` (doc links/consistency, architecture incl. the `test-hooks` compile probe, fmt, lint, unit tests) | `647be51` | passed |
| `ledger-stress` unit tests (incl. `fault::tests::a_dependency_timeout_fails_the_fault_run_but_outages_and_refusals_do_not`) | `647be51` | 6 passed |
| `scripts/fault.sh 4 2 100 10` under the new `DEPENDENCY_TIMEOUT` policy (before F11) | `647be51` (before F11) | **FAULT FAILED**: 165 × `500 INTERNAL` (F11), 0 × `DEPENDENCY_TIMEOUT`; every other gate inside the run green (4 server kills, 2 PostgreSQL kills, 1060 in-doubt replays, 0 inconsistent, Σ refs.version = Σ ref_events = Σ accepted = Σ outbox = 3973, verifier clean) |
| `scripts/fault.sh 2 1 60 6` with server warnings captured (diagnosis) | `647be51` (before F11) | **FAULT FAILED**: 9 × `500 INTERNAL`, all `ledger error mapped to INTERNAL … Postgres protocol error (reading Authentication)` during the PostgreSQL kill window |
| `scripts/fault.sh 4 2 100 10` after F11 (`target/fault/20261007T132450Z`) | `647be51` | **FAULT OK**: 4 server kills (ready again in 0.9–1.1 s), 2 PostgreSQL kills (replicas ready in 2.0–2.1 s without restart), 100 writers over 10 graphs, 966 in-doubt replays, 0 unresolved, 0 inconsistent, Σ refs.version = Σ ref_events = Σ accepted = Σ outbox = 4590, verifier clean; error classes `503 DEPENDENCY_UNAVAILABLE` 198, `503 RESOURCE_LIMIT` 15393, transport 1921; unexpected classes none — zero `DEPENDENCY_TIMEOUT` under the new policy, zero `500` after F11 |

## Independent reviews (M1, 2026-10-07)
Five bounded read-only reviews (cancellation/session reuse, admission/starvation, idempotency/transaction semantics, test determinism/fault evidence, security/tenant leakage). Dispositions:

| finding | severity | disposition |
|---|---|---|
| ADR §2 said PostgreSQL 17 `transaction_timeout` cancels the statement; it terminates the session (FATAL 25P04) and the error is unclassified today | P1 | **Fixed in the ADR** (backstop at `bound + statement_timeout`, 25P04 retryable in M2, reconnect cost measured in M2) and **measured** (`pg17_transaction_timeout_…`: fires at 300 ms, 25P04, `Storage` today, backend replaced) |
| A `db_work` permit released when the handler is dropped under-counts busy connections by the abandoned ones (scenario (d)); the headroom guarantee did not hold | P1 | **Fixed in the ADR**: the edge detaches the admitted store operation (permit lifetime = operation lifetime); the drop-release alternative is recorded as rejected |
| The future transaction-bound test ran its over-the-bound work inside the hook, where a per-statement check never runs; red for the wrong reason | P1 | **Fixed**: split into `…does_not_commit_late` (pre-COMMIT check, injected work as one unit) and `…paused_past_its_bound_…` (real statements only); the statement tail is covered by `a_statement_timeout_…`; the per-statement check is M2's unit test |
| `/ready` is unauthenticated and its schema verification is several catalog statements on the reserved headroom | P2 | **Decided in the ADR**: single-flight + 1 s cache (M3) |
| Replay lookups before admission (validation replay, merge propose stored lookup) vs "first pooled query under the permit" | P2 | **Decided**: under the permit; a retry during saturation is refused, not replayed |
| The future headroom test used `ApiLimits::default()` (E = 12, V = 4) with N = 3 and could not tell permit-before- from permit-after-`authorized_graph` | P2 | **Fixed**: E = V = 1; `graphs` locked after admission so followers of three classes must be refused without touching the pool; cheap read on a row-locked (not table-locked) `main` |
| "Every write path" overclaimed: the validation record transaction, `mark_superseded` and the raw CAS have no hooks or drop test | P2 | **Narrowed** everywhere to the eight idempotent workflow paths; the validation drop test is a prerequisite of M2 acceptance (Not in M1) |
| Same-key concurrency tests did not force an overlap (a barrier only starts tasks together) | P2 | **Fixed**: winner paused before COMMIT, three followers observed `wait_event_type = 'Lock'` before resume |
| "Locks released" was inferred from retry latency; "session idle" was not asserted; "≈ 90 ms cancel latency" was inferred from one client-side sample | P2 | **Fixed**: `pg_locks` and `pg_stat_activity` assertions on named stores with lock-release timing; the figure is now labelled client-observed, one sample, and the PostgreSQL 17 claim removed |
| The compile probe matched symbols by last name, so a leaked projector `FailPoint` could hide behind the store's error; `--offline` only; stale lockfile | P2 | **Fixed**: per-symbol match on the echoed `pub use … as ProbeN` line, online fallback, lockfile copied every run |
| An error returned by `COMMIT` itself must be "outcome unknown", never "rolled back" (synchronous replication, FATAL mid-COMMIT) | P2 | **Decided in the ADR** (§4 row; COMMIT never wrapped in a client timeout) |
| `fault.sh` might fail spuriously under the new policy (a killed replica's backend keeps locks until the kernel notices) | P2 (plausible) | **Run three times** (Evidence): zero `DEPENDENCY_TIMEOUT` in every run; the runs instead exposed F11 (500s during PostgreSQL recovery), fixed, and the final run is FAULT OK. If a future run shows lock waits from dead peers, tune `tcp_keepalives_*` / `client_connection_check_interval` in compose rather than weaken the gate |
| One tenant can hold every `db_work` permit; the plan said "never a cross-tenant information channel" | P3 | **Reworded** (aggregate load only; fairness is Phase 7B); `V < db_work` so a slow validator cannot hold every permit |
| Projector and admin binaries did not refuse a `test-hooks` build at start-up | P3 | **Fixed** (both refuse like the server) |
| Hook doc said the connection is "back in the pool" after COMMIT (the return is asynchronous); `TABLES` omitted `refs`/`commit_parents`; the HTTP after-COMMIT test did not wait for `hook.reached()`; LEAKS lacked driver phrases; the saturation session's `statement_timeout` capped the lock wait | P3 | **Fixed** |
| Weight-3 merge acquisition can be starved by weight-1 work; `immutable_objects` is shared across tenants so an identical in-flight insert from another tenant can wait on the unique index (could surface as `DEPENDENCY_TIMEOUT` with `lock_timeout` 5 s) | P3 (plausible) | Recorded in the ADR (fairness) and here (M2 checks the insert-wait class; object ids are content-addressed, not tenant-scoped) |
| `--skip future_` is a substring filter | P3 | Convention recorded in `test-strategy.md`: no other test name may contain `future_` |

Open after the reviews and the fault runs: no P0/P1 (F11 fixed and re-verified). P2: the validation record drop test and one-connection proof (before M2 acceptance). P3: `mark_superseded` hooks (F8 scope), the insert-wait class, per-tenant fairness (Phase 7B).

## Deferred work
F8 (`mark_superseded` key), per-tenant rate limiting, metrics, projector lease `lease_until` fencing on acknowledge (optional), `ledger-admin` pool session limits beyond configuration validation.

## Completion criteria
Every box checked with evidence; the ADR accepted before M2 code; every M1 test passing on PostgreSQL 17 and 15 after M2/M3 with the pre-change failure (or marker) recorded; fault and stress runs green with `DEPENDENCY_TIMEOUT` counted as a failure; the production-qualification matrix rows "Cancel PostgreSQL work after an HTTP timeout" and "Admission control for writes" moved to done with evidence; no P0/P1 review finding open; hosted CI green on the PR against `main`.
