# Validation freshness and the binding of acceptance to a validation record

## Status
Accepted (2026-09-27, Plan 0006 / Phase 2 slices P2.2 and P2.5); amended before release (validator trust anchor, validation invocation identity). Affects persistent
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

### Acceptance names the validation *and* the semantic environment
```
accept(candidate, expected_head, validation_id, semantic_environment_id, reason?)
```
Both identifiers are required under the production policy (`validation_policy =
"validated"`). The reviewer or orchestrator states which environment it is accepting
under; Sculpin — not the ledger — knows the currently applicable environment (base KB
revision, ontology, shapes, reasoning, external-source catalog revision, validator versions)
and
can compute its id from the frozen, candidate-independent layout
`sculpin-semantic-environment/v1` (ADR-0018) before any candidate exists. Inside the single acceptance transaction, after the
idempotent-replay lookup and before any write, the repository verifies:

1. the validation record exists and belongs to the caller's graph and tenant
   (`VALIDATION_NOT_FOUND` otherwise — indistinguishable from nonexistent);
2. its `candidate_commit` is the candidate being accepted (`LINEAGE_MISMATCH`);
3. its `candidate_state_digest` equals its context's digest for that candidate (enforced by
   the composite foreign key `validation_records(context_id, graph_id, candidate_commit,
   candidate_state_digest) → semantic_execution_contexts(…)`; the repository re-reads it);
