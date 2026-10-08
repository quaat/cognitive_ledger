# ADR-0026: Request, transaction, cancellation and admission lifecycle

## Status
**Accepted (2026-10-07, Plan 0013 / Phase 7A; M2 implements §2–§4 and §8, M3 implements §5).**
Drafted after the M1 characterization evidence existed ([Plan 0013](../exec-plans/active/0013-phase7a-resource-governance.md),
Evidence; measurements in [`plan-0013-m1-lifecycle-2026-10-07.md`](../quality/evidence/plan-0013-m1-lifecycle-2026-10-07.md)),
reviewed by five bounded independent reviews, and accepted at the start of M2 with two
clarifications from the architecture review recorded in §8 (the ownership boundary of a
detached operation starts at the route's first pooled query, not at the "store call", and
detached operations are tracked by the server through shutdown). Both are judged
clarifications of §3/§5, which already required detaching rather than dropping and a drain
that covers detached work; neither changes a decision, so M2 proceeds under this text. It builds on ADR-0013
(atomic acceptance transaction; idempotency results, not reservations) and ADR-0016 (session
limits set at connect by the runtime identity). No protocol identity, commit format, RDF
semantics, merge semantics or durable schema changes; see Compatibility.

## Context
What one HTTP request may hold in PostgreSQL — a connection, a transaction, a row or advisory
lock, an admission permit — and for how long, was never decided as one model. Plan 0013 M0
inventoried it and M1 measured it on PostgreSQL 17.2 and 15.19 (`pg_lifecycle`, `pg_api`
`p7a_*`), all on the unchanged code of `d27e7f8`:

- **Cancellation does not exist.** sqlx 0.8.6 has no cancel API. When the edge timeout drops
  a handler future, the in-flight statement keeps running server-side; the pooled connection
  is returned only once PostgreSQL has finished (or `statement_timeout` cancelled) that
  statement, and the queued `ROLLBACK` is sent then. Measured: a statement of 1.5 s pins its
  connection for 1.50 s after it was sent; with `statement_timeout` 700 ms the connection is
  back in the pool ≈ 0.81 s after the statement was sent (client-observed, one sample per
  server: the limit plus the cancel and the return to the pool). Locks are held meanwhile.
- **Between statements, a drop is cheap.** A write dropped at the pause just before `COMMIT`
  (locks held, every row written, idempotency result inserted) is rolled back and its
  same-key retry — which serializes on the idempotency advisory lock — completes in 2–13 ms
  on all eight idempotent workflow write paths; the locks are observed released in `pg_locks`
  0.6–2.2 ms after the drop. A rolled-back attempt leaves no idempotency row (ADR-0013 holds).
- **After `COMMIT`, a drop loses only the response.** Every write path's rows are durable,
  the invariants hold, and the same-key retry replays exactly.
- **A backend pid names the session, not the request.** With a one-connection pool, a
  request abandoned mid-statement, its statement allowed to finish, and the connection lent
  to the next request, a `pg_cancel_backend(pid)` issued late cancelled the *next* request's
  statement (57014). The `pg_cancel_backend` drop guard proposed in M0 has exactly this race.
- **No transaction bound exists.** Three 400 ms statements inside one accept transaction,
  each inside `statement_timeout` = 1 s, commit after ≈ 1.21 s; a transaction paused for
  0.8 s just before `COMMIT` commits too; nothing stops a transaction from running as long as
  it issues statements.
