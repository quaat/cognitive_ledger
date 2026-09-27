# Projection state, stream leases and the projector database identity

## Status
Accepted (2026-09-27, Plan 0007 / Phase 3; amended before first release on 2026-09-28 by the
Phase-3 review round: partial uniqueness, owner-only disabled transitions, enable rules,
reconciliation claims, lease sizing). Affects persistent schema (migration 0011) and the
database identity model (amends ADR-0016, see "Projector database identity").

## Context
`projection_outbox` (migration 0006) already records, in the acceptance transaction, one row
per accepted ref movement with `delivered_at` / `attempts` reserved for a Phase 3 consumer
whose grant was deferred (0008). Consuming it reliably needs: an explicit statement of which
streams are projected to which target and cognitive graph; per-stream progress that survives
crashes; exclusivity between projector replicas without holding a database transaction across
an HTTP call; bounded retries; and operator-visible failure. ADR-0020 makes the target write
itself idempotent and monotonic, so the database side only has to be crash-safe and exclusive
enough to avoid wasted work.

## Decision

### Migration 0011 (additive; 0001–0010 untouched)
`projection_state` — one row per enabled projection stream:
```
graph_id, branch, target_id                 PRIMARY KEY
tenant_id                                   (graph_id, tenant_id) → graphs
cognitive_graph                             UNIQUE (target_id, cognitive_graph)
                                            WHERE status <> 'disabled'  (partial index)
status             'active' | 'blocked' | 'rebuild_required' | 'disabled'
projected_commit, projected_ref_version    both NULL or both set;
                                           (graph_id, branch, projected_ref_version, projected_commit)
                                           → ref_events (graph_id, branch, new_version, new_head)
lease_owner, lease_until, lease_epoch       lease (owner/until both NULL or both set)
next_attempt_at, consecutive_failures       backoff
last_success_at, last_error_at, last_error_code, rebuilds
created_at
```
- Rows are created and disabled only by the operator (`ledger-admin projection enable |
  disable`, owner identity): the cognitive graph IRI is derived from the graph's
  `knowledge_base_id` by the ADR-0020 mapping at enable time. Enable refuses: a graph without
  a KB id; a graph that is not `active` (bootstrap, importing, archived); any ref other than
  `main` (ADR-0020 v1); a ref whose head was not reached by an accepted change (a bootstrap or
  imported head has no outbox event, so nothing would ever project it — accept a change
  first); and a cognitive graph already used by another non-disabled stream of the target
  (checked in the enabling transaction, and by the partial unique index against races,
  SQLSTATE 23505). Re-enabling a disabled row reactivates it.
- The uniqueness is **partial** (`status <> 'disabled'`): disabling a stream frees its
  cognitive graph, so the KB's feed can be switched to another ledger graph (disable + enable
  + rebuild; ADR-0020 `TARGET_CONFLICT`) without deleting rows, which the guard forbids.
- A guard trigger keeps the identity columns (`graph_id`, `branch`, `target_id`, `tenant_id`,
  `cognitive_graph`, `created_at`) immutable, refuses DELETE, never lets
  `projected_ref_version`, `lease_epoch` or `rebuilds` decrease, and allows a status change
  into or out of `'disabled'` only when `current_user` owns the table (SQLSTATE 42501
  otherwise): the projector may move a stream among `active`, `blocked` and
  `rebuild_required`, but can never enable or disable one.
- A second guard trigger on `projection_outbox` makes delivery monotonic: `delivered_at` once
  set never changes, `attempts` never decreases (the 0006 trigger already freezes the
  identity columns and refuses DELETE).
- `ref_events` gains `UNIQUE (graph_id, branch, new_version, new_head)` as the FK target
  binding a stream's recorded progress to a real accepted ref state.

### Outbox semantics
Eligibility and order come from `projection_state.projected_ref_version` and the outbox's
`ref_version`, never from timestamps: a stream has work iff an outbox row for its
`(graph_id, branch)` has `ref_version > coalesce(projected_ref_version, 0)`. The projector
materializes the **latest** such event (state-based projection, ADR-0020) and, in the
acknowledging transaction, sets `delivered_at` on every row of the stream up to that version
(`delivered_at` = first delivery to a target; per-target progress lives in
`projection_state`). Outbox rows are never deleted; streams that are not enabled keep their
rows pending and are reported as unconfigured.

