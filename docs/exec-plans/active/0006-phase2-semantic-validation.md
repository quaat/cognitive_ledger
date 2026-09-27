# Plan 0006: Phase 2 — semantic validation coordination

Status: **in progress** (started 2026-09-27 after PR #2 merged). Branch
`claude/p2-semantic-validation` from `main` at `f81be37d14b1de102c00fd339a63784b88d725c6`
(the PR #2 merge, P1.5 production qualification). Execution slices, in order:
(P2.1) validation protocol + persistent records; (P2.2) validation workflow + acceptance
binding; (P2.3) HTTP/service boundary; (P2.4) Sculpin adapter contract; (P2.5)
semantic-context freshness and revalidation; (P2.6) integration/qualification. No gate below
is reported as passed until it is executable and has run; status per slice is recorded under
"Evidence".

## Goal
Connect the P1.5-qualified knowledge-evolution engine to Sculpin's semantic capabilities
without moving any semantic responsibility into the ledger: the ledger prepares immutable
candidates, records the exact semantic execution context a validator used, stores immutable
validation records, enforces an explicit acceptance policy over them, and moves the accepted
ref atomically only when a matching, conforming, unsuperseded validation is named. Sculpin
(pySHACL, Python reasoning, ontology/base KB, Virtual A-Box) decides semantic truth; the
ledger verifies the *integrity and applicability* of the validation record. The ledger stays
validator-agnostic, reasoning-engine-agnostic, SHACL-engine-agnostic and independent of
Fuseki projection (ADR-0014, spec invariant 14).

## Scope
1. **Protocol crate `ledger-validation-protocol`** (infrastructure-free; depends on
   `ledger-core` only): versioned `SemanticExecutionContext` v1, `ValidationRecord` v1,
   `ValidationOutcome`, `ValidatorIdentity`, `VirtualContextRef`, the validator request /
   response wire contract, and the `ValidationClient` boundary trait. Deterministic binary
   canonical encodings in the same family as commit v2 / request v1 (ADR-0018), with golden
   vectors and an independent Python reference encoder (`scripts/golden/
   semantic_context_v1_reference.py`, `validation_record_v1_reference.py`).
2. **`candidate_state_digest`** (ADR-0018): `sculpin-rdf-state/v1` over the canonical
   reconstructed dataset, computed through the bounded reconstruction interface; derived
   metadata, never part of `CommitId`. Golden and property tests.
3. **Migration 0010** (additive): `semantic_execution_contexts`, `semantic_virtual_contexts`,
   `validation_records`, `validation_violations`, `decision_validations`; `idempotency`
   gains operation `validate` / result kind `validated` / `result_validation_id`;
   `decisions` gains the FK target `(decision_id, graph_id, candidate_commit)`; content-
   addressed CHECKs on the two canonical-bytes tables; write-once triggers; the runtime
   grant function is re-issued with the Phase-2 column set (ADR-0016 amendment).
4. **Schema verifier extension**: every new table, PK/UNIQUE/FK by shape, every named CHECK
   by deparse and fingerprint, the two new content-address probes, five new write-once
   guard triggers, runtime table model and sequence model, PostgreSQL 15 and 17.
5. **`ValidationRepository`** (`ledger-store`, PostgreSQL): begin a validation (idempotency
   lookup, graph/tenant/candidate binding, bounded reconstruction and state digest),
   record an immutable validation (context + record + violations + idempotency result in one
   transaction), load / list records, verify candidate–graph association. Shares the
   application pool.
6. **Acceptance binding** (ADR-0019): `AcceptRequest` carries `validation_id` +
   `semantic_context_id`; inside the existing acceptance transaction the repository verifies
   existence, graph, tenant, candidate, state digest agreement (FK-enforced), outcome
   `conforms`, exact context match, and records `decision_validations` alongside the
   existing `validation_ids`. Any mismatch leaves zero accepted-workflow side effects.
7. **HTTP boundary**: `POST /v1/graphs/{graph}/proposals/{candidate}/validations`
   (`validate` capability, `Idempotency-Key`), `GET …/validations/{validation}` (`read`),
   `accept` body extended; new stable codes `VALIDATION_REJECTED`, `VALIDATION_STALE`,
   `VALIDATION_NOT_FOUND`, `VALIDATOR_UNAVAILABLE`, `VALIDATOR_ERROR`; request identity v2
   for the new/extended operations (ADR-0015 amendment); OpenAPI updated and router-tested.
8. **Validator client**: `ValidationClient` trait (protocol crate), `HttpValidationClient`
   (`ledger-api`, same hardening as the JWKS fetch: https in production, no redirects,
   bounded body, content-type check, timeout, credential never logged), deterministic
   `FakeValidator` for tests. The Sculpin service contract is documented in
   `docs/design/sculpin-validation-service.md` with example schemas.
9. **Resource limits**: validation metadata bytes, virtual-context count, report summary
   bytes, report reference length, validator timeout, validator response bytes, concurrent
   validations (dedicated budget beside the P1.5 expensive slots).
10. **Tests**: protocol goldens; PostgreSQL suites for persistence, atomicity, replay,
    tenant isolation, least privilege and verifier drift; API suite with the fake validator
    covering every ADR-0014 scenario (valid, invalid SHACL, reasoning-derived, Virtual A-Box
    success/rejection with two external versions, stale context, validator unavailable) plus
    revalidation, concurrency and security cases.

## Non-goals
Fuseki projection consumer (Phase 3; the outbox stays dormant), general branches,
merge, checkpoints, garbage collection, SPARQL, OWL/SHACL/reasoning inside Rust, S3 storage,
the full graph-import lifecycle, any change to commit v1/v2, patch or request-v1 canonical
bytes, live Sculpin as a workspace-gate dependency, and the P1.5 external qualification
evidence (live issuer, PITR, fencing) which stays truthful and pending.

## Invariants
All Phase 0/1/1.5 invariants unchanged. Added: (a) a validation record and its semantic
execution context are content-addressed, write-once and never enter hashed commit bytes;
(b) a candidate may carry any number of validation records (different times, contexts,
validator versions) — none is ever deleted or rewritten; (c) an accepted decision names, via
database foreign keys, validation records of exactly its own candidate and graph; (d) the ref
never moves on a validation that is missing, foreign, non-conforming, for another candidate,
or for a context other than the one the reviewer names; (e) the ledger never interprets
ontology, shapes, base-KB or Virtual A-Box identifiers — they are opaque, bounded tokens.

## Persistent data changes
Migration 0010 only (0001–0009 untouched). New tables above; `idempotency.operation` /
`result_kind` CHECKs re-issued with `validate` / `validated`; new nullable
`idempotency.result_validation_id` FK; `decisions_identity UNIQUE (decision_id, graph_id,
candidate_commit)`. No content migration. Existing decision semantics are unchanged
(`validation_ids` stays and is populated; `decision_validations` is the enforced relation).

## API changes
See scope 7. Existing routes keep their contracts; `accept` without validation fields keeps
its request-v1 identity so P1.x retries replay across the upgrade; `accept` with
`validation_id` uses request v2. `validation_policy` names `no-validation` or `validated`.

## Security boundary
`validate` is a distinct capability (`ledger.validate`). Clients never submit
`validator_identity`, `tenant`, `principal`, `recorded_at` or ids: the validator service
identity is deployment configuration (`LEDGER_VALIDATOR_SERVICE_ID`), versions come from the
validator's authenticated response, `recorded_at` is server-assigned. The validator cannot
move refs (it never holds `review`; its response only becomes a record). Foreign-tenant
validations are `VALIDATION_NOT_FOUND`, indistinguishable from nonexistent. The outbound
client is hardened like the OIDC fetch. Database: runtime role gains column-level INSERT on
the new tables and USAGE on none (content ids are text); no UPDATE/DELETE; owner unchanged.

## Sculpin contract (P2.4)
`docs/design/sculpin-validation-service.md`: synchronous `POST /validate`, request =
candidate references (graph, commit, state digest, inline bounded quads + state href) and
requested-context hints; response = effective `SemanticExecutionContext` fields, outcome,
bounded violation summary, report digest/reference, validator versions. Sculpin composes an
opaque `base_kb.revision` (from `source_graph_hash`, `shapes_hash`, ontology version) — an
explicit integration prerequisite, never guessed by the ledger.

## Resource limits
`LEDGER_LIMIT_VALIDATION_METADATA_BYTES`, `_VIRTUAL_CONTEXTS`, `_VIOLATION_SUMMARY_BYTES`,
`_REPORT_REFERENCE_BYTES`, `_VALIDATOR_TIMEOUT_SECONDS`, `_VALIDATOR_RESPONSE_BYTES`,
`_CONCURRENT_VALIDATIONS`; protocol caps (frozen) bound everything from above.

## Failure semantics
Validator unreachable/timeout/5xx → `VALIDATOR_UNAVAILABLE` (503, retry same key); validator
4xx or a malformed/mismatching response → `VALIDATOR_ERROR` (502); nothing persisted in
either case. Accept without validation → `VALIDATION_REQUIRED`; with violations →
`VALIDATION_REJECTED`; with another context → `VALIDATION_STALE`; unknown/foreign →
`VALIDATION_NOT_FOUND`; each moves nothing. Validation persisted but accept crashes → the
record remains, the ref is unchanged. Prepare and reads never depend on the validator.

## Migration impact
Stop replicas → owner `ledger-admin migrate --runtime-role` (0010 re-grants) → new build.
A pre-0010 build refuses 0010 (`ahead`), a 0010 build refuses 0009 (`behind`). No data
rewrite.

## Affected crates
new `crates/ledger-validation-protocol`; `ledger-rdf` (state digest); `ledger-store`
(migration 0010, schema verifier, `ValidationRepository`, accept binding, invariant verifier);
`ledger-api` (routes, client, limits, request identity v2); `apps/ledger-server`
(configuration); `scripts/golden`, `fixtures/golden`; docs.

## Quality gates
`./scripts/check-fast.sh` (incl. the new Python reference checks), `./scripts/
check-supply-chain.sh`, `pg_validation`, `pg_workflow`, `pg_least_privilege`, `pg_verify`,
`pg_api` on PostgreSQL 15 and 17, `./scripts/test-integration.sh`; ci-fast / ci-integration /
ci-security / ci-fuzz stay green; no existing golden vector changes.

## Acceptance evidence (all executable; deferred ≠ pass)
- Protocol goldens (Rust + Python) for context v1, record v1, state digest v1.
- PostgreSQL: context/record persistence, write-once, content-address probes, two contexts
  per candidate, replay identity, conflict, tenant isolation, verifier drift on 0010
  constraints (PG 15 + 17), least-privilege matrix with the new tables.
- Accept binding: every mismatch class refused with zero side effects; success records
  `decision_validations`; replay after lost response identical.
- API + fake validator: all ADR-0014 scenarios, revalidation, concurrency, security.
- Independent read-only reviews (architecture, invariants, storage/concurrency, security,
  test, semantic-integration) with every confirmed P0/P1 fixed before closing a slice.

## Evidence

### Slice P2.1 — protocol + persistent records (2026-09-27): implemented, executed
- ADR-0018 (context/record/state-digest identities), ADR-0019 (freshness + acceptance
  binding), ADR-0015/0016 amendments, ADR-0014 status; `ledger-validation-protocol`
  (`SemanticExecutionContext` v1, `ValidationRecord` v1, `ValidationOutcome`,
  `ValidatorIdentity`, `VirtualContextRef`, `RequestedContext`, validator request/response
  wire types, `ValidationClient` trait); `ledger_rdf::state_digest`
  (`sculpin-rdf-state/v1`); golden vectors `fixtures/golden/validation/` (7 positive, 9
  negative) and `fixtures/golden/states/` (3) produced by the independent Python references
  `scripts/golden/validation_v1_reference.py` / `state_v1_reference.py` (wired into
  `check-fast.sh`) and verified by the Rust encoders; migration 0010; schema verifier at
  `REQUIRED_SCHEMA_VERSION = 10` (18 guard triggers, 53 CHECKs by deparse + fingerprint, two
  new content-address probes, full PK/UNIQUE/FK inventory, runtime table model with the
  five new tables and `result_validation_id`); `ledger_store::verify` +9 invariants;
  `ValidationRepository` (`begin` / `record` / `load` / `list_for_candidate`, two
  transactions around the validator call); `WorkflowRepository::accept` enforces the
  ADR-0019 predicates and writes `decision_validations`; `reject` may cite a validation.
- Executed 2026-09-27 (base `f81be37`): `check-fast` Python checks (doc links, architecture,
  18 commit-v2, 6 request-v1, 16 validation-v1, 3 state vectors) exit 0; `cargo fmt --check`,
  `cargo clippy --workspace --all-targets --all-features -D warnings`, `cargo test --workspace`
  green (protocol crate 11 unit + 4 golden tests; store schema unit tests 12). Real
  PostgreSQL 17.2 (compose) and 15.19 (`postgres:15-bookworm`): `pg_validation` 5/5 on both
  (content-addressed write-once rows, two contexts per candidate with distinct ids by
  external source version, replay identity under one key, `IDEMPOTENCY_CONFLICT` for other
  hints, two racing validators → one record, foreign-tenant/unprepared/mismatching refusals
  with nothing persisted, every acceptance predicate — required, unknown, foreign graph, other
  candidate, violations, stale context — refused with an identical side-effect snapshot,
  matching pair accepted + linked + replayed, cross-candidate link refused by FK 23503,
  revalidation V1 violations → V2 conforms → accepted, HEAD race → `HEAD_CHANGED`, rejection
  citing a validation); `pg_workflow` 14/14, `pg_verify` 1/1, `pg_cas_race` 1/1 (17),
  `pg_graphs_migration` 7/7 (15); `pg_least_privilege` 15/15 on both versions incl. the new
  `validation_persistence_runs_under_the_runtime_identity_and_its_controls_are_verified`
  (runtime records + accepts under a cited validation; dropped `dv_validation_fk`,
  `vr_context_fk`, disabled `validation_records_write_once`, vacuous `vr_content_addressed`,
  dropped `idempotency_operation`, `UPDATE (outcome)` granted and `INSERT (canonical_bytes)`
  revoked each refused at start-up/readiness/identity and healthy after restoration).
- Not yet executed: `./scripts/test-integration.sh` (Docker compose end-to-end; the compose
  scenario still runs the development `no-validation` acceptance and needs no change for
  0010), `check-supply-chain.sh`, independent reviews (next step).

### Slices P2.2–P2.6: planned (see "Remaining work")

## Remaining work (handoff)
1. `ledger-api`: `POST …/validations` (`validate` capability, `Idempotency-Key`, expensive
   slot or dedicated budget), `GET …/validations/{id}`, `accept`/`reject` bodies with
   `validation_id` + `semantic_context_id`, error codes `VALIDATION_REJECTED` /
   `VALIDATION_STALE` / `VALIDATION_NOT_FOUND` / `VALIDATOR_UNAVAILABLE` / `VALIDATOR_ERROR`,
   request identity v2 (ADR-0015 amendment; vectors `request-v2-*` + Python reference),
   OpenAPI + router test, `Capability::Validate` (`ledger.validate`), `HttpValidationClient`
   (reqwest, https in production, no redirects, bounded body, content-type, timeout, token
   never logged), `FakeValidator` in `ledger-testkit`, `ApiLimits` additions, server
   configuration (`LEDGER_VALIDATOR_URL`, `_SERVICE_ID`, `_BEARER_TOKEN`, limits).
2. `docs/design/sculpin-validation-service.md` (contract with example JSON), docs updates
   (storage-boundaries, security, deployment, README, tech-debt), `pg_api` scenarios for all
   ADR-0014 cases with the fake validator, security tests (§27), `test-integration.sh`.
3. Independent reviews of P2.1 (architecture, invariants, storage/concurrency, security,
   test, semantic-integration) and fixes; then the same after P2.3.

## Sub-agent decomposition (§42)
Main session owns protocol/canonicalization (crate, encodings, goldens, ADRs), the migration
and the acceptance transaction. Bounded reviewers per slice as above.