- **PostgreSQL 17 `transaction_timeout` terminates the session.** Measured: set to 300 ms, it
  fires after 300 ms with FATAL SQLSTATE 25P04 ("terminating connection due to transaction
  timeout"); the backend is gone, the pool reconnects with a new pid, and today the error is
  classified `Storage` (500). PostgreSQL 15 has no such setting (42704).
- **No reserved headroom exists.** `accept` takes no permit: three accepts blocked inside
  PostgreSQL occupy a three-connection pool completely; the next cheap read and `/ready` wait
  the hard-coded 10 s pool acquire timeout and fail with 503 `DEPENDENCY_UNAVAILABLE`, the
  read being told to "retry with the same idempotency key". A pool exhausted by any means
  fails read, readiness, prepare and accept alike after 10 s.
- **The timeout envelope conflates outcomes.** An accept whose `COMMIT` succeeded but whose
  handler was dropped by the edge timeout answers 503 `RESOURCE_LIMIT` "request exceeded the
  configured time limit" — the same as an admission refusal where nothing happened — with no
  replay guidance, although the write is durable and the retry replays it.
- **Every workflow write path needs one connection at a time** (the eight idempotent paths and
  the merge preview proved on a one-connection pool; the validation begin/record pair, the
  state read and the history walk acquire sequentially by code reading, not yet by test), which
  is what makes a connection-shaped admission permit meaningful.
- **Permits and connections have different lifetimes today.** A permit is a local RAII guard in
  the handler, released the instant the edge timeout drops the future; the connection stays
  busy until PostgreSQL ends the statement (above). Any admission model that releases the
  permit on drop therefore under-counts heavy connections by exactly the abandoned ones.

## Decision

### 1. Vocabulary
```text
COMMIT is the durable-outcome boundary.
  before COMMIT:            no durable workflow result exists; a drop ends in ROLLBACK
  after successful COMMIT:  the result exists and is replayable by its idempotency key
HTTP acknowledgement happens later and may be lost; a success returned to the client is
already durable, and losing a response after COMMIT never makes the durable outcome
ambiguous once the caller retries with the same key.
```
"Cancellation" means ending work the client has given up on; it is defence in depth and
never a correctness mechanism (ADR-0013 semantics hold whether or not it fires).

### 2. Timeout model
One hierarchy, validated at server start-up (fail closed with a named reason) and in the
projector for its own pool:

```text
lock_timeout        <  statement_timeout  <  transaction_bound  <  request_timeout  ≤  drain_deadline
transaction_bound + statement_timeout  ≤  request_timeout
request_timeout + statement_timeout    ≤  drain_deadline      (the longest detached operation, §3)
statement_timeout ≤ idle_in_transaction_session_timeout ≤ request_timeout
pool_acquire_timeout                   ≤  request_timeout     (every single acquire is additionally
                                                              wrapped in min(acquire, remaining budget))
validator_timeout + statement_timeout  ≤  request_timeout     (when a validator is configured; the
                                                              call is also capped by the remaining budget)
```

| setting | default (was) | enforced by |
|---|---|---|
| `lock_timeout` | 5 s (10 s) | PostgreSQL, per session at connect (ADR-0016) |
| `statement_timeout` | 10 s (30 s) | PostgreSQL, per session at connect |
| `transaction_bound` | 20 s (none) | the ledger (below); PostgreSQL 17 `transaction_timeout` as a server-side backstop at `transaction_bound + statement_timeout` |
| `idle_in_transaction_session_timeout` | 30 s (60 s) | PostgreSQL, per session at connect |
| `pool_acquire_timeout` | 5 s (10 s, hard-coded) | sqlx pool, configurable; a request never waits past its own deadline |
| `request_timeout` | 30 s | the edge (`tokio::time::timeout` around the handler) |
| `drain_deadline` | 40 s (30 s constant) — ≥ acquire + bound + statement, so detached work finishes | graceful shutdown |
| `validator_timeout` | 15 s (20 s) | the validation client |

**The transaction bound, honestly.** Every workflow transaction (`begin_scoped`) carries a
deadline `T = min(begin + transaction_bound, request deadline)` (`begin` is after the pool
acquire, so the bound never includes the wait for a connection; the cap means **no `COMMIT` is
ever sent after the request deadline**). Before **every** statement of the transaction and before
*sending* `COMMIT`, the deadline is checked; at or past `T` the transaction is rolled back and
the request fails with `DependencyTimeout` (`DEPENDENCY_TIMEOUT`, retry by key). The check is
structural, not a convention: a bounded transaction does not dereference to a connection, and
the only way to run SQL on it is the checked accessor `Statements::stmt(phase)`, which every
helper the transaction calls — lookups, locks, publications, index writes, reconstruction
windows, idempotency rows — obtains immediately before each statement (PR #17 review P1: a
helper that ran several statements behind one outer check could keep starting statements past
the deadline on PostgreSQL 15, which has no backstop). The same accessor on a pooled
request-path connection refuses to start a statement after the request deadline. Only
`test-hooks` builds have an unchecked accessor, for the injected statements that simulate a
check-skipping bug in the PostgreSQL 17 backstop test. That check alone is not a hard bound — a statement started at `T − ε` runs on.
The declared bound, the same on both supported servers, is:

```text
a transaction holds its connection and locks for at most
    T + one in-flight statement tail, the tail ≤ statement_timeout
    (+ the cancel and pool-return latency, client-observed ≈ 0.1 s), i.e. ≤ 30 s with the
    defaults — never beyond request_timeout (transaction_bound + statement_timeout ≤
    request_timeout) and, because T ≤ the request deadline, never a COMMIT after the client's
    budget ended
```
PostgreSQL 17's `transaction_timeout` **terminates the session** (FATAL, SQLSTATE 25P04) — it
does not cancel the statement and leave the connection reusable — so it is set as a
server-side *backstop* at `transaction_bound + statement_timeout`, where the ledger's own
check and `statement_timeout` have normally already ended the transaction: it fires only if
the ledger misbehaves, and then the pool reconnects. 25P04 is classified as retryable
(`DependencyTimeout`: the transaction is gone; retry by key) in M2 — today it would fall to
`Storage`/500 (M1 measured the termination: 300 ms setting → fired at 300 ms, 25P04, new
backend). The ≈ 0.1 s figure is the client-observed release after `statement_timeout`, one
sample per server; the backstop's reconnect cost is measured in M2 before it is relied on. A remaining-budget-aware `SET LOCAL statement_timeout` before statements near the
deadline would make the bound exact at the cost of one round trip per such statement; **not
adopted** in Phase 7A (the overshoot is bounded and inside the request timeout). Any per-
transaction GUC the ledger ever sets uses `SET LOCAL` inside the explicit transaction, so
nothing leaks to the next borrower. The M1 tests pin the contract: `a_statement_timeout_
inside_an_accept_…` proves the statement tail is cut by `statement_timeout`;
`future_a_transaction_exceeding_its_bound_does_not_commit_late` (red today) proves a
transaction past its bound rolls back at the pre-`COMMIT` check instead of committing late
(its injected work is one uninterruptible unit, so it exercises the pre-`COMMIT` check, not the
per-statement check); `future_a_transaction_paused_past_its_bound_rolls_back_before_commit`
(red today) proves the pre-`COMMIT` check with real statements only. The per-statement check
is M2's own unit test. The graph-status lock G stays held across prepare's reconstruction
(Plan 0013 F7); the bound now caps that hold.

### 3. Cancellation model: timeout-only, and the edge detaches instead of dropping
```text
statement_timeout < request_timeout   is the cancellation floor:
  every statement an abandoned request left running ends by itself or is cancelled by
  PostgreSQL within statement_timeout; the connection then returns to the pool and the
  ROLLBACK releases the locks. Measured: released ≤ statement_timeout + ≈ 0.1 s after the
  statement was sent (client-observed).
No active cancellation is issued: the ledger never calls pg_cancel_backend,
pg_terminate_backend or sends a protocol CancelRequest.
The edge never drops an admitted database-bearing operation: it DETACHES it. The handler
runs the WHOLE database-bearing part of the route — from the authorized_graph lookup (the
first pooled query) through replay lookups, repository operations and the last pooled
query — as one tracked task that owns the admission permits (§8); at the request deadline
the handler stops waiting and answers REQUEST_TIMEOUT, while the task runs to its own
bounded end (transaction_bound + statement tail, or completion) and only then releases
the permits.
```
Detaching, not dropping, is what makes the admission bound real (§5): a dropped future would
free its permit while its connection stays busy for up to `statement_timeout`, so repeated
edge timeouts could push heavy connection usage past the budget into the reserved headroom
(scenario (d) of Plan 0013). It also removes the only place where sqlx's drop-mid-statement
semantics mattered: no store future is ever dropped by the edge, so a detached write either
commits (durable, replayable: the client was told the outcome is unknown) or rolls back at a
bound. Because every acquisition is bounded by the request budget, every transaction deadline is
capped by the request deadline and the validator call is capped by the remaining budget, a
detached operation ends at most `request_timeout + one statement tail` after it began (40 s
by default); the drain deadline is validated to cover exactly that. Spawning needs `'static`
futures (owned request values and `Arc`s), which the handlers provide.
**Why not the M0 `pg_cancel_backend(pid)` drop guard.** `pg_cancel_backend(pid)` addresses a
backend session. The pool lends that session to the next request as soon as the abandoned
statement ends, so a cancel that arrives after that (the common case for a detached task
racing a statement that is about to finish) cancels an unrelated request — demonstrated by
`a_late_cancel_addressed_by_pid_hits_the_next_borrower_of_the_pooled_session`. A late cancel is
therefore *not* harmless. Active cancellation is acceptable only under a design that proves
all of: (a) the target backend still belongs exclusively to the abandoned operation when the
cancel is issued; (b) a delayed cancel cannot affect a later borrower (the connection is not
returned to the pool until the cancel has settled: `ReadyForQuery` reached, transaction rolled
back, connection known reusable); (c) the control path cannot starve behind the exhausted pool
(a dedicated control connection, not a pooled one); (d) cleanup reaches a reusable state before
reuse. Option A (an owner task keeping exclusive ownership of the `PgConnection` until the
cancel settles) satisfies these; Option B (a protocol-level `CancelRequest` with the backend
key sqlx does not expose) needs a protocol dependency for the same fencing problem. Both are
**deferred**: correctness beats shaving the cancellation tail, and the measured tail with the
new defaults is at most `statement_timeout` = 10 s after the client gave up (the old defaults
allowed 30 s). Revisit only with the fenced ownership design and the backend-reuse race test
flipped to assert the later borrower is never cancelled; that test is the release gate.

**Proof that cancellation cannot affect a later borrower in Phase 7A:** no cancellation message
of any kind is sent, so there is nothing to misdirect. `statement_timeout`, `lock_timeout`,
`idle_in_transaction_session_timeout` and `transaction_timeout` are enforced by the backend on
its *own* current statement or transaction and cannot cross sessions; none of them is armed
while a connection sits idle in the pool (they time statements and open transactions, not
idleness outside a transaction), and the only session settings that persist across borrowers
are the ones `after_connect` sets on purpose. M2 adds the acceptance test "a connection idle
in the pool longer than `transaction_bound`, then borrowed, serves normally".
The M1 backend-reuse test pins the *hazard* (it issues the cancel itself, through a raw pool):
it is not the gate for a fenced design. Such a design must bring its own gate test driving the
production cancellation path with an injectable delay between deciding and delivering the
cancel, covering a cancel that lands before the statement ends, during `ROLLBACK`, and after
the session was reused, and asserting the later borrower's workflow commits and the
connection was never lent out before the cancel settled.

### 4. Outcome model (Design A — conservative, no lifecycle state)
The edge does not know where inside a handler a timeout struck, and inferring commit status
from a dropped future is forbidden. Client guidance is therefore decided from what the edge
*does* know — the response class and whether the request is an idempotent write:

| situation | status / code | message guidance |
|---|---|---|
| admission refusal (permit unavailable) | 503 `RESOURCE_LIMIT` | nothing was done; retry later (unchanged) |
| size limit | 413 `RESOURCE_LIMIT` | unchanged |
| read request timed out at the edge | 503 `REQUEST_TIMEOUT` | nothing was written; retry |
| idempotent write timed out at the edge (whether or not execution began; the detached operation may still commit) | 503 `REQUEST_TIMEOUT` | the outcome is unknown; retry with the same idempotency key (it replays a committed result or executes afresh) |
| failure raised by a statement before `COMMIT` was sent (statement/lock timeout, deadlock, serialization, the ledger's deadline) | 503 `DEPENDENCY_TIMEOUT` | rolled back; retry with the same key (unchanged) |
| an availability or timeout error returned by `COMMIT` itself (connection lost, I/O, protocol, FATAL 25P04/57P01, 57014 while committing) | 503 `DEPENDENCY_UNAVAILABLE` or `DEPENDENCY_TIMEOUT` by class | **the outcome is unknown**; retry with the same key — never "rolled back": under synchronous replication or a lost reply the transaction may be durable, and the retry replays it (ADR-0013: the stored result is read under the idempotency lock in a fresh snapshot) |
| an ordinary ERROR raised by `COMMIT` (a deferred integrity trigger of migrations 0009/0012 firing at commit) | 500 `INTERNAL` (unchanged) | the server reported the failure, so the transaction is definitely rolled back; this is a defect to investigate, not a retry |
| database unavailable / pool acquire timeout | 503 `DEPENDENCY_UNAVAILABLE` | reads: not available, retry; writes: retry with the same key |
| committed result whose response was lost | (the retry) 200 `replayed: true` | — |

The class is decided from the route and the presence of an `Idempotency-Key` header only,
never from how far the handler got. `COMMIT` is never wrapped in a client-side timeout.

`REQUEST_TIMEOUT` is one new stable code (additive; the status class stays 5xx; the OpenAPI
document lists it). It replaces `RESOURCE_LIMIT` for the HTTP time limit — a client that keyed
on `RESOURCE_LIMIT` for timeouts must accept `REQUEST_TIMEOUT` (the only client-visible code
change of Phase 7A; `RESOURCE_LIMIT` keeps meaning admission or size). The class is decided from
the method and the `Idempotency-Key` header (a mutating method with a key is an idempotent
write; everything else, merge preview included, is a read). Key-less reads are never told to
retry with an idempotency key. The
envelope reveals only the caller's own route class and aggregate load (already observable
through latency); no graph, tenant or other caller's identity enters it. Design B
(a monotonic request lifecycle state `NotStarted → RunningPreCommit → CommitStarted →
Committed → ResponseReady`, with `CommitStarted` set before awaiting `COMMIT` and a timeout in
that state conservatively "unknown") would let the edge say "nothing committed" for timeouts
before `CommitStarted`; its operational value is small because the retry-by-key contract
already resolves the ambiguity, and it adds shared mutable state to every handler. **Not
adopted**; may be revisited if operators need the distinction in logs/metrics (Phase 7B).

### 5. Admission model
Three resources, each with its own semaphore, exact acquire/release points, and capacity
derived from and validated against the pool:

```text
pool                 N physical PostgreSQL connections (max_connections, default 16)

db_work              bounds pooled connections held by heavy and write requests
  capacity           N − reserved_cheap                      (reserved_cheap ≥ 2, default 2 → 14)
  acquire            try_acquire (non-blocking) in the handler, after authentication and the
                     capability check, BEFORE authorized_graph and before any other pooled
                     query of the route — the idempotent-replay shortcuts included; refusal is
                     503 RESOURCE_LIMIT with nothing done
  release            when the DETACHED store operation completes (§3) — not when the handler
                     is dropped; by then every pooled connection it used is released or in
                     its asynchronous return to the pool (an ε of one round trip, not a
                     statement)
  routes             prepare, accept, reject, validations (held across the validator call: no
                     connection is held then, but the capacity for the record transaction is;
                     the validations permit is taken BEFORE the reconstruction so a busy
                     validator costs no wasted work), merge preview / propose / apply, branch
                     create / delete / restore, GET …/commits/{c}/state, history / log walks
  not under it       refs, branch list / status (bounded page), validation record reads, /health,
                     and the graph lookup those routes make: bounded single-statement or small-
                     page reads on the reserved headroom
  /ready             at most one schema verification runs at a time; concurrent probes get the
                     cached result (≤ 1 s old) — an unauthenticated flood costs one connection

expensive            bounds reconstruction work (CPU / memory), not connections
  capacity           E ≤ db_work                              (default 12)
  weights            1 for prepare, state read, validation begin; 3 for merge preview / propose
                     (three reconstructions on ONE connection — a work cost, never 3 connections)
  acquire / release  try_acquire right after db_work (both non-blocking, so no wait cycle);
                     released with the handler

validations          bounds concurrent calls to the external validator
  capacity           V < db_work                              (default 4): a slow validator can
                     never hold every db_work permit while the pool sits idle
  acquire / release  before the validator call; after the record transaction (unchanged)

start-up invariants  reserved_cheap ≥ 2;  db_work = N − reserved_cheap ≥ 1;  E ≤ db_work;
                     V < db_work;  projector workers ≤ projector pool  — fail closed
```

**Reserved-headroom guarantee, derived.** An admitted heavy operation holds at most one
connection at any time (M1 on a one-connection pool: the eight write paths, the merge preview,
branch creation from a historical commit, the state read and the history walk; validation
begin/record by code reading, its test is an M2 prerequisite), it cannot touch the pool before
holding a `db_work` permit, and the permit outlives every connection it uses (§3: detached, not
dropped), so at every instant `connections held by heavy operations ≤ db_work = N −
reserved_cheap` (+ connections in their one-round-trip return to the pool): at least
`reserved_cheap` connections are never held by heavy work and are available to cheap routes,
`authorized_graph` on those routes, and `/ready`. What is **not** guaranteed, and said so:
cheap traffic can exhaust its own headroom (it is unbounded; each statement is bounded by
`statement_timeout`; per-tenant fairness and rate limiting are Phase 7B / gateway concerns),
one tenant can hold every `db_work` permit (the budget is global and reveals aggregate load to
every tenant — it is not a per-tenant information channel, but it is not fairness either), a
merge's weight-3 `expensive` acquisition can be starved by a steady stream of weight-1 work
(non-blocking acquisition; accepted, recorded), and an admitted heavy request may wait for a
connection when cheap traffic is heavy — each acquire is wrapped in `min(pool_acquire_timeout,
remaining budget)`, so holding the permit while waiting is correct (the permit bounds heavy
connection usage, which is exactly what is being waited for). Consequences of taking the
permit before `authorized_graph`: under saturation a caller naming a missing or foreign graph
gets 503 instead of 404 (it learns only that the system is busy; 404 semantics are unchanged
otherwise), and a same-key retry during saturation is refused with `RESOURCE_LIMIT` (nothing
done; retry later) rather than replayed on the headroom — accepted, because the alternative
breaks the bound. Permits are taken in the handler, after authentication: an unauthenticated
flood can never consume one (M3 acceptance test). The old `expensive + write_budget +
validation ≤ N − reserved_cheap` formula is withdrawn: it mixed work weights with connections.
A separate write budget is not introduced; whether `db_work` is later partitioned by class is
a tuning question with no invariant value.

Refusal status: 503 `RESOURCE_LIMIT` with `Retry-After` guidance, unchanged (429 was considered
and rejected: it means "the client is sending too much", which a global budget cannot assert).

### 6. Idempotency (ADR-0013, preserved verbatim)
```text
idempotency rows are completed results, not reservations
if a durable result exists:   same key + same digest      → replay
                              same key + different digest → IDEMPOTENCY_CONFLICT
if the whole first attempt rolled back before producing a durable result:
                              the key remains unused; the retry executes afresh
```
A cancelled, timed-out, refused or dropped attempt never leaves a key behind (M1: the eight
idempotent workflow write paths; the validation record transaction has the same
`begin_scoped`/`record_result` shape and gets its drop test before M2 is accepted). Durable key
reservations are explicitly **not** introduced; they would change ADR-0013 and need their own
ADR and migration.

### 7. Compatibility and escalation
No change to protocol identity, canonical bytes, commit or patch formats, RDF or merge
semantics, lock order, outbox or projection protocol, or the database schema. Additive: one
error code (`REQUEST_TIMEOUT`), more precise messages, new configuration
(`LEDGER_DB_TRANSACTION_TIMEOUT_MS`, `LEDGER_DB_ACQUIRE_TIMEOUT_MS`,
`LEDGER_RESERVED_CHEAP_CONNECTIONS`, `LEDGER_DRAIN_TIMEOUT_MS`; changed defaults for the
existing `LEDGER_DB_*_TIMEOUT_MS`, `LEDGER_LIMIT_REQUEST_SECONDS` and `LEDGER_LIMIT_VALIDATOR_SECONDS`), start-up validation that
refuses inconsistent settings (a deployment with `statement_timeout` ≥ `request_timeout` no
longer starts); `25P04` joins the retryable SQLSTATEs (M1 already made the driver's
`Protocol` failures retryable after the fault gate exposed 500s during PostgreSQL crash
recovery, Plan 0013 F11); `ledger-projector` and `ledger-admin` gained the server's
start-up refusal of a `test-hooks` build in M1. If M2 or M3 turns out to need a table (an admission registry, a cancellation
registry, key reservations) or a different idempotency persistence model, Phase 7A stops and
that becomes a separate architectural decision — never smuggled into M2.

### 8. Ownership boundary and detached-operation tracking (clarifications, M2)
**Ownership boundary.** `authorized_graph` is itself a pooled query, so the boundary cannot
be "the store call": once a route is admitted to database work, the entire database-bearing
route operation — from the first `authorized_graph` query through replay lookups, repository
operations and the last pooled query — is owned by one bounded, tracked, detached operation.
An HTTP timeout may stop waiting for that operation; it never drops it. Authentication, the
capability check and bounded request parsing (body size, JSON) stay outside the operation, in
the ordinary handler, and may be dropped freely: they touch no pooled connection. The
canonical request identity is computed inside the operation, after the graph lookup, so the
error precedence of every route is unchanged (a missing or foreign graph is `NOT_FOUND` before
a malformed body is `INVALID_REQUEST`); it is pure CPU work on an already bounded body. The readiness
probe's schema verification (several catalog statements) runs on one request-budgeted
connection and obtains each statement through the checked accessor, so a `/ready` whose
client has timed out stops at its next catalog statement (PR #17 review of `dadb1f2`, P1). The M3 `db_work` permit attaches to exactly this operation lifetime; the
`expensive` and `validations` permits already do in M2 (owned permits moved into the task). A
request-side connection acquisition inside the operation never waits beyond the request's
remaining budget (`min(pool_acquire_timeout, remaining)`), and an acquisition attempted after
the budget is spent fails at once (`DependencyUnavailable`, nothing done). **Transaction
start-up is cancellation-safe** (PR #17 review P1): the acquisition is the only database step
raced against a client-side timer; a budget that ends between the acquisition and `BEGIN`
returns the clean connection without beginning anything; `BEGIN` itself is awaited in a task
of its own that is never dropped — in sqlx 0.8.6 the client-side transaction depth is
incremented only after `BEGIN`'s `ReadyForQuery`, so a begin future dropped in that window
would return to the pool a connection PostgreSQL still considers inside a transaction, which
the next borrower would unknowingly reuse. If the awaiting future is ever dropped, the begin
task still completes and drops its transaction there (queueing `ROLLBACK`), so the pool never
receives a connection with an open transaction the client does not know about; a `BEGIN` that
cannot complete is an error and its connection is not reused. Regression tests:
`pg_lifecycle::a_request_budget_that_ends_before_begin_…` and
`a_caller_dropped_during_begin_never_returns_an_open_transaction_to_the_pool`. Concretely: an operation whose
client has already received `REQUEST_TIMEOUT` therefore stops at its next connection
acquisition — typically before its transaction began — and the retry by key executes afresh;
an operation already inside a transaction runs to its bound and commits or rolls back on its
own terms (the retry then replays or executes afresh). Both outcomes are covered by the
"outcome unknown; retry with the same key" guidance the client was given.

**Tracking through shutdown.** Axum's graceful shutdown waits for HTTP connections, not for
tasks that outlive a timeout response. The server therefore owns a tracker of detached
operations with these lifecycle semantics: (1) the server owns the tracker; (2) every detached
database-bearing operation is registered in it before it runs; (3) on the shutdown signal the
server stops accepting new HTTP work and (4) closes the tracker to new detached operations (a
route reaching it answers 503 `DEPENDENCY_UNAVAILABLE` "shutting down", nothing done) — both
wake from the same signal, in no guaranteed order, which is harmless: a request admitted in the
window is tracked and drained, one refused in the window did nothing; (5) ordinary HTTP
requests and tracked detached work are given the configured `drain_deadline`, which is
validated to cover the longest detached operation (`request_timeout + statement_timeout ≤
drain_deadline`, §3); (6) the process exits when both have completed or the
drain deadline is reached, logging only counts (never request contents, tenant data,
idempotency keys, SQL or credentials); (7) a forced exit at the deadline is never described as
proving the rollback of an ambiguous `COMMIT` — PostgreSQL ends the abandoned session's
transaction by its own rules, and the retry-by-key contract (§6) resolves the outcome.

## Alternatives considered
- **`pg_cancel_backend` on drop (M0 proposal).** Rejected for Phase 7A: cancels the next
  borrower of the session (demonstrated); safe only with connection fencing (Option A).
- **Protocol `CancelRequest` with the backend key (Option B).** Needs a protocol-level
  dependency and the same fencing; deferred with Option A.
- **Permit after connection acquisition (M0 model).** Rejected: heavy requests could occupy
  every connection before any permit was refused (demonstrated), so no headroom was reserved.
- **Permit released when the handler is dropped (RAII in the handler).** Rejected: the
  connection outlives the dropped future by up to `statement_timeout`, so the budget would be
  under-counted by exactly the abandoned requests (review finding); hence detach, not drop.
- **Replay lookups before admission.** Rejected: a heavy route's first pooled query must be
  under the permit for the bound to hold; the cost is a `RESOURCE_LIMIT` on retries during
  saturation.
- **A second, reserved production pool for cheap traffic.** Rejected: doubles connection
  budgets and session-limit configuration for a guarantee the acquisition order already gives.
- **Lifecycle state at the edge (Design B).** Not adopted; see §4.
- **Durable key reservations.** Rejected: changes ADR-0013; out of scope.
- **Budget-aware per-statement timeouts on PostgreSQL 15.** Not adopted; see §2.

## Consequences
- M2 implements §2–§4 and §8 (hierarchy validation, transaction bound, PostgreSQL 17
  `transaction_timeout` backstop and its measurement, `25P04` classification, configurable
  acquire timeout bounded per acquire by the request budget, the tracked detached operation
  and the drain covering it, validator headroom check, `REQUEST_TIMEOUT` envelope and
  per-class messages, COMMIT errors as "outcome unknown"); M3 implements §5 (db_work with the
  detached lifetime, acquisition order incl. replay lookups, `/ready` single-flight + cache,
  start-up validation of the admission budget, projector configuration validation incl. its
  own pool's hierarchy). Each lands only against the M1 tests that are red today
  (`future_*`) plus the preservation tests that must stay green.
- Operators get shorter defaults: statements over 10 s, lock waits over 5 s and transactions
  over 20 s fail with `DEPENDENCY_TIMEOUT`. Plan 0011/0012 measurements put a 256-window
  statement at milliseconds and a prepare at the 10,000-depth ceiling at ≈ 0.3 s, so legitimate
  work is far inside the defaults; the fs→pg migration and `ledger-admin verify` keep their own
  pools and limits.
- The production-qualification rows "Cancel PostgreSQL work after an HTTP timeout" and
  "Admission control for writes" close with M2/M3 evidence; active cancellation stays an open
  hardening item with its gate test.

## Gate
Plan 0013 M1 tests on PostgreSQL 17 and 15: the preservation tests green before and after
M2/M3; `future_*` red before and green after; the backend-reuse race test unchanged (no active
cancellation); `scripts/fault.sh` and `scripts/stress.sh` green with `DEPENDENCY_TIMEOUT`
counted as a failure; hierarchy validation unit tests; hosted CI.