### Leases (exclusivity without a transaction across HTTP)
1. **Claim** (one short transaction): pick one eligible, `active`, due, unleased (or
   lease-expired) stream of the projector's target with `FOR UPDATE SKIP LOCKED`, set
   `lease_owner`, `lease_until = now() + ttl`, `lease_epoch = lease_epoch + 1`; commit.
2. Outside any transaction: reconstruct, read the marker, write, read back (ADR-0020).
3. **Acknowledge** (one short transaction): update the stream only if `lease_owner` and
   `lease_epoch` still match (fencing); advance `projected_*`, reset backoff, release the
   lease, mark the outbox rows delivered. If the lease was lost, nothing is written — the
   next holder observes the target's marker and acknowledges itself.
4. **Fail**: under the same fencing, record `last_error_*`, increment
   `consecutive_failures`, set `next_attempt_at` by bounded exponential backoff with jitter
   (retryable), or set `status = 'blocked'` (permanent); release the lease.
5. **Reconcile claim**: the same claim for an `active`, unleased, **idle** stream (no
   eligible event) whose `last_success_at` is older than the reconcile interval; the work
   item is the recorded version, and a consistent target is acknowledged without a write
   (refreshing `last_success_at`). This is how a target that lost its data is repaired
   without a new acceptance (ADR-0020 "Reconciliation").
6. **Operator claim** (`rebuild`): a named stream, ignoring backoff and `blocked` /
   `rebuild_required` status, never a disabled or live-leased one.

A crashed worker's lease simply expires. Fencing is by `(lease_owner, lease_epoch)`: a
re-claim by the *same* owner name after expiry gets a new epoch, so the stale attempt is
still fenced. A step checks its elapsed time before the target write and gives up (releases)
past 75 % of the TTL; the configuration refuses `LEDGER_PROJECTOR_LEASE_SECONDS < 4 ×
LEDGER_PROJECTOR_TARGET_TIMEOUT_SECONDS + 30` (a step makes up to four target requests). If a
write still outlives its lease, correctness holds (ADR-0020 guarded writes: the late write is
a no-op against anything newer), only work is repeated.

### Projector database identity (amends ADR-0016)
ADR-0016 defined two database identities (owner, runtime). Phase 3 adds a third; the
ADR-0016 rules (no ownership, no membership in privileged roles, exact and exhaustive
privilege verification at start-up and readiness, owner-only grant functions) apply to it
unchanged.
A distinct least-privilege role `ledger_projector`, granted by the owner through
`ledger_grant_projector(role)` (`ledger-admin migrate --projector-role`):
- SELECT on `graphs`, `refs`, `immutable_objects`, `commit_index`, `commit_parents`,
  `ref_events`, `projection_outbox`, `projection_state` and `_sqlx_migrations` (schema level
  check) — nothing else: not `idempotency`, `proposals`, `decisions`, validation tables;
- UPDATE on exactly `projection_outbox (delivered_at, attempts)` and on the progress, lease,
  backoff and error columns of `projection_state`;
- no INSERT, DELETE, TRUNCATE, REFERENCES, TRIGGER; no sequence privileges; no membership in
  or ability to become the runtime role, the owner or any privileged role; may not execute
  either grant function.
The projector refuses to start unless the connected identity matches this model exactly
(the same structural checks as the runtime identity, parameterized by model). The runtime
role gains SELECT on `projection_state` (for status reads) and no write on it or on the
outbox delivery columns: the HTTP server can never mark projection progress.

## Alternatives considered
- **Lease columns on `projection_outbox` (per event).** Per-event leases allow N+1 to be
  claimed while N is in flight; ordering would need extra locking. The stream is the unit of
  ordering, so it is the unit of leasing.
- **Reuse the runtime role.** Would give the HTTP server UPDATE on delivery columns it never
  needs, and blur audit of who moved projection progress.
- **Advisory locks held during the HTTP call.** Pins a database connection across an external
  call and is lost silently on connection drop; rejected like in ADR-0019.
- **Auto-enable every graph with a KB id.** Hides the target decision; operators enable
  explicitly, and the uniqueness constraint catches conflicting enables.

## Consequences
- Projector replicas can run in parallel; they partition work by stream.
- A graph's projection is observable end to end (`ledger-admin projection status [--json]`,
  projector metrics): ledger head vs projected version, lag, pending events, oldest pending
  age, last success, last error, rebuilds, lease.
- The upgrade 0010 → 0011 needs the owner to run `migrate --projector-role` once; existing
  outbox backlog becomes consumable as soon as a stream is enabled.
