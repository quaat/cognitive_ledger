# Plan 0013: Phase 7A — runtime resource governance and PostgreSQL resilience

Status: **M0 complete (inventory, findings, concurrency/resource model, test strategy); M1 (acceptance tests) not started; no production behaviour changed** (started 2026-10-07). Branch `claude/p7a-resource-governance` from `main` at `d27e7f8` (the PR #14 Phase 6C merge). Continues [Plan 0012](../completed/0012-phase6c-batched-retrieval.md); the first bounded slice of the roadmap's Phase 7 ("Production security and resilience").

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
tenant isolation       unchanged; admission is global, never a cross-tenant information channel
stable error taxonomy  codes unchanged; a message may become more precise; a status may not change class
                       (4xx stays 4xx, 5xx stays 5xx)
no acknowledged write lost because a client disconnected
                       COMMIT is the acknowledgement boundary: work the server committed is durable and
                       replayable by key even if the response never reached the client; work the server
                       did not commit is rolled back and leaves no idempotency row
```
**HTTP cancellation ≠ transaction rollback.** The two are related only through COMMIT: a request abandoned *before* COMMIT must end in rollback (today: eventually, after the running statement finishes); a request abandoned *after* COMMIT has happened and must be reported as "outcome unknown" to the client, who replays by key. No design in this plan may assume that dropping a future rolls anything back; the tests in M1 prove which side of COMMIT a drop landed on.

## Assumptions
- sqlx 0.8.6 (`Cargo.lock`), verified in the vendored source: `sqlx-postgres` exposes **no cancellation API** (no `cancel_token`, `CancelRequest` appears only in a doc comment); when an executor future is dropped mid-statement the connection's `pending_ready_for_query_count` stays positive and `return_to_pool` → `ping` → `wait_until_ready` **blocks until PostgreSQL finishes the abandoned statement** (or `statement_timeout` cancels it), only then releasing the connection; a dropped `Transaction` *queues* `ROLLBACK` (`queue_simple_query`) that is flushed at that point, so **locks are held until the abandoned statement ends**. `pg_api::edge_timeout_slow_loris_and_body_boundary_are_bounded` already observes the abandoned query draining.
- PostgreSQL 17.2 is the compose/qualification server; PostgreSQL 15 is a tested target. `transaction_timeout` exists only from 17, so it can be a conditional hardening, not the primary bound.
- `pg_cancel_backend(pid)` is permitted for sessions of the same role without superuser privilege; the runtime identity can therefore cancel its own abandoned sessions from another pool connection (to be confirmed by a test in M1 under the least-privilege role).

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

Hooks: `FailPoint` (seven points, before COMMIT only, **compiled into release builds** — `with_failpoint` is public, `postgres_workflow.rs:638`); `test_hooks::PauseHook` (test-hooks only; two points, both in merge propose).

### M0.5 Findings and disposition
| # | finding | severity | disposition |
|---|---|---|---|
| F1 | Abandoned statements run to completion after the edge timeout; the connection is pinned up to `statement_timeout`; locks (I, G, R, B) held meanwhile | P2 (matrix item) | M2 |
| F2 | `statement_timeout` = `request_timeout` = 30 s; `idle_in_transaction` 60 s > request; no validated hierarchy; no transaction-level bound (prepare's bound is the reconstruction limits) | P2 (matrix item) | M2 |
| F3 | `accept`, `reject`, `merge_apply`, branch writes take no permit; 12 + 4 slots = 16 = pool; `/ready` and `authorized_graph` need a connection; prepare holds a slot while waiting for a connection; slots never checked against the pool | P2 (matrix item) | M3 |
| F4 | One `RESOURCE_LIMIT` code for "outcome unknown" (edge timeout), "nothing done" (admission) and 413 size; pool timeout message tells key-less reads to retry with a key | P2 | M2 (message/guidance precision; codes unchanged unless the ADR decides a new code) |
| F5 | `FailPoint` is reachable in release builds | P2 (hardening) | M1 (gate it behind `test-hooks` or prove it unreachable; a test must fail if it is reachable) |
| F6 | Fault gate (`ledger-stress/src/fault.rs:789-793`) does not fail on `DEPENDENCY_TIMEOUT` although `test-strategy.md` says it does; merge crash tests assert only `is_err()` (`pg_merge.rs:1625, 1654`); the fault evidence calls "lost response after COMMIT" deterministically proven, but no fail point fires at or after COMMIT | P2 (evidence accuracy) | M1 (fix the gate and the assertions; add the post-COMMIT hook; correct the wording with the new evidence) |
| F7 | Graph status change (G exclusive) queues behind long prepares/validation reads and blocks every new workflow on that graph until `lock_timeout` | P3 | recorded; M2 decides whether G is held across reconstruction |
| F8 | `mark_superseded` has no idempotency key | P3 | tech-debt (not this slice unless M1 shows a lost-response path reaches it) |
| F9 | Projector `number()` accepts 0; projector DB session limits not configurable; concurrency not checked against its 8-connection pool; `ledger-admin` pools have no session limits | P3 | M3 (configuration validation) |
| F10 | Server drain 30 s not tied to `request_timeout`; validator check ignores reconstruction + record time | P3 | M2 |

## Concurrency and resource model (decision input for the ADR; recommendation marked)

**Timeout hierarchy (M2).** Enforce at startup, fail closed on violation:
```text
lock_timeout < statement_timeout < transaction bound < request_timeout ≤ drain deadline
validator_timeout + reconstruction/record headroom < request_timeout
idle_in_transaction_session_timeout ≤ request_timeout   (an idle transaction cannot outlive its request)
```
Defaults to be proposed in the ADR, e.g. lock 5 s, statement 10 s, transaction 20 s, request 30 s, idle 30 s. The *transaction bound* is enforced by the ledger: a deadline carried by the workflow transaction, checked before each statement and before COMMIT (so a transaction that has exceeded its bound rolls back instead of committing late); PostgreSQL 17's `transaction_timeout` is set additionally when the server version supports it (detected at connect), never relied upon alone (PostgreSQL 15 target).

**Cancellation of abandoned work (M2). Recommended: a combination.**
1. `statement_timeout < request_timeout` as the hard floor (every abandoned statement ends by itself within a bounded time; connections and locks are released soon after the client gave up).
2. **Deliberate cancellation on drop**: each handler records its session's `pg_backend_pid()` (one cheap statement per acquired connection, or captured in `after_connect` into connection metadata) and installs a drop guard; when the request future is dropped while a statement may be running, the guard spawns a detached task that issues `SELECT pg_cancel_backend($1)` on another pool connection (bounded by a small reserved headroom; failure to cancel is logged, never fatal — the floor still applies). Rejected alternatives: raw `CancelRequest` over a fresh socket using `BackendKeyData` (sqlx does not expose the key; would need a protocol-level dependency), relying on sqlx future drop alone (does nothing server-side), killing the connection (`close_hard`, loses the pooled connection and still does not stop the server-side statement).
3. **Outcome classification at the edge**: the response for a timed-out request distinguishes "no write committed" (admission refusal, or a deadline hit before COMMIT) from "outcome unknown; replay with the same idempotency key" (deadline hit while COMMIT may have been sent). The transaction deadline above makes the second case rare and bounded.

**Shared admission budget (M3). Recommended model.**
```text
pool = N connections
reserved_cheap  ≥ 2          auth/graph lookups, ref/branch reads, /ready        (never permit-gated)
expensive       ≤ N − reserved_cheap − write_budget − validation
write_budget    ≥ 1          accept, reject, merge apply, branch writes           (new, permit-gated)
validation      ≤ …          validations (permit held across the validator call, no connection)
invariant:      expensive + write_budget + validation ≤ N − reserved_cheap, checked at startup (fail closed)
permit rule:    a permit is taken only after the connection/transaction it protects is acquired,
                or the acquisition is bounded by the permit holder's own deadline (no slot held
                through a 10 s pool wait); the pool acquire_timeout becomes configurable and
                is bounded by the request deadline
