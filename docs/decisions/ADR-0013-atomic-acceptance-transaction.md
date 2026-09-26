# Atomic acceptance transaction

## Status
Accepted (design); implemented in Phase 1 (outbox consumer in Phase 3).

## Context
The bootstrap `RefStore` is a pure compare-and-set primitive: `head := new_head`.
Production acceptance of a cognitive change must record more than a head move, and it must
record it atomically: the ref movement, an immutable ref event, the accept/reject
decision, an idempotency result, and a durable projection intent. The architecture
requires ref events and durable, retryable projection. If any of these are written
non-atomically, a crash can leave a moved ref with no decision, a lost outbox event, or a
projection that can never catch up.

## Decision
Introduce a PostgreSQL application repository (an acceptance/ref repository) whose
`advance_ref` runs entirely in one SQL transaction:

```
advance_ref(graph, branch, expected_head, new_head,
            actor, operation, decision, idempotency_key)

1. verify/CAS the expected ref            (predicated UPDATE / INSERT, as in Plan 0002)
2. update ref head and bump Ref.version   (ADR-0010)
3. append an immutable ref_event
4. append the decision/acceptance record  (ADR-0014 DecisionRecord)
5. insert a projection_outbox event
6. store the idempotency result
7. commit
```

The immutable candidate commit is written earlier, during prepare (ADR-0014), into the
shared immutable store (ADR-0012). No message-queue acknowledgement occurs before this
transaction commits — the projector consumes the outbox only after commit (transactional
outbox). Idempotency is scoped by at least `(tenant, actor, graph, operation,
idempotency_key)`; the request digest and final response are persisted. Same key + same
payload returns the original result; same key + different payload returns
`IDEMPOTENCY_CONFLICT`.

The pure `RefStore` CAS primitive remains for the filesystem/dev backend; production
acceptance goes through this repository transaction. Rejection records a decision and does
not move the ref.

Target existence (invariant 7) MUST be an explicit predicate *inside* the transaction —
a foreign key from the ref/new_head to the shared immutable content table, or an
equivalent existence check — rather than trusting that prepare ran first. This keeps
"a ref never points to missing content" true even under a buggy or reordered caller, and
does not rely on `Ledger::advance_ref`'s application-layer check alone.

### Lineage predicates per operation type (P0 sign-off amendment, 2026-09-26)
Target existence is necessary but not sufficient: without a lineage check a buggy caller
could CAS `main` from `C100` back to an existing historical `C20`, or sideways to an
unrelated commit, and still satisfy "the target exists". `advance_ref` therefore takes an
explicit `operation` and enforces, inside the same transaction, the rules for that
operation. Every rule also requires `new_head.graph_id == graph_id` (ADR-0009/0010).

```
genesis     expected_head = none;   new_head.parents = []
advance     new_head.parents[0] == expected_head
merge       new_head.parents[0] == expected_head (target HEAD)
            new_head.parents[1] == source HEAD named in the request       (Phase 5)
reset       distinct, audited administrative operation; never implied by advance;
            requires a protection-policy exception and records its own ref_event
            and decision kind                                             (if ever allowed)
```

Reading the parents of `new_head` requires decoding the candidate commit inside the
transaction (or a `commit_parents` index written at prepare time and verified against the
content id); either way the predicate is evaluated against immutable content, not against
caller-supplied claims. A request whose operation does not match its lineage fails with a
stable `LINEAGE_MISMATCH` error and moves nothing.

### Idempotency covers prepare, not only accept/reject
`recorded_at` is server-generated and identity-bearing (ADR-0009), so a client that loses
the response to a successful prepare and retries would otherwise mint a second candidate
with a different `CommitId`. Prepare is therefore idempotent under the same
`Idempotency-Key` rules as accept/reject: the key, request digest, and the resulting
candidate identity are persisted atomically with the candidate's publication, a replay
with the same digest returns the original candidate, and a different digest returns
`IDEMPOTENCY_CONFLICT`. This matters most once autonomous agents are the callers and
retries are automatic.

## Alternatives considered
- **Separate writes per concern.** A crash between writes yields partial acceptance: a
  moved ref with no decision, or a missing outbox event that strands the projection.
- **Acknowledge a queue before durability.** Produces phantom acceptances that cannot be
  reconstructed; violates the "history is authoritative" model.

## Consequences
- Phase 1 adds `ref_events`, `decisions`, `projection_outbox`, and `idempotency` tables
  and the repository; the Phase 3 projector consumes the outbox idempotently.
- Depends on ADR-0010 (ref identity/version), ADR-0012 (candidate durable before the ref
  advances), and ADR-0014 (decision/validation references).

## Gate
Fault injection at each step (§35) shows: no ref points to missing content, no
acknowledged decision disappears, no partial acceptance is observable, retries are safe,
and outbox delivery is idempotent. The suite MUST include: an `accept` that references a
never-prepared candidate, rejected by the in-transaction existence predicate; an
`advance` whose `new_head` exists but is a historical ancestor of `expected_head`, an
`advance` to an existing commit of another graph, and an `advance` whose `parents[0]` is
not `expected_head`, all rejected by the lineage predicate; and a prepare retried after a
simulated lost response, returning the original candidate identity.
