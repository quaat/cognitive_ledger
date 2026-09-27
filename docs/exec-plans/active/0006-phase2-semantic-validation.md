# Plan 0006: Phase 2 — semantic validation coordination

Status: **in progress — P2.1–P2.4 implemented and reviewed; P2.5 freshness implemented; P2.6 qualification partly executed** (started 2026-09-27 after PR #2 merged). Branch
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

### Slice P2.1 — protocol + persistent records: implemented, executed, reviewed (2 rounds)
- ADR-0018 (context / environment / record / state-digest identities) and ADR-0019
  (freshness + acceptance binding), ADR-0015 and ADR-0016 amendments, ADR-0014 status.
- `ledger-validation-protocol`: `SemanticExecutionContext` v1, `SemanticEnvironment` v1
  (candidate-independent: base KB, ontology, shapes, optional reasoning, Sculpin-declared
  `sources_revision`, validator versions), `ValidationRecord` v1 (verdict, reported-result
  count, bounded summary; conforming verdicts may carry non-blocking results),
  `VirtualContextRef`, `SourcePin`, validator request/response, `ValidationClient`. Strict
  decoders; every set ordered by element encoding. `ledger_rdf::state_digest`
  (`sculpin-rdf-state/v1`).
- Golden vectors: `fixtures/golden/validation/` (12 positive incl. mixed-length object refs,
  one environment shared by another candidate hydrating other sources, warnings; 15
  negative, each asserted by its rejection reason) and `fixtures/golden/states/` (3), produced
  by the independent Python references (`validation_v1_reference.py`, `state_v1_reference.py`,
  strict like Rust) and checked by Rust and by `check-fast.sh`. Commit v1/v2, patch and
  request-v1 vectors unchanged.
- Migration 0010 (unreleased; revised twice during review, dev databases reset each time):
  content-addressed write-once contexts/records, virtual-context projection, bounded
  summaries, `decision_validations`, record→context FK binding graph, candidate, state digest
  and validator, composite idempotency→record FK with shape CHECK, re-issued
  `ledger_grant_runtime`. Schema verifier at level 10: 18 guard triggers, 56 CHECKs by deparse
  + fingerprint, three content-address probes matched by constraint name, full FK/PK/UNIQUE
  inventory, runtime table model. `ledger-admin verify`: +11 SQL checks plus Rust decode of
  every record/context with column agreement.
- `ValidationRepository`: `replayed` / `begin(_with_limits)` / `record` (two transactions
  around the validator call; private tickets; identical records across keys shared) /
  `load` / `list_for_candidate`; acceptance and replay decide on hash-verified bytes.

### Slice P2.2 — acceptance binding: implemented, executed
- `ValidationPolicy::Validated { validation_id, semantic_environment_id }`; inside the
  acceptance transaction: record exists for graph + tenant (`VALIDATION_NOT_FOUND`), same
  candidate (`LINEAGE_MISMATCH`), record/context agreement, conforming
  (`VALIDATION_REJECTED`), produced by the configured validator service and run in the named
  environment (`VALIDATION_STALE`); `decision_validations` + `validation_ids` written with
  the decision. Reject may cite a validation.

### Slices P2.3 / P2.4 — HTTP boundary, validator client, Sculpin contract: implemented, executed
- Routes `POST /v1/graphs/{graph}/proposals/{candidate}/validations` (`validate`),
  `GET …/validations/{validation}` (`read`); accept/reject bodies with `validation_id` and
  `semantic_environment_id`; codes `VALIDATION_REJECTED`, `VALIDATION_STALE`,
  `VALIDATION_NOT_FOUND`, `VALIDATOR_UNAVAILABLE`, `VALIDATOR_ERROR`; request identity v2
  (6 vectors, Rust + Python); OpenAPI 1.1.0-p2, router-tested; `Capability::Validate`
  (`ledger.validate`); limits (validation budget, validator timeout, response cap, shipped
  state bytes, hint bytes); replay before admission; no reconstruction without a configured
  validator; reconstruction capped at shippable bytes; ignored hints refused; summaries normalized deterministically.
- `HttpValidationClient` (https, loopback-only http in development, no redirects, no proxy,
  total timeout, streamed cap, strict content type and shape, redacted errors); server
  configuration `LEDGER_VALIDATOR_URL` / `_SERVICE_ID` / `_TOKEN_FILE` (64 KiB cap).
- `docs/design/sculpin-validation-service.md`: request/response schemas, derived identities,
  pySHACL mapping, Sculpin prerequisites, timeouts/retries/idempotency, transport rules.

### Executed gates (2026-09-27; exit codes recorded from the invoking shell, logs under `target/p2/`)
| Gate | Result |
|---|---|
| `./scripts/check-fast.sh` (fmt, clippy `-D warnings`, workspace tests, doc links, architecture guard incl. the protocol crate, all Python reference checks: 18 commit-v2, 12 request, 28 validation, 3 state vectors) | exit 0 (round-3 content) |
| `./scripts/check-supply-chain.sh` (audit, deny, SBOM 206 components) | exit 0 (run on `205272a`; no dependency change since) |
| PostgreSQL 17.2 and 15.19 on the round-3 content: `pg_validation` 9, `pg_verify` 2, `pg_workflow` 14, `pg_least_privilege` 16, `pg_validation_api` 12, `pg_api` 13 (`pg_graphs_migration` 7 on `2650156`) | all passed on both |
| `validator_http` (local server) 5 | passed |
| `scripts/fuzz.sh 120 validation_decode` (sanitizer none) | 10.7 M executions, cov 1131, no crash |
| `./scripts/test-integration.sh` (compose PostgreSQL 17.2, distroless image, all 10 PostgreSQL suites incl. `pg_validation` and `pg_validation_api`, end-to-end container scenario, `ledger-admin verify`) | exit 0, `INTEGRATION OK`, `VERIFY OK` on `2650156` and again on the round-3 content `cbaedf9` |

### ADR-0014 scenarios (executable, passing; `pg_validation_api`, deterministic fake validator)
valid candidate → accepted; invalid SHACL → `VALIDATION_REJECTED`, ref unchanged, rejection
citing the record; reasoning-derived violation naming the reasoning profile → refused; Virtual
A-Box version A conforms / B violates, both records coexist; stale environment (O1 validated,
O2 required) → `VALIDATION_STALE`, revalidation under O2 → accepted; validator unavailable /
timeout / unconfigured → `VALIDATOR_UNAVAILABLE`, prepare and reads work, accept
`VALIDATION_REQUIRED`, history unchanged; revalidation V1 violations → V2 conforms → accepted
(`pg_validation`). **Scope note:** the fake decides by scripted rules on quad text; these prove
the ledger's coordination, not semantics. A live Sculpin/pySHACL end-to-end run is external
evidence and has not been run (no Sculpin endpoint exists).

### Reviews
- Round 1 on `562f468` (invariant, storage/concurrency, semantic-integration, test; Opus,
  read-only): P1s fixed — object-ref sort mismatch between encoder and decoder/reference;
  freshness key not implementable (context id); verifier checks without failure tests;
  privilege matrix missing new tables; write-once tests passing for the wrong reason; no fault
  injection for the new writes; context content address untested. P2s fixed as listed in the
  commit messages (`205272a`, `9440a62`).
- Round 2 on `b2c9579` (security, semantic-integration re-review): security no P0/P1, P2s
  fixed (reconstruction before the state cap, reconstruction without a validator, raw pins
  forwarded); semantic P1 fixed — per-run source pins made the environment
  candidate-dependent → Sculpin-declared `sources_revision`; P2s fixed (configured-validator
  rule, ignored hints refused, pySHACL message normalization, contract gaps).
- Accepted/recorded (tech-debt): runtime is the trusted writer of new records; live Sculpin
  endpoint pending; `upgrade.sh` from 0009 with populated data not re-run; a principal holding
  both `validate` and `review` can pin an older environment Sculpin still honours (production
  role maps should keep them separate; per-branch server-side pinning is a later policy).

- Round 3 on `2650156` (semantic-integration + storage + invariant, Opus): P1s fixed —
  source-version pins remained a hint and could alias the current environment → pins removed
  from hints (only `sources_revision` selects source versions); ADR-0015 v2 text stale →
  amended. P2s fixed: `sources_revision` deployment-declared and required with hydrated
  sources (+ negative vector), configured-validator rule tested
  (`acceptance_requires_the_configured_validation_service`), no-validator behaviour stated
  in ADR-0019, plan evidence corrected.

## Remaining Phase-2 work
1. A further independent review of the round-3 changes before merge (optional; no open P0/P1).
2. `scripts/upgrade.sh` from the P1.5 release with populated data (0009 → 0010).
3. Sculpin: implement the service contract; then a live end-to-end run as external evidence.
4. Longer fuzz campaign of `validation_decode` under both sanitizers on the hosted runner
   (`ci-fuzz` picks the target up automatically).

## P1.5 external blockers (unchanged, still pending)
Live Entra ID issuer smoke test; deployment PITR/WAL evidence; writer fencing / restore
operational evidence. Phase 2 does not change them; the service is **not production-qualified**.

## Sub-agent decomposition (§42)
Main session owns protocol/canonicalization (crate, encodings, goldens, ADRs), the migration
and the acceptance transaction. Bounded reviewers per slice as above.