4. the record was produced by the validation service the deployment **trusts**
   (`VALIDATION_STALE` otherwise). The environment deliberately omits the ledger-side
   service identity, so this is a separate, opaque ledger policy (see "Amendment: validator
   trust anchor" below). Without a trust anchor every validated acceptance is refused. It is
   checked before the verdict, so an untrusted service's verdict is never acted on or
   disclosed;
5. its outcome is `conforms` (`VALIDATION_REJECTED` otherwise);
6. the environment of its context equals the `semantic_environment_id` the request names
   (`VALIDATION_STALE` otherwise).

Predicates 2–6 are evaluated on the verified canonical bytes of the record and its context
(hash checked, strictly decoded), never on the relational projection columns.

Only then do the ADR-0013 lineage predicates, the ref movement, ref event, accepted decision,
`decision_validations` row, projection outbox row and idempotency result commit together. A
failed predicate leaves zero accepted-workflow side effects; the candidate and every
validation record remain immutable and auditable.

### Freshness is content identity, never time
"Superseded" is not inferred by the ledger. A validation is applicable iff its context is
exactly the one the accepting party names. The stale scenario (validated under ontology O1,
acceptance required under O2) is expressed by the orchestrator naming O2's environment id,
which differs from the record's → `VALIDATION_STALE`; revalidation produces a new record in
O2's environment and acceptance with that pair succeeds if HEAD and lineage still hold. Changes in base-KB revision, ontology, shapes, reasoning configuration, the external-source
catalog revision or validator versions all change the environment id and are therefore all
covered by one rule; a change of validation service is covered by the service rule above. Deployments may later add server-side pinning policies (per branch);
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

## Amendment: validator trust anchor (2026-09-27, before release)
The first implementation derived the trusted service from the configured endpoint: with
`LEDGER_VALIDATOR_URL` unset no service was required, so a conforming historical record of
*any* service satisfied acceptance — a missing runtime setting broadened trust. Trust and
reachability are now separate:

- `LEDGER_VALIDATOR_SERVICE_ID` is the trust anchor (`ValidationTrustPolicy`, one service
  for this version). `LEDGER_VALIDATOR_URL` is only the ability to call that service; the
  client always records under the trusted id.
- Service id + URL: new validations run; records of that service are acceptable.
- Service id, no URL (outage, endpoint withdrawn): `validate` answers
  `VALIDATOR_UNAVAILABLE`; earlier records of the trusted service still satisfy
  acceptance; completed validations still replay.
- URL without service id: startup is refused. Production authentication without a service
  id: startup is refused (production acceptance always requires validation). Development
  without either: validated acceptance fails closed (`VALIDATION_STALE`); unvalidated
  acceptance stays behind its separate, conspicuous development-only switch.
- Predicate 1 is evaluated before predicate 4, so a foreign or nonexistent validation is
  `VALIDATION_NOT_FOUND` under every trust configuration; a refusal never names the
  service that produced a record.

## Amendment: validation invocation identity (2026-09-27, before release)
Keeping the validator call outside any database transaction (above) means two identical
concurrent ledger requests under one `Idempotency-Key` can both pass the replay lookup and
both call the validator; so can a retry after a crash between the validator's answer and the
record. If the validator's environment moved in between, the durable record would name
whichever environment happened to record first. The fix is at the protocol boundary, not a
lock held across the call:

- Every outbound call carries `sculpin-validation-invocation/v1` — a deterministic id over
  the authenticated idempotency scope (tenant, principal type/id, delegation, graph,
  operation `validate`, key) and the canonical request-v2 digest, with its own header for
  domain separation (layout: [`validation-protocol.md`](../design/validation-protocol.md)). It is
  sent as `invocation_id` and as the `Idempotency-Key` header: one identifier, two
  representations. Correlation ids, time, arrival order and instance never enter it.
- The Sculpin contract requires that repeated or concurrent calls with the same invocation
  identity resolve to the same logical effective semantic context and validation result
  (at-least-once delivery, exactly-once logical validation;
  [`sculpin-validation-service.md`](../design/sculpin-validation-service.md)).
- It is not semantic identity: it is never part of `CommitId`, `SemanticContextId`,
  `SemanticEnvironmentId` or `ValidationId`, and it is not stored. The ledger's own
  idempotency is unchanged: phase B records under the idempotency lock, a concurrent loser
  replays the winner's record, the same key with another body is `IDEMPOTENCY_CONFLICT`.
- A same-key retry repeats the original logical validation, even where an absent hint
  means "current": if the validator's environment moved since, the record names the
  original environment and an acceptance naming the new one is `VALIDATION_STALE`. To
  validate against the current environment, use a new `Idempotency-Key`.
- "Exactly-once logical" covers the effective context, environment and verdict, not the
  `ValidationId`: the record hashes the server-assigned `recorded_at`, so the record a retry
  commits has another id than the lost attempt would have had. Only one record per key is
  ever committed, and both concurrent responses name it.
- What this buys is determinism, not safety: without validator deduplication (or after
  Sculpin's retention expired) a record is still an honest record of the environment it ran
  in, and acceptance still requires the environment the reviewer names. The dependency on
  Sculpin is recorded as external in `docs/exec-plans/tech-debt.md`.
- Golden vectors (`fixtures/golden/validation/invocation-v1-*`) are produced by the
  independent Python reference and checked by Rust; `sculpin-validation-request/v1` gains
  the required `invocation_id` field (unreleased, amended in place).

## Alternatives considered
- **Accept with `validation_id` only; the ledger picks "current" context.** Requires the
  ledger to know Sculpin's current ontology/shapes/KB revision; rejected (boundary).
- **Expiry windows on validations.** Time is not evidence of applicability; rejected.
- **Accept names the full context id.** Rejected in review round 1: the context hashes
  candidate-specific provenance (state digest, object references, hydration digests) that
  only a finished run knows, so an orchestrator could only copy it from the record it cites
  and the stale check could never fire.
- **Accept passes the expected environment *fields* instead of the id.** Equivalent but
  larger; the id is sufficient because the layout is frozen and independently computable.
- **Synchronous validate-then-accept in one request.** Rejected by ADR-0014; a convenience
  orchestration may be layered above later.
- **Hold a lock (or an in-flight row) across the validator call.** Rejected: an external
  call must never pin a database connection, transaction or advisory lock, and an in-flight
  marker still needs the validator's cooperation after a ledger crash. The invocation
  identity gives the validator what it needs without either.
- **Trust whichever service the endpoint belongs to.** Rejected by the trust-anchor
  amendment: reachability is not trust.

## Revision (review round 1, 2026-09-27)
The first draft bound acceptance to the context id; see the rejected alternative above. The
environment id replaces it; no data or release used the draft.

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