refusal:        unchanged status 503 RESOURCE_LIMIT with Retry-After guidance; 429 vs 503 is decided
                in the ADR (taxonomy change → ADR)
```

**Deadlock and starvation scenarios to model (M1 tests, M2/M3 design):** (a) all connections held by writes while reads and `/ready` starve; (b) a slot held by a request waiting for a connection held by another slot holder (today's prepare ordering); (c) a graph status change queued behind a long prepare blocking the graph (F7); (d) repeated edge timeouts pinning connections (F1); (e) two same-key concurrent writes on a contended ref; (f) `lock_timeout` on R(x) during an accept burst and the retry storm it can cause; (g) the projector pool versus its worker count.

**Retry and idempotency invariants under cancellation:** a retried request with the same key either replays the committed result or runs fresh against the current state; it never observes a half-written state; a different digest under the same key is a conflict even if the first attempt was cancelled before COMMIT (today: a cancelled attempt leaves no idempotency row, so the second digest runs fresh — the ADR records whether that stays).

## Test strategy (M1, before any production change)
Deterministic PostgreSQL tests (`#[ignore]`, `test-integration.sh`), each pinning current behaviour or marked as the behaviour M2/M3 must produce:
1. **Drop before COMMIT** for prepare, accept, merge apply, branch create/delete/restore: a `PauseHook` point before COMMIT; drop the future while paused; assert rollback, no idempotency row, locks released within `statement_timeout`, the retry runs fresh.
2. **Drop after COMMIT** (new `PauseHook` point between COMMIT and the response): assert the write is durable, the replay returns it, `pg_stat_activity` shows the session idle.
3. **Abandoned statement cancellation**: a slow reconstruction (deep history or a `pg_sleep` test query under `test-hooks`) abandoned at the edge timeout; assert the session returns to idle within the configured bound (today: within `statement_timeout`; after M2: within the cancellation bound).
4. **Error mapping through a write transaction**: `statement_timeout` (57014), `lock_timeout` (55P03), forced deadlock (40P01, two hook-held locks in opposite order), serialization (40001 if reachable) → `DependencyTimeout`, rollback, retry by key succeeds; extend `dependency_classification.rs` to every SQLSTATE in the mapping.
5. **Pool exhaustion**: pool of 2–3 with held connections; assert the status and time to refusal for a read, `/ready`, `accept`, `prepare`; after M3 assert the reserved cheap headroom keeps `/ready` and reads answering.
6. **Admission**: slot accounting for merges (3 permits), validate's two permits, the new write budget; the startup refusal of a budget exceeding the pool (unit test on configuration validation).
7. **Same-key concurrency** for accept on a non-genesis head and for merge apply (`until_waiting` pattern).
8. **Hierarchy validation**: unit tests on `apps/ledger-server` configuration (every violated relation fails startup with a named reason); projector zero values refused.
9. **F5/F6**: a test that fails if `FailPoint` is reachable in a non-`test-hooks` build (compile-time gate), the fault gate failing on `DEPENDENCY_TIMEOUT`, merge crash tests asserting the injected error.
Fault-injection script extensions (M4): SIGSTOP of a replica mid-write (network-partition stand-in), a dropped-response proxy, a `pg_cancel_backend` storm.

