# Plan 0004: Phase 1 — production persistence and API foundation

## Goal
Turn the Phase 0 decisions (ADR-0008…ADR-0014) into a horizontally correct, authenticated,
idempotent write foundation — without merge, branches beyond named refs, projection
delivery, or the Sculpin validation adapter. Realises Phase 1 / priority P1 of
`product_development_plan.md` (§36 Phase 1, §41). **Do not start until the P0
persistent-identity ADRs are signed off.**

## Scope
- Multi-graph identity (ADR-0010): `graphs`, `refs`, `projections` tables; API and authz
  scoped by `(tenant_id, graph_id)`; `refs` gains protection policy and monotonic version.
- Commit `v2` with dual read (ADR-0009): a `v2` envelope type in `ledger-core`, a
  deterministic encoder/decoder, and checked-in golden vectors that pin absent-vs-empty
  optional fields, `parents[]`/`evidence_refs[]` ordering+dedup identity, and a fixed
  `recorded_at`. `v1` remains readable; unknown versions fail closed.
- Authenticated principal (ADR-0011): a service-boundary auth extractor producing
  `AuthenticatedPrincipal`; `CommitBody` loses `author`/tenant; `principal_type` derived
  from verified identity; RFC3339 `event_time` parsing/normalization; server `recorded_at`.
- Production immutable store (ADR-0012): `ImmutableStore` trait in `ledger-core`; a
  `PostgresImmutableStore` in `ledger-store`; a migration path from filesystem content; a
  two-replica read/reconstruct correctness test. No shared ref may reference node-local
  content.
- Atomic acceptance (ADR-0013): a PostgreSQL `RefRepository` whose `advance_ref` runs the
  single transaction (CAS + ref_event + decision + projection_outbox + idempotency), with
  target existence enforced as an in-transaction predicate. Effective-delta patch semantics
  (ADR-0008) with the checkpoint-vs-reconstruction identity property test.
- Idempotency (`Idempotency-Key`) scoped by `(tenant, actor, graph, operation, key)`;
  request digest + response persisted; `IDEMPOTENCY_CONFLICT` on key reuse with a
  different payload.
- Resource limits (§22): body/patch bytes, operation count, term/message length, evidence
  count, ref-name length, state-export bytes/quads, reconstruction depth.
- OpenAPI description and a stable error taxonomy.

## Non-goals
No general branches or branch policies beyond named refs (Phase 4), no diff/merge/DAG
crate (Phase 5), no projection consumer — the outbox is written but not drained (Phase 3),
no Sculpin validation adapter (Phase 2), no checkpoints (Phase 6), no skolemization.

## Relevant invariants
All product invariants, with special attention to 2 (deterministic content identity — the
v2 golden vectors), 7 (a ref never points to missing content — the in-transaction target
predicate), 8 (deterministic reconstruction — effective-delta identity), 9/10 (no silent
rewrite; projection failure cannot corrupt history), and 12 (canonicalization is protocol —
new version, dual-read, golden vectors).

## Carried-forward obligations (from the P0 reviews)
- v2 golden vectors pin absent-vs-empty and list ordering/dedup, plus `recorded_at`.
- effective-delta identity proven equal via checkpoint and full reconstruction.
- target existence is an in-transaction predicate; fault test accepts a never-prepared
  candidate and asserts rejection.
- the graph model arrives as a new monotonic migration; 0001 is never rewritten.
- multi-replica / shared-ref PostgreSQL is unsupported until the shared immutable store
  ships in this phase.

## Affected crates
`ledger-core` (`ImmutableStore` trait, v2 envelope type, `AuthenticatedPrincipal`,
effective-delta primitive), `ledger-store` (`PostgresImmutableStore`, `RefRepository`,
migrations), `ledger-api` (auth extractor, v2 surface, limits, OpenAPI, error taxonomy),
`ledger-server` (wiring/backend selection), `ledger-testkit` (multi-process harness,
in-memory reference for property tests).

## Migration impact
New monotonic migrations for graphs/refs(+version, protection)/ref_events/decisions/
projection_outbox/idempotency/immutable-content. Commit v2 is additive with dual read; v1
vectors are unchanged and v1 stays readable forever. Clean-install and upgrade paths tested.

## Suggested sub-agent decomposition (§42; disjoint ownership)
- storage/concurrency: `ImmutableStore` + `PostgresImmutableStore` + `RefRepository` + the
  atomic transaction and migrations (reviewed by `storage-concurrency-reviewer`).
- protocol: commit v2 type, encoder/decoder, golden vectors (main-session owned;
  `invariant-reviewer`).
- API/security: auth extractor, principal model, resource limits, OpenAPI, error taxonomy
  (`security-reviewer`).
- testing/fault-injection: multi-process correctness, idempotency, fault injection at the
  acceptance boundary (`test-reviewer`).
Canonicalization/protocol files stay owned by the main session.

## Gate
Multi-process/restart correctness (two replicas on a shared store agree and reconstruct);
no client can spoof the actor; retries are idempotent; no shared ref resolves to node-local
content; v2 golden vectors stable across builds; effective-delta identity property holds;
fault injection at the acceptance boundary shows no partial acceptance. Fast + integration
gates green before closing.

## Completion criteria
The Phase 1 gate passes with recorded evidence, the carried-forward obligations each have
an executable test, and independent storage/concurrency, invariant, security, and test
reviews raise no unresolved P0/P1 findings.
