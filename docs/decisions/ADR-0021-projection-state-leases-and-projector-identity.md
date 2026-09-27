# Projection state, stream leases and the projector database identity

## Status
Accepted (2026-09-27, Plan 0007 / Phase 3). Affects persistent schema (migration 0011) and
the database identity model (ADR-0016 amendment).

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
  `knowledge_base_id` by the ADR-0020 mapping at enable time; a graph without one is refused.
- A guard trigger keeps the identity columns (`graph_id`, `branch`, `target_id`, `tenant_id`,
  `cognitive_graph`, `created_at`) immutable, refuses DELETE, and never lets
  `projected_ref_version` decrease or `lease_epoch` decrease.
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
A crashed worker's lease simply expires. The lease TTL must exceed the projector's
reconstruction plus target timeout; if it does not, correctness still holds (ADR-0020
conditional write), only work is repeated.

### Projector database identity (ADR-0016 amendment)
A distinct least-privilege role `ledger_projector`, granted by the owner through
`ledger_grant_projector(role)` (`ledger-admin migrate --projector-role`):
- SELECT on the ledger tables (reconstruction, graphs, refs, events, outbox, state);
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
- A graph's projection is observable end to end (`ledger-projector status`, metrics):
  ledger head vs projected version, lag, oldest pending age, last error.
- The upgrade 0010 → 0011 needs the owner to run `migrate --projector-role` once; existing
  outbox backlog becomes consumable as soon as a stream is enabled.