## Work
- [x] M0: inventory of entry points, permits, transactions, locks and order, timeouts, cancellation semantics (sqlx source verified), existing coverage; findings F1–F10 with disposition; concurrency/resource model and test strategy (this document)
- [ ] M1: acceptance tests 1–9 above (PostgreSQL 17 and 15), hooks added under `test-hooks` only, F5 and F6 fixed; every test's current-behaviour result recorded in Evidence
- [ ] ADR-0026 (request lifecycle vs transaction lifecycle: timeout hierarchy, cancellation, outcome classification, admission budget model) — accepted before M2 code
- [ ] M2: hierarchy validation at startup; transaction deadline; cancellation on drop; precise timeout envelope; configurable pool acquire timeout; drain tied to the request timeout; validator headroom check
- [ ] M3: write budget and reserved headroom; permit-after-acquisition ordering; startup validation against the pool; projector and admin configuration validation
- [ ] M4: `scripts/fault.sh` and `scripts/stress.sh` on the new behaviour; `test-integration.sh`; PG 17 + 15 suites; upgrade qualification; hosted CI; qualification matrix and `deployment.md` updated
- [ ] Independent reviews (storage/concurrency, security, test) per milestone; completion report

## Decisions
1. **Tests before behaviour.** No production change lands in M2/M3 without an M1 test that failed (or was explicitly marked) against the pre-change code.
2. **COMMIT is the acknowledgement boundary** (restated from ADR-0013): the plan's cancellation design must never make a committed write unreplayable, and must never let an uncommitted one leave a key behind.
3. **Cancellation is defence in depth, not a correctness mechanism**: `statement_timeout < request_timeout` is the floor; `pg_cancel_backend` on drop shortens the tail; neither changes what a retry observes.
4. **Admission is global and connection-shaped**: budgets are derived from, and validated against, the pool size; they are not per-tenant fairness (non-goal).

