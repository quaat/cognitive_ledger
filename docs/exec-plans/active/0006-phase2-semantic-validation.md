# Plan 0006: Phase 2 — semantic validation coordination

Status: **merge candidate — P2.1–P2.6 implemented, locally qualified and reviewed; hosted CI on the final head and the live Sculpin run are external evidence (see "Closure" below)** (started 2026-09-27 after PR #2 merged). Branch
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
   `semantic_environment_id` (the draft's `semantic_context_id` was replaced in review
   round 1); inside the existing acceptance transaction the repository verifies existence,
   graph, tenant, candidate, state digest agreement (FK-enforced), the trusted validation
   service, outcome `conforms`, exact environment match, and records `decision_validations`
   alongside the existing `validation_ids`. Any mismatch leaves zero accepted-workflow side
   effects.
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

## Closure (2026-09-27): main integration, trust anchor, invocation identity, qualification
Labels: **implemented** (code + docs in the branch), **executed ✓** (ran here, passed, log
under `target/p2/` or the named run directory), **not executable here**, **pending external**.

### Main integration and dependencies — executed ✓
- `origin/main` `d12c1b7` merged with `--no-ff` as `1724216` (no conflicts; `Cargo.toml` keeps
  `crates/ledger-validation-protocol` and `sha2 = "0.11.0"`; `Cargo.lock` resolved by cargo,
  unchanged by resolution; the fuzz workspace lock re-resolved for sha2 0.11 in `a71fd09`).
  Brought in: `actions/checkout` 6.1.0 → 7.0.1, `dependency-review-action` 4.9.0 → 5.0.0,
  `sha2` 0.10.9 → 0.11.0 (0.10.9 remains only under `sqlx-core`). SQLx 0.9 (Rust 1.94) is out
  of scope; tech-debt entry added; MSRV stays 1.89.
- **sha2 0.11 protocol gate — executed ✓:** `check-fast.sh` on `1724216` with no fixture
  change: commit v1 (3 tests) and v2 (18 vectors), patch v1, request v1/v2 (12), semantic
  context / environment / record (28), state digest (3) — Rust goldens and every independent
  Python reference byte- and hash-identical.

### Validator trust separated from reachability — implemented, executed ✓
- Before: `AppState::with_validation` installed the client *and* the required service; with
  `LEDGER_VALIDATOR_URL` unset no service was required, so a conforming record of any
  service satisfied acceptance.
- Now (ADR-0019 amendment "validator trust anchor"): `ValidationTrustPolicy` from
  `LEDGER_VALIDATOR_SERVICE_ID`; the URL is only the client. URL without id, token file
  without URL, empty token file with production auth, and production auth without an id are
  refused at start-up; no trust anchor → validated acceptance `VALIDATION_STALE`; trust is
  checked before the verdict (an untrusted verdict is never disclosed); foreign/nonexistent
  records stay `VALIDATION_NOT_FOUND` under every configuration; `AppState` seeds trust from
  the store and never silently replaces it.
- Tests: `ledger-server` `validator_trust_is_the_service_id_and_never_the_endpoint`
  (configuration matrix); `pg_validation::acceptance_requires_the_configured_validation_service`
  (other service, no anchor, NOT_FOUND without anchor, untrusted non-conforming → STALE);
  `pg_validation_api::validator_trust_is_independent_of_the_endpoint_and_fails_closed`
  (restarts: S1+URL → validate; S1 without URL → `VALIDATOR_UNAVAILABLE`, replay still works,
  old S1 record accepted; S2 → STALE; none → STALE; foreign tenant NOT_FOUND; no service
  named in refusals). Mutation check: restoring the old predicate turns both PG tests red.

### Validation invocation identity — implemented, executed ✓
- `sculpin-validation-invocation/v1` (`crates/ledger-validation-protocol/src/invocation.rs`):
  SHA-256 over header + tenant, principal type byte, principal id, delegation, graph,
  `validate`, key, request-v2 digest; never in any content identity, never stored. Sent as
  `invocation_id` and `Idempotency-Key` (one value). Vectors `invocation-v1-*` (6 positive
  incl. human / 256-byte / non-ASCII keys, 8 refused inputs) from the Python reference,
  checked by Rust. Sculpin contract: at-least-once delivery, exactly-once logical validation
  within Sculpin's declared retention; single flight, body binding, caller scoping,
  Sculpin-side failure definition, same-key-retry semantics
  (`docs/design/sculpin-validation-service.md`).
- Tests (`pg_validation_api`, deduplicating fake with gate, live KB revision, crash hook):
  `concurrent_same_key_validations_are_one_logical_invocation` (both requests reach the
  validator with one id, one logical validation, the environment at start wins over one that
  moved mid-flight, one idempotency row, one record, both responses name it; control: another
  key sees the moved environment); `a_retry_after_a_lost_answer_reuses_the_invocation`;
  `a_retry_after_a_crash_between_answer_and_record_reuses_the_invocation` (store failpoints
  after context, after record, before commit); `same_key_with_other_hints_is_another_invocation_and_conflicts`
  (`IDEMPOTENCY_CONFLICT`, one record); `validator_http` asserts header = body id. Mutation
  check: deriving the id from the correlation id turns the concurrent and retry tests red.
  **Scope:** the fake implements the contract; Sculpin's deduplication is pending external.

### Schema verifier and verify — implemented, executed ✓
- NOT NULL of every migration-declared column verified at start-up/readiness (composite FKs
  are `MATCH SIMPLE`); exact inventory test; `NOT VALID` not-null (PG18) not counted; drift
  test on five key columns.
- Logically restored CHECKs (ADR-0017 amendment): `backup-restore.sh` on `b54906f` showed the
  server refusing a `pg_dump` restore (strict deparse from P1.5's `7842f14`, also on main);
  the verifier accepts exactly the flattened re-parse of the 7 affected CHECKs; tested by
  recreating every CHECK from its deparsed text. The released P1.5 binary keeps the defect.
- `ledger-admin verify`: every summary / virtual-context row compared with the decoded bytes
  in one REPEATABLE READ snapshot (the first version raced a live writer — reproduced by
  parallel suites, fixed; pg_validation then 10/10 repeated runs); `insert_record` collision
  compares graph and tenant.

### 0009 → 0010 upgrade from the P1.5 release — executed ✓
`scripts/upgrade-p2.sh` (new): previous `f81be37` (schema 9) built from git; populated
through its API — 3 graphs / 2 tenants, 4 refs, 21 accepted commits, 3 rejected and 3 pending
proposals, 24 decisions, 21 ref events, 21 outbox rows, 51 idempotency keys, 21 states; stop →
`pg_dump -Fc` (restored twice: rows identical, DDL/ownership identical except the 3
re-parsed branch-bound CHECKs, previous `verify` VERIFY OK) → owner `migrate --runtime-role`
(re-run idempotent) → Phase-2 server as runtime. Proven: all 11 pre-upgrade tables
byte-identical after migrate and after replay (commit/patch ids, parents, objects, refs,
ref events, decisions, outbox, idempotency, proposals, graphs); 21 states identical; 51 old
keys replay identically, refs unmoved, no validator call; VERIFY OK; runtime-only connection,
18 runtime write probes denied; unvalidated accept `409 VALIDATION_REQUIRED` (ref unchanged),
validation (header = `invocation_id`) and validated acceptance on upgraded graphs; P1.5
server refuses 0010 (`ahead`), Phase-2 server and verify refuse 0009 (`behind`); 0010 guard
refuses a pre-existing validation id and leaves 0009 untouched; clean 0010 install and
upgraded schema identical (809 DDL/grant lines, 70 owned objects). Final run on `32dc825`:
see the table below. NOTE recorded: the P1.5 server refuses a logically restored copy.

### Executed gates on the closure candidate
| Gate | Head | Result |
|---|---|---|
| `./scripts/check-fast.sh` | `32dc825` | exit 0 (fmt, clippy `-D warnings`, workspace tests, doc links, architecture, Python references: 18 commit-v2, 12 request, 42 validation incl. invocation, 3 state) |
| `./scripts/check-supply-chain.sh` | `a71fd09` (no dependency change since) | exit 0 (advisories/bans/licenses/sources ok, SBOM 213 components) |
| PostgreSQL 15.19 — all 10 suites (`pg_validation` 9, `pg_verify` 2, `pg_workflow` 14, `pg_least_privilege` 17, `pg_cas_race` 1, `pg_immutable_store` 9, `pg_graphs_migration` 7, `pg_fs_migration` 8, `pg_validation_api` 17, `pg_api` 13) | `b54906f` | all passed; `pg_least_privilege` 18 and `pg_verify` 2 again on `32dc825` |
| PostgreSQL 17.11 — same 10 suites | `b54906f` | all passed; `pg_least_privilege` 18 and `pg_verify` 2 again on `32dc825` |
| `validator_http` (5) | `b54906f` | passed (in `check-fast`) |
| `./scripts/test-integration.sh` (compose PG 17.2, distroless image, all PG suites, container scenario, verify) | `b54906f` | exit 0, `INTEGRATION OK` |
| `./scripts/backup-restore.sh` | `b54906f` / fix | **failed** on `b54906f` (dump restore refused, above) → `BACKUP RESTORE OK` with the fix |
| `scripts/upgrade-p2.sh` | fix | `UPGRADE-P2 OK` |
| fuzz `validation_decode` (now also the validator JSON response path), sanitizer none, 120 s | `07420ae` | 15.8 M execs, cov 2950, no crash |

### Reviews
- Round 4 (Opus, read-only, `1724216..a71fd09`): migration-0010 security review (no P0/P1;
  P2 NOT NULL verification fixed; P2 insert-after-seal on detail tables accepted as the
  ADR-0016 trusted-writer class, recorded; P3s fixed or recorded); invariant, storage/
  concurrency, security, semantic-integration and test reviews — no P0/P1 anywhere; fixed:
  trust-before-verdict, verify snapshot race, neutral STALE text, empty/oversized token file,
  trust seeding, NOT VALID guard, doc misplacement, crash/lost-answer tests, bounded race
  tests, exact inventory, table-driven verify tampering, more vectors, fuzz binding,
  upgrade-harness assertions, contract gaps (retention, single flight, failure, same-key
  retry), security notes (service id is an operator assertion; invocation id unkeyed).
  Recorded as risk decisions: signed validator responses / keyed invocation id (ADR needed).
- Codex (read-only `codex exec`, focused prompt) on `b54906f`: **no P0/P1**; one P2 (upgrade
  harness force-removed a fixed worktree path) fixed in `32dc825`.

## Remaining before merge / release
1. Hosted CI (ci-fast, ci-integration, ci-security, ci-fuzz) on the pushed final head.
2. Sculpin: implement the service contract including invocation deduplication; a live
   end-to-end run — **pending external** (no Sculpin endpoint exists; the fake proves the
   ledger's coordination only, never SHACL or reasoning correctness).
3. 900 s `validation_decode` under AddressSanitizer: **not executable here** (the host's ASan
   runtime crashes at start-up, Plan 0005); runs on hosted `ci-fuzz` (`address` matrix, weekly
   900 s schedule or `workflow_dispatch`).

## P1.5 external blockers (unchanged, still pending)
Live Entra ID issuer smoke test; deployment PITR/WAL evidence; writer fencing / restore
operational evidence. Phase 2 does not change them; the service is **not production-qualified**.

## Sub-agent decomposition (§42)
Main session owns protocol/canonicalization (crate, encodings, goldens, ADRs), the migration
and the acceptance transaction. Bounded reviewers per slice as above.
