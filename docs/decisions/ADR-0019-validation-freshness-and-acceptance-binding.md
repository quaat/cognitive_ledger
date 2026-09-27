# Validation freshness and the binding of acceptance to a validation record

## Status
Accepted (2026-09-27, Plan 0006 / Phase 2 slices P2.2 and P2.5). Affects persistent
atomicity: the acceptance transaction (ADR-0013) gains validation predicates and a new
enforced relation.

## Context
A validation proves "candidate + semantic context", not eternal truth. Between validation
and acceptance the base KB revision, ontology, shape set, reasoning configuration, Virtual
A-Box source versions or the validator itself may change. ADR-0014 requires acceptance to
name an immutable `ValidationRecord`; it does not say when a record is acceptable. "Latest"
inferred from timestamps would let a stale but recent record win, and teaching the ledger to
inspect ontologies or data sources would move semantic responsibility across the boundary
(spec invariant 14).

## Decision

### Acceptance names the validation *and* the context
```
accept(candidate, expected_head, validation_id, semantic_context_id, reason?)
```
Both identifiers are required under the production policy (`validation_policy =
"validated"`). The reviewer or orchestrator states which context it is accepting under;
Sculpin — not the ledger — knows the currently applicable context and can compute its id
from the frozen layout (ADR-0018). Inside the single acceptance transaction, after the
idempotent-replay lookup and before any write, the repository verifies:

1. the validation record exists and belongs to the caller's graph and tenant
   (`VALIDATION_NOT_FOUND` otherwise — indistinguishable from nonexistent);
2. its `candidate_commit` is the candidate being accepted (`LINEAGE_MISMATCH`);
3. its `candidate_state_digest` equals its context's digest for that candidate (enforced by
   the composite foreign key `validation_records(context_id, graph_id, candidate_commit,
   candidate_state_digest) → semantic_execution_contexts(…)`; the repository re-reads it);
4. its outcome is `conforms` (`VALIDATION_REJECTED` otherwise);
5. its `semantic_execution_context_id` equals the `semantic_context_id` the request names
   (`VALIDATION_STALE` otherwise).

Only then do the ADR-0013 lineage predicates, the ref movement, ref event, accepted decision,
`decision_validations` row, projection outbox row and idempotency result commit together. A
failed predicate leaves zero accepted-workflow side effects; the candidate and every
validation record remain immutable and auditable.

### Freshness is content identity, never time
"Superseded" is not inferred by the ledger. A validation is applicable iff its context is
exactly the one the accepting party names. The stale scenario (validated under ontology O1,
acceptance required under O2) is expressed by the orchestrator naming O2's context id, which
differs from the record's → `VALIDATION_STALE`; revalidation produces a new record under
O2's context and acceptance with that pair succeeds if HEAD and lineage still hold. Changes
in base-KB revision, ontology, shapes, reasoning configuration, Virtual A-Box source
versions or validator identity/version all change the context id and are therefore all
covered by one rule. Deployments may later add server-side pinning policies (per branch);
those are additive and do not weaken this rule.

### Rejection may cite validations
`reject(candidate, reason, validation_id?)` records the decision `rejected` and, when a
record is named, links it through `decision_validations` (same candidate/graph enforced by
foreign key). A non-conforming validation therefore stays auditable as "candidate C, record
V = violations, decision rejected" without moving any ref.

### Enforced relation
`decision_validations(decision_id, validation_id, graph_id, candidate_commit)` with foreign
keys to `decisions(decision_id, graph_id, candidate_commit)` and
`validation_records(validation_id, graph_id, candidate_commit)`: PostgreSQL proves that a
decision references validations of its own candidate and graph. The existing
`decisions.validation_ids` array stays and is populated identically (additive; the invariant
verifier checks agreement) — the array is informational, the relation is the guarantee.

### Validation workflow is separable and idempotent
`validate(candidate, requested_context_hints)` is its own operation with its own
`Idempotency-Key` scope (`operation = validate`). It never moves a ref. It runs in two
database transactions around the outbound validator call so no lock is held while waiting
on Sculpin: (1) idempotency lookup (replay or conflict), graph/tenant/candidate binding,
bounded reconstruction and state digest; (2) after the response — idempotency lock, replay
if a concurrent identical request completed, verify the response against the computed
candidate and digest, insert the context (content-addressed, idempotent), the record, the
violation summary and the idempotency result. Validator outage is `VALIDATOR_UNAVAILABLE`
(retryable, nothing persisted); a malformed or mismatching response is `VALIDATOR_ERROR`
(nothing persisted). The ledger remains fully operable without the validator: prepare,
reads and rejection work, acceptance stays fail-closed (`VALIDATION_REQUIRED`).

## Alternatives considered
- **Accept with `validation_id` only; the ledger picks "current" context.** Requires the
  ledger to know Sculpin's current ontology/shapes/KB revision; rejected (boundary).
- **Expiry windows on validations.** Time is not evidence of applicability; rejected.
- **Accept passes the expected context *fields* instead of the id.** Equivalent but larger
  and duplicates the encoder in the request; the id is sufficient because the layout is
  frozen and independently computable.
- **Synchronous validate-then-accept in one request.** Rejected by ADR-0014; a convenience
  orchestration may be layered above later.

## Consequences
- `WorkflowRepository::accept`/`reject` gain the validation fields and predicates;
  `ValidationPolicy` gains `Validated`; `NoValidation` stays development-only.
- New stable errors `VALIDATION_REJECTED`, `VALIDATION_STALE`, `VALIDATION_NOT_FOUND`,
  `VALIDATOR_UNAVAILABLE`, `VALIDATOR_ERROR` beside `VALIDATION_REQUIRED`.
- Request identity: accept/reject that name a validation, and validate, use
  `sculpin-ledger-request/v2` (ADR-0015 amendment); legacy accept/reject shapes keep v1 so
  P1.x retries replay across the upgrade.
- Depends on ADR-0013 (transaction), ADR-0014 (roles), ADR-0016 (privileges), ADR-0018
  (identities).
