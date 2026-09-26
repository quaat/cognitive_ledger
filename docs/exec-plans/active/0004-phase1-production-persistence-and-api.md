# Plan 0004: Phase 1 — production persistence and API foundation

## Goal
Turn the Phase 0 decisions (ADR-0008…ADR-0014) into a horizontally correct, authenticated,
idempotent write foundation — without merge, branches beyond named refs, projection
delivery, or the Sculpin validation adapter. Realises Phase 1 / priority P1 of
`product_development_plan.md` (§36 Phase 1, §41). The P0 persistent-identity ADRs were
conditionally signed off on 2026-09-26; the sign-off amendments (ADR-0009 `evidence_refs`
set + `correlation_id` removal, ADR-0010 many-graphs-per-KB, ADR-0013 lineage predicates
+ prepare idempotency) are folded into the ADRs and into the scope below.

## Scope
- Multi-graph identity (ADR-0010): `graphs`, `refs`, `projections` tables; API and authz
  scoped by `(tenant_id, graph_id)`; `refs` gains protection policy and monotonic version;
  `graphs.knowledge_base_id` is nullable and non-unique (many graphs may reference one KB).
- Commit `v2` with dual read (ADR-0009): a `v2` envelope type in `ledger-core`, a
  deterministic encoder/decoder, and checked-in golden vectors that pin absent-vs-empty
  optional fields, ordered `parents[]`, `evidence_refs[]` order-independence + dedup (a
  sorted unique set), no `correlation_id`, caller-declared `source_system`, and a fixed
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
  target existence **and the per-operation lineage predicates** (`new_head.graph_id ==
  graph_id`; genesis has no parent; advance has `parents[0] == expected_head`) enforced
  in-transaction; `LINEAGE_MISMATCH` is a stable error. Effective-delta patch semantics
  (ADR-0008) with the checkpoint-vs-reconstruction identity property test.
- Idempotency (`Idempotency-Key`) for **prepare as well as accept/reject**, scoped by
  `(tenant, actor, graph, operation, key)`; request digest + response persisted;
  `IDEMPOTENCY_CONFLICT` on key reuse with a different payload; a prepare retry after a
  lost response returns the original candidate identity.
- Resource limits (§22): body/patch bytes, operation count, term/message length, evidence
  count, ref-name length, state-export bytes/quads, reconstruction depth.
- OpenAPI description and a stable error taxonomy.

## Non-goals
No general branches or branch policies beyond named refs (Phase 4), no diff/merge/DAG
crate (Phase 5), no projection consumer — the outbox is written but not drained (Phase 3),
no Sculpin validation adapter (Phase 2), no checkpoints (Phase 6), no skolemization.
**No opportunistic Phase 2 work:** tables/interfaces may be shaped so they can later hold
`ProposalRecord`, `ValidationRecord`, and `DecisionRecord`, but no validation endpoint,
adapter, or `SemanticExecutionContext` handling is built here. P1 proves that the ledger
is a reliable, authenticated, horizontally correct immutable change service first.

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
- lineage predicates are in-transaction; tests advance to a historical ancestor, to a
  commit of another graph, and to a commit whose `parents[0] != expected_head`, and assert
  `LINEAGE_MISMATCH` with no ref movement.
- prepare idempotency: a same-key/same-digest retry returns the original candidate id; a
  different digest returns `IDEMPOTENCY_CONFLICT`.
- `graphs.knowledge_base_id` is non-unique; a test creates two graphs referencing one KB.
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

## Implementation order (fixed at sign-off; reduces incompatible-persistence risk)
```
P1.1  Persistent protocol (main session; ledger-core)
      commit v2 type + canonical field rules, dual read, golden vectors,
      graph identity types, temporal/principal types
          ↓ freeze the executable protocol
P1.2  Shared persistence (storage agent; ledger-store)
      ImmutableStore abstraction, PostgresImmutableStore, graph/ref migrations,
      two-replica reconstruction
          ↓ prove shared content correctness
P1.3  Atomic workflow persistence (storage agent; ledger-store)
      RefRepository, ref_events, decisions, idempotency (prepare + accept),
      projection_outbox, lineage predicates
          ↓ prove crash/concurrency atomicity
P1.4  HTTP/security boundary (API/security agent; ledger-api, ledger-server)
      authenticated principal, tenant/graph authz, prepare idempotency,
      resource limits, stable errors, OpenAPI
          ↓
P1.5  Adversarial qualification (testing agent; ledger-testkit, tests/)
      multi-process tests, 1,000-writer CAS, restart tests, fault injection,
      protocol golden verification, migration/upgrade tests, independent reviews
```
P1.1 is not parallelised with the database schema: `graph_id`, principal representation,
and idempotency semantics shape the storage schema, so the v2 types and identity rules are
frozen first. After that seam is fixed, P1.2–P1.4 can proceed largely independently.

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