## Discoveries
- sqlx 0.8.6 semantics (Assumptions): the connection is pinned, not closed, by a dropped statement; `ROLLBACK` is queued, not sent.
- `accept`'s replay answer is built from the outbox join (`postgres_workflow.rs:1792`), so "no write after COMMIT" holds for it too.
- The validation path takes and drops its expensive permit inside a match arm (`lib.rs:1545`), so the permit covers the read transaction only.

## Risks
- `pg_cancel_backend` may cancel a statement that has already sent COMMIT (race) — harmless (the cancel is ignored once the command completed), but the test must prove the committed write survives.
- A transaction deadline that fires between statements must roll back without racing the in-flight statement's own `statement_timeout`; the mapping must stay `DependencyTimeout`.
- Reducing `statement_timeout` below 30 s can fail legitimate long statements (a 256-window chain statement is ≈ 2–8 ms; the fs→pg migration and `ledger-admin verify` run on their own pools and keep their own limits) — the ADR fixes defaults from the Plan 0011/0012 measurements.
- Permit-after-connection ordering changes the observable refusal order under saturation; tests 5–6 pin it.

## Gates
`check-fast`; `ledger-store --features postgres` and `ledger-api` PostgreSQL suites on 17 and 15; `test-integration.sh`; `scripts/fault.sh`; `scripts/stress.sh`; `upgrade-p5.sh`; fuzz; supply chain; container/security; hosted CI on the PR; independent reviews.

## Evidence
| Gate | Revision | Result |
|---|---|---|
| M0 inventories (four bounded read-only sub-agents: endpoints/permits, transactions/locks, timeouts/cancellation, test coverage) and the main session's verification of sqlx 0.8.6 source | `d27e7f8` | recorded above; no production code changed |

## Deferred work
F8 (`mark_superseded` key), per-tenant rate limiting, metrics, projector lease `lease_until` fencing on acknowledge (optional), `ledger-admin` pool session limits beyond configuration validation.

## Completion criteria
Every box checked with evidence; the ADR accepted before M2 code; every M1 test passing on PostgreSQL 17 and 15 after M2/M3 with the pre-change failure (or marker) recorded; fault and stress runs green with `DEPENDENCY_TIMEOUT` counted as a failure; the production-qualification matrix rows "Cancel PostgreSQL work after an HTTP timeout" and "Admission control for writes" moved to done with evidence; no P0/P1 review finding open; hosted CI green on the PR against `main`.
