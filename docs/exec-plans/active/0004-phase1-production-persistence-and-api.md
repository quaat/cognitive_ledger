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
- `graphs` migration tests (ADR-0010): same `graph_id` under two tenants fails (global
  uniqueness); `UPDATE graphs SET tenant_id` fails while other columns update; two graphs
  per KB succeed; the 0001 upgrade backfills a `status='bootstrap'` row for `'default'`
  and clean install equals upgrade.
- v1 graph-binding policy (ADR-0010): a `V1Binding::Reject` store refuses v1 writes and
  still reads them; a `BindTo` store indexes them under the bootstrap graph; one commit
  id can never be indexed under two graphs.
- `commit_index` is verified: re-derivation from bytes matches every row (ADR-0012).
- typed ref target: an existing non-commit object (patch or blob) can never become HEAD.
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

## Current status
P1.1 (persistent protocol) implemented and gated green on 2026-09-26:
- `ledger-core` gains `GraphId`, `TenantId`, `PrincipalId`, `PrincipalType`, `Actor`,
  `AuthenticatedPrincipal` (identity caps frozen: 128-byte graph id, 512-byte tokens,
  4096-byte message, 64 evidence refs), `LedgerTimestamp` (RFC 3339 → UTC microsecond
  canonical form), `CommitV2` with a strict encoder/decoder, and `AnyCommit` dual read;
  v1 `Commit` and its vectors are untouched. `time` (already a workspace dependency) is
  added to `ledger-core`; the architecture check still passes.
- Golden vectors `fixtures/golden/commits/v2-*` (five positive, thirteen negative) generated
  by the independent Python reference encoder `scripts/golden/commit_v2_reference.py`;
  `crates/ledger-core/tests/golden_v2.rs` verifies them from the Rust side.
- `docs/design/canonicalization.md` gains the v2 section; ADR-0010 pins the `graph_id`
  representation.
P0-bridging task (2026-09-26, six items from the owner's P1.1 review), all landed:
1. ADR-0010: `graph_id` globally unique, `tenant_id` binding immutable, and the four
   `graphs` migration tests specified (executable when the `graphs` migration lands).
2. ADR-0010: production v1 graph-binding policy defined and made executable as
   `V1Binding::{Reject, BindTo}` on `PostgresImmutableStore`.
3. `ImmutableStore::put_commit/get_commit` are version-neutral over `AnyCommit`; the
   legacy v1-only `ObjectStore`/`CommitStore` traits were removed (unused since
   ADR-0012). `FileStore` and `Ledger` hold v1 and v2 commits side by side.
4. `Ledger::advance_ref` uses the typed `get_commit` check again (the `exists` probe
   from the ADR-0012 refactor let any object become HEAD); regression tests cover a
   patch and an arbitrary blob masquerading as a commit.
5. ADR-0012 specifies `immutable_objects` + verified `commit_index`/`commit_parents` as
   the P1.2 foundation P1.3's transactional graph/lineage predicates build on.
6. P1.1 gate re-run green (below); **P1.2 begun**: migrations 0002/0003,
   `PostgresImmutableStore` (content-addressed publish, typed parent/patch checks and
   index rows in one transaction, digest-verified reads, `verify_commit_index`),
   `Ledger::with_stores`, server `LEDGER_IMMUTABLE_BACKEND=postgres`, compose harness on
   the shared backend, and the ignored PostgreSQL suite `tests/pg_immutable_store.rs`.
**P1.2 (shared persistence) complete, 2026-09-26.** Closing work, all with real-PostgreSQL
evidence:
- Integrity corrections: truthful publication (`publish_object` reads back the
  authoritative row; different bytes are `ObjectCollision`; a damaged seeded row makes a
  later publication fail hard); `put_commit` verifies the authoritative index row and
  ordered parents after its conflict-free inserts, so concurrent incompatible v1 bindings
  resolve to exactly one graph with the loser getting `GraphBindingConflict` (5 barrier
  rounds plus a deterministic blocked-path test via `pg_stat_activity`, commit and
  rollback variants); same-graph ancestry (`CrossGraphParent`) for v2→v2, merge, and
  v2→v1 shapes; `put_content` refuses commit-envelope bytes; corrupt or unknown-version
  envelopes are errors, not "absent"; a patch may not be an indexed commit; v1 history
  binds only to `bootstrap`/`importing` graphs; READ COMMITTED pinned per transaction.
- Server: with a database URL the shared PostgreSQL backend is the default;
  `LEDGER_IMMUTABLE_BACKEND=filesystem` is an explicit single-host opt-in with a loud
  warning; selection is a pure, unit-tested function; startup runs `Ledger::verify_head`
  and refuses a HEAD that does not resolve in the configured store. Compose binds ports to
  loopback.
- Migration 0004 (`graphs`, ADR-0010): globally unique `graph_id`, immutable
  `graph_id`/`tenant_id` (trigger covering `ON CONFLICT DO UPDATE`), many graphs per KB,
  `refs`/`commit_index` FKs with RESTRICT, bootstrap `default` row on clean install and
  upgrade, fail-closed guard naming unowned graphs from both `refs` and `commit_index`
  with a tested re-run after the operator remedy. Migration 0005: write-once triggers on
  the immutable tables and immutable ref identity. Upgrade-from-0001 and clean install
  converge on a literal schema snapshot (columns, constraints, indexes, triggers,
  function bodies).
- Filesystem → PostgreSQL migration (`FsToPgMigration`, `ledger-admin migrate-fs-to-pg`):
  read-only source, HEAD resolved from the filesystem ref or the existing shared `refs`
  row (both topologies; wrong or partial source is `MissingTarget`), graph-membership
  check before publication, topological import, scoped index verification, both-backend
  state comparison, ref installed only afterwards; the v1 binding must name the target
  ref's graph and a ref is never installed onto another graph's history.
- Independent reviews (storage/concurrency, invariant, test, security; Opus) — every
  confirmed P0/P1 finding fixed before closure: ref-to-graph binding gap in the migration
  (all four), shared-refs topology HEAD resolution and wrong-source success
  (storage), argument echo of a misplaced database URL and argv binding to any graph
  (security/storage), source opened read-write (security), silent skips in
  `list_objects` (security/invariant), order-dependent bootstrap-delete test, no negative
  `verify_commit_index` test, race test not proving the blocked path, unfaithful
  interruption simulation, guard's `commit_index` branch untested (test). Accepted with
  documentation instead of code: filesystem/PostgreSQL acceptance-rule difference
  (filesystem is not a qualification target), v1 `graph_id` not byte-derivable (policy),
  memory O(source) in the admin tool, no catch-up with a moved destination ref, role
  separation for trigger ownership, error-body graph ids (P1.4) — all in `tech-debt.md`
  or ADR-0012.
P1.3 (atomic workflow persistence) is next and is **not** started here.

### P1.2 production-hardening pass (prerequisite for P1.3; scope fixed 2026-09-26)
1. A commit's referenced patch must be a valid canonical RDF patch (not merely existing,
   non-commit content) in `FileStore::put_commit`, `PostgresImmutableStore::put_commit`
   and `verify_commits`; typed `InvalidPatch` error; no commit object/index row on refusal.
2. `FsToPgMigration` fails closed on moving refs: source HEAD re-read before cutover
   (`MigrationSourceMoved`), an existing destination HEAD re-read (`HeadChanged`), absent
   destination installed by CAS with a concurrently installed identical HEAD as idempotent
   success; after success the destination HEAD equals the verified HEAD. Deterministic
   hook-based tests, no sleeps. Source writers must be quiesced; no online catch-up.
3. Backend selection fails closed on `LEDGER_IMMUTABLE_BACKEND=postgres` without a
   database URL; bare-server default listen address becomes `127.0.0.1:8080` (containers
   set `0.0.0.0:8080` explicitly); every combination unit-tested.
4. Graceful shutdown on SIGINT and, on Unix, SIGTERM; no platform-specific compile failure.

Status: **done (2026-09-26)**. Independent reviews (storage/concurrency, invariant, test,
security; Opus) found no P0; every confirmed finding was fixed before the gate: a stored
patch whose bytes no longer hash to its id was labelled `InvalidPatch` (a 400) on the
PostgreSQL path instead of corruption — now `CorruptObject` on both backends, with
`InvalidPatch` reserved for hash-correct non-canonical bytes and its reason bounded (no
stored RDF text is echoed; the API reports it generically); the SIGTERM test only proved
registration — shutdown is now a tested selector (`shutdown_on`) with a bounded 30 s drain;
verification's non-canonical-patch branch, a corrupted-patch publication on each backend, a
ref appearing during a no-HEAD run, and a ref moved after installation had no tests — all
added (the cutover hook now has two phases); an empty or non-Unicode database URL selected
filesystem mode silently — now a startup error; `verify_head` also checks the HEAD's patch.
Accepted with documentation: the tool detects rather than prevents source movement
(quiescence is an operational requirement), history written before the patch-validity rule
fails closed at migration/startup and needs repair, `.dockerignore` tightened.

### P1.2 hardening evidence (2026-09-26)
`cargo fmt --check`, clippy `-D warnings`, `cargo test --workspace` (ledger-server now 7
tests incl. `shutdown_resolves_on_either_signal_and_names_it` and the six backend-selection
combinations; ledger-store 9 lib), `check-doc-links` 44 files, `check-architecture`,
golden 18/18 all exit 0. Real PostgreSQL (fresh volume): `pg_cas_race` 1,
`pg_immutable_store` 9 (incl. `commit_patch_must_be_a_canonical_rdf_patch`: blob,
non-canonical, malformed, healthy, child-with-parent, corrupted-stored-patch),
`pg_graphs_migration` 6 (tamper test now covers hash-mismatch and non-canonical patch
branches; upgrade-guard retry reconnects because a failed sqlx run keeps its advisory lock
on the pooled connection), `pg_fs_migration` 9 (six cutover scenarios) — 25 passed. Full
Docker integration `./scripts/test-integration.sh` exit 0: image build, all 25 database
tests, HTTP commit + ledger-container restart, "shared backend confirmed: 2 commits in
PostgreSQL, 2 indexed under 'default', 0 node-local object files".

## P1.3 design — atomic workflow persistence (ADR-0013; fixed before implementation)

**Schema (migration 0006, additive; 0001–0005 untouched).**
- `refs`: `version BIGINT NOT NULL DEFAULT 1` (monotonic; a trigger requires
  `NEW.version = OLD.version + 1` on every head change), `protected BOOLEAN NOT NULL
  DEFAULT true` (strict effective-delta policy), composite FK `(graph_id, head) →
  commit_index(graph_id, id)`. The migration first raises an actionable error listing any
  existing ref whose head is not an indexed commit of its graph; nothing is repaired.
- `proposals` (append-only): candidate commit (FK to `commit_index(graph_id,id)`,
  unique), requested patch id (FK `immutable_objects`), effective patch id, graph, branch,
  expected head, principal fields, tenant, created_at. Records what the caller asked for
  versus what actually changed (ADR-0008); raw intent never enters commit identity.
- `ref_events` (append-only): graph, branch, old/new head, old/new version, operation
  (`genesis` | `advance`), principal fields, tenant, reason, recorded_at; unique
  `(graph, branch, new_version)`; FK new head → `commit_index(graph_id,id)`.
- `decisions` (append-only): candidate, optional proposal, graph, branch, decision
  (`accepted` | `rejected` | `superseded`), principal fields, tenant, reason,
  `validation_ids TEXT[]` (empty in P1.3; Phase 2 fills it — no schema replacement),
  optional ref_event id (accepted only), decided_at. At most one terminal decision per
  proposal. No dummy validation record is ever written.
- `projection_outbox`: graph, branch, commit, ref_version, event kind (`ref_advanced`),
  ref_event id, created_at, delivery metadata nullable; unique `(graph, branch,
  ref_version)`; FK to the ref event. P1.3 writes rows only; no consumer.
- `idempotency`: PK `(tenant, principal, graph, operation, key)`, request digest,
  result columns (candidate / ref version / decision id / error code), created_at,
  completed_at. A row is inserted inside the workflow transaction and therefore becomes
  visible only as a completed result: a loser blocked on the unique index reads the
  winner's completed row after commit (same digest → replay; different digest →
  `IDEMPOTENCY_CONFLICT`); a rolled-back winner leaves no row, so the loser proceeds.
- Write-once triggers on `proposals`, `ref_events`, `decisions`; `projection_outbox`
  identity columns immutable (delivery columns mutable for Phase 3); `idempotency`
  rows immutable.

**Repository (`ledger-store`, PostgreSQL only).** A composition root
`PostgresLedgerStore { pool, immutable, graphs, workflows }` shares one pool.
`WorkflowRepository` exposes `prepare`, `accept`, `reject`, `mark_superseded`, each ONE
transaction pinned to READ COMMITTED:
- `prepare`: idempotency reservation → graph must be `active` → ref read (`expected_head`
  must equal the current head, `HEAD_CHANGED` otherwise) → base state reconstructed →
  `effective_delta(base, requested, policy)` (strict on protected refs) → requested patch
  and effective patch published as content → candidate v2 commit published through the
  shared in-transaction publication helper (refactored out of
  `PostgresImmutableStore::put_commit`, not duplicated) → proposal row → idempotency
  result → commit. Candidate identity and idempotency commit together, so a lost response
  replays the exact original `CommitId`.
- `accept`: idempotency reservation → graph `active` → `SELECT … FOR UPDATE` on the ref row
  (absent row = genesis path) → candidate must be indexed under the requested graph →
  lineage: genesis (`expected_head = None`, ref absent, `parent_count = 0`) or advance
  (ref head = `expected_head`, `parents[0] = expected_head`), else `LINEAGE_MISMATCH` /
  `HEAD_CHANGED` → ref update with `version + 1` → ref_event → accepted decision → outbox
  → idempotency result → commit. Any failure rolls back every mutable record; the
  candidate stays as an unattached proposal.
- `reject`: idempotency + rejected decision, atomically; no ref move, no outbox.
- `mark_superseded`: explicit call for a proposal whose expected head is no longer the
  ref head; no automatic supersession in P1.3.
- Fault injection: `WorkflowRepository::with_failpoint(FailPoint)` aborts the transaction
  at after-reservation, after-lineage, after-ref-update, after-ref-event, after-decision,
  after-outbox, before-commit. Lost response = commit, drop the result, retry.

**Effective delta (`ledger-rdf`, pure).** `effective_delta(base, requested, policy) →
Result<Patch, DeltaError>`: strict — delete-absent is `BASE_MISMATCH`, add-present is
dropped, empty result is `NO_EFFECTIVE_CHANGE`; permissive — no-ops dropped, empty result
still `NO_EFFECTIVE_CHANGE`. Deterministic seeded property tests: idempotence,
add-present collapse, delete-absent strict rejection, mixed reduction, empty rejection,
identical identity from a supplied state versus full reconstruction (checkpoints repeat
this in Phase 6).

**Policies.** Normal acceptance requires `graphs.status = 'active'`; bootstrap/import
graphs move only through the administrative paths. Raw `PgRefStore` CAS remains for
filesystem/dev/bootstrap/admin/tests and now bumps `version`; production accepted
transitions go through `WorkflowRepository`, and the bootstrap v1 write path is labelled
as such until the authenticated API (P1.4) routes `prepare`/`accept`. P1.3 atomicity is
verified with an explicit no-validation policy; that is **not** production protected
semantic acceptance (Phase 2).

**Not in P1.3.** Merge/second-parent semantics, reset, projector, HTTP auth, branches,
role qualification (documented direction only).

### P1.3 status — complete (2026-09-26)
Delivered exactly as designed above, with these decisions made during implementation:
- Idempotency is serialized by a per-scope transaction advisory lock taken before the
  stored result is read, so identical concurrent requests (genesis included) replay
  deterministically instead of racing to `HEAD_CHANGED`/`LINEAGE_MISMATCH`.
- The repository verifies graph ownership against the principal's tenant (foreign or
  missing graph → `UNKNOWN_GRAPH`, nothing leaks); accept/reject require a proposal bound
  to the requested graph, branch and expected head, so only `prepare`d candidates (and
  therefore effective-delta patches) are ever decided; a decided candidate is refused
  (`LINEAGE_MISMATCH`) and the schema enforces one terminal decision per candidate.
- Migration 0006 consequences: PostgreSQL refs can only target indexed commits of their
  graph, so the legacy "filesystem objects + PostgreSQL ref" topology is refused by the
  server; `ledger-admin migrate-fs-to-pg` cuts such databases over in schema order
  (content schema → import → workflow schema). Raw `PgRefStore` CAS is confined to
  `bootstrap`/`importing` graphs. `refs.protected` is immutable until Phase 4.
- Branch names, idempotency keys and reasons are bounded up front and by CHECK constraints.
- No validation record is written; acceptance runs under an explicit
  `ValidationPolicy::NoValidation`. P1.3 atomicity verified ≠ production protected semantic
  acceptance (Phase 2).
Executed: `cargo fmt --check`, clippy `-D warnings`, `cargo test --workspace` (ledger-rdf 12
incl. the seeded effective-delta property test; ledger-server 7), doc links 44, architecture,
golden 18/18 with `git diff -- crates/ledger-core fixtures` showing only error variants. Real
PostgreSQL 17.2 (fresh volume, 37 tests): `pg_cas_race` 1, `pg_immutable_store` 9,
`pg_graphs_migration` 6 (upgrade converges through 0006, `version = 1` backfilled),
`pg_fs_migration` 8 (incl. the legacy shared-ref topology cutover in schema order and the
no-op partial migration on an upgraded database), `pg_workflow` 13: genesis+advance counts
(1 event, 1 decision, 1 outbox row each), strict effective delta with no persistence on
refusal, materialized-vs-reconstructed identity, prepare idempotency incl. a two-replica
race yielding one candidate and one indexed commit, acceptance idempotency incl. a 6-writer
same-HEAD race (one accepted) and 4-way same-key acceptance (one effect, identical ids,
late replay), concurrent genesis (same key → one effect and replays; different candidates →
one winner, `HEAD_CHANGED` naming it), lineage matrix with the firing rule asserted,
graph lifecycle incl. tenant-scoped bootstrap refusal, fault injection at 7 accept + 4
prepare + 3 reject points plus a genesis failure after the ref insert (no mutable effect,
retry succeeds once, lost response replays identical ids), rejection/supersession incl.
decided-candidate refusals, concurrent accept-vs-reject (exactly one decision), foreign
tenant/bounds/raw-CAS refusals, and database-level invariants (composite FKs, version
trigger on insert and update, write-once audit tables, outbox identity, idempotency
uniqueness). Full `./scripts/test-integration.sh` exit 0 with all 37 database tests and the
HTTP restart scenario ("shared backend confirmed"). Differential: seam-only, live Fluree
deferred by policy (not passed). Not executed: the 1,000-writer race (P1.5), crash/kill
fault injection (P1.5), database role split (P1.5).

Independent reviews (Opus; storage/concurrency, invariant, test, security, then a second
storage + invariant round on the fixes). Confirmed findings, all fixed before closure:
identical concurrent genesis returned `HEAD_CHANGED` and same-key retries could return
`LINEAGE_MISMATCH` instead of replaying (advisory lock per idempotency scope); one terminal
decision per candidate only enforced in code (unique index + typed mapping of the unique
violation); proposals not bound to their ref and commits without a proposal acceptable
(binding + proposal required); no tenant ownership check and `mark_superseded` unscoped
(tenant check with `UNKNOWN_GRAPH`, graph-scoped supersession with active-graph check);
`migrate_up_to` would fail on an upgraded database (`ignore_missing`); unbounded branch,
key and reason inputs (bounds + CHECKs); raw CAS movable on active graphs and racy status
check (refused, share-locked); prepare could starve the pool by reconstructing through the
pool while holding its transaction (reconstruction on the transaction connection);
reject vs supersede deadlock through `FOR UPDATE` vs FK key-share (`FOR NO KEY UPDATE`);
test suites that could pass for the wrong reason (rule messages asserted, commit-index
counts, shared request objects, full identity equality on replays). Accepted with
documentation: the repository trusts the API layer's request digest (P1.4 defines the
canonical request); an imported graph's audit trail starts at its first workflow advance;
`mark_superseded` has no idempotency key (explicit operator action); graph lifecycle
transitions do not exist yet (status share-locked during workflows so a future transition
cannot interleave); database role split remains P1.5 and the production security gate is
not passed.

## P1.4 design — authenticated HTTP and security boundary (scope fixed 2026-09-26)

**Scope.**
- Migration 0007 (additive): idempotency scoped by the *complete* actor (`principal_id`,
  `principal_type`, `on_behalf_of`, NULL treated as a canonical absence via `UNIQUE NULLS
  NOT DISTINCT`), backfilled deterministically from the actor of the request each row
  recorded (proposal for prepares, decision for accepts/rejects) and failing closed on any
  row that cannot be bound; bounded optional `correlation_id` on
  `proposals`, `ref_events`, `decisions`; `graphs UNIQUE (graph_id, tenant_id)` and
  composite `(graph_id, tenant_id)` FKs from `proposals`, `ref_events`, `decisions`,
  `idempotency` so PostgreSQL itself proves a row's graph belongs to its tenant.
- `RequestScope` carries the complete actor and a correlation id; the advisory-lock key
  and every idempotency lookup include `principal_type` and `on_behalf_of`.
- `ReconstructionLimits` (depth, quads, bytes) in `ledger-store`, enforced by the
  workflow's base reconstruction and by public state reads; `RESOURCE_LIMIT` error.
- `RequestContext { principal, capabilities, correlation_id }` produced only by verified
  authentication: OIDC bearer tokens (signature via JWKS with rotation, issuer, audience,
  exp/nbf; unknown key fails closed) or an HS256 development authenticator that is refused
  on non-loopback binding unless a conspicuously named override is set. Principal type,
  tenant and roles come only from verified claims and explicit configuration.
- Capabilities `read | propose | review | admin` mapped from verified roles in one policy
  component; a foreign-tenant graph is indistinguishable from a missing one.
- Graph-scoped API: prepare / accept / reject proposals, read ref, read bounded state;
  branch names are body fields (they may contain `/`). `POST /v1/commits` and the raw
  v1 write path are removed from the public surface; the shared server runs
  `V1Binding::Reject` and public writes create v2 only.
- Canonical request identity `sculpin-ledger-request/v1` (typed, length-prefixed,
  evidence as a sorted set; excludes key, correlation id, recorded_at, JSON order) with
  repository-owned golden vectors and an independent Python reference encoder; the API
  computes the digest, never the client.
- `Idempotency-Key` required on mutations; unknown JSON fields rejected; retries return
  identical durable results.
- Unvalidated acceptance fails closed (`VALIDATION_REQUIRED`) unless
  `LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only` is set
  (startup warning); no validation record is ever fabricated.
- Configurable limits below the untrusted boundary: body bytes, operation count, term
  length, metadata bytes, reconstruction depth/quads/bytes, request duration, concurrent
  expensive operations.
- Stable, redacted error envelope `{code, message, correlation_id}`; `/health` liveness,
  `/ready` dependency check; checked-in OpenAPI enforced against the router in tests.
- `ledger-admin graph create` for operator provisioning; the Docker integration provisions
  a graph, authenticates, prepares and accepts a v2 candidate under the explicit CI
  no-validation switch, restarts, and reads the accepted state back through the
  authenticated API.

**Non-goals.** Semantic validation (Phase 2), projection (Phase 3), branches/policies
(Phase 4), merge (Phase 5), checkpoints, graph lifecycle transitions, a public graph
administration API, role split / migrations off the runtime path (P1.5).

**Migration impact.** 0007 is additive; 0001–0006 untouched. Upgrade requires every
existing idempotency row to resolve its actor through its proposal and every audit row's
`(graph_id, tenant_id)` to match `graphs`; mismatches fail the migration with an
actionable error.

**Security assumptions.** Tokens are validated cryptographically; tenant, principal, type
and roles are trusted only from verified claims plus explicit configuration; the
development authenticator is not production authentication; the database role split is
still P1.5, so the service is **not** production-qualified after P1.4.

**Acceptance evidence.** Real PostgreSQL: 0007 clean-install/upgrade convergence and
mismatch refusal; complete-actor idempotency (replay / independent namespaces / conflict);
canonical digest goldens; authentication (bad signature, issuer, audience, expiry, unknown
kid), authorization (capability matrix), cross-tenant read/write indistinguishability,
safe errors, limits, lost-response HTTP replay; Docker integration with restart through the
v2 workflow API and proof that no v1 commit was created.

### P1.4 status — complete (2026-09-26)

Implemented (all in this change; protocol files untouched):
- Migration `0007_actor_scope_and_tenant_integrity.sql` (additive; 0001–0006 unchanged):
  complete-actor idempotency (`principal_type`, `on_behalf_of`, `UNIQUE NULLS NOT
  DISTINCT` with equality columns leading), deterministic backfill (prepared rows from
  their proposal's actor, accepted/rejected rows from their decision's actor) with
  fail-closed guards, surrogate `idempotency_id`,
  bounded `correlation_id` on `proposals`/`ref_events`/`decisions`, `graphs UNIQUE
  (graph_id, tenant_id)` and composite tenant FKs from all four workflow tables.
- `ledger-store`: `RequestScope.correlation_id`; advisory-lock key and every idempotency
  lookup include the complete actor; `ReconstructionLimits` shared by the workflow's base
  reconstruction and public reads (`PostgresLedgerStore::with_limits`, applied by
  `AppState::new`); `prepare` refuses a candidate whose depth or resulting state would
  exceed the limits; `ValidationPolicy::Required` enforced in `accept` after the replay
  lookup; `LedgerError::{ResourceLimit, ValidationRequired, DependencyUnavailable}` (pool
  timeout / connection failures → 503, not 500); reconstruction keeps patch ids only;
  `PostgresLedgerStore::{commit_graph, ref_head, ready}` (readiness checks the schema
  level `REQUIRED_SCHEMA_VERSION = 7`).
- `ledger-api` (rewritten): `auth` (`Authenticator` trait; `OidcAuthenticator` with JWKS
  single-flight refresh, one-hour maximum key age, rate-limited retries, `use=sig` filter,
  alg pinned per key family, required `exp`/`iss`/`aud`, no-redirect size-capped fetch,
  redacted errors; `DevHs256Authenticator` not production grade; `ClaimsPolicy` →
  `AuthenticatedPrincipal` + `Capabilities`, refusing a type claim that disagrees with
  configured client ids), `request_identity` (`sculpin-ledger-request/v1`, ADR-0015, 6
  golden vectors + `scripts/golden/request_v1_reference.py`, exercised through the
  handlers' `canonical_prepare/accept/reject`),
  `RequestContext` extractor, correlation middleware, `ApiLimits`, envelope-preserving
  `ValidJson`, stable error envelope with redaction, graph-scoped routes only
  (`docs/api/openapi.json` enforced by a unit test), filesystem read-only router.
- `ledger-server`: `LEDGER_AUTH_MODE=oidc|dev-hs256` (no unauthenticated/header mode),
  unvalidated acceptance refused together with production authentication,
  non-loopback refusal without production auth unless
  `LEDGER_ALLOW_INSECURE_NON_LOOPBACK=allow-insecure-non-loopback-development-only`,
  `LEDGER_UNVALIDATED_ACCEPTANCE` exact-value switch with startup warning, `LEDGER_LIMIT_*`,
  shared mode `V1Binding::Reject`, no `Ledger::commit`/`PgRefStore` path; filesystem mode
  read-only and loopback-only. `ledger-admin graph create`. Dockerfile ships `ledger-admin`;
  compose carries the conspicuously named development switches; `.dockerignore` admits the
  API contract.
- Docs: security, storage boundaries (runtime table, API, provisioning), ADR-0010/0011/0013
  implementation notes, migrations README (0007), tech-debt (stale "status read without a
  lock" entry removed — the graph row is share-locked in the acceptance transaction; P1.4
  entries closed; new entries for OIDC live-issuer testing, statement timeouts, digest
  producer), README, ARCHITECTURE, docs index, test strategy, this plan, and Plan 0005
  (P1.5) created.

Evidence (2026-09-26, final code after the review fixes; real PostgreSQL 17.2 via compose,
`LEDGER_TEST_DATABASE_URL`):
- `./scripts/check-fast.sh` exit 0: fmt check, doc links (46 files), architecture check,
  both Python golden checks (18 commit v2 vectors, 6 request vectors), clippy `-D
  warnings`, `cargo test --workspace`. Unit highlights: `ledger-api` 14 lib tests (5 OIDC
  tests against a local JWKS server: RSA+EC acceptance and identity mapping, unknown kid
  then rotation, refresh rate limit and zero max-age withdrawal, alg confusion / missing kid
  / missing iss-aud-exp / forged signature, unreachable key source → `KeySourceUnavailable`
  without URL leak; claims-policy mapping and conflict refusal; dev HS256 secret length;
  every OpenAPI route served and unknown routes/methods enveloped; database outage →
  redacted 503 on `/ready` and on an authenticated read with liveness unaffected; request
  timeout → 503 `RESOURCE_LIMIT`; OpenAPI ≡ routes and error codes; correlation bounds;
  request-identity properties) + `request_goldens` 3 (all 6 vectors byte- and
  digest-identical to the Python reference through the handlers' `canonical_*` builders;
  reordered ≡ advance; empty `source_system` ≡ absent, empty reason refused);
  `ledger-server` 14 (backend selection, non-loopback refusal incl. wrong switch value and
  unresolvable bind, filesystem mode loopback-only, exact unvalidated-acceptance value and
  its refusal with production auth, authenticator selection with no unauthenticated/header
  mode and https-only JWKS, strict role-map parsing, shutdown).
- Store suites: `pg_cas_race` 1, `pg_immutable_store` 9, `pg_graphs_migration` 7 (incl.
  `migration_0007_backfills_actor_scope_and_refuses_unbound_or_mismatched_rows`: proposer
  and reviewer rows bound to their own actors, results unchanged, write-once trigger back
  in force, repository replay for the bound actor and a fresh proposal for a different
  delegation, populated-upgrade schema ≡ clean install incl. trigger enablement, unbound /
  disagreeing-actor / wrong-tenant refusals), `pg_fs_migration` 8, `pg_workflow` 14 (incl.
  `idempotency_is_scoped_by_the_complete_actor_and_correlation_is_recorded`: replay;
  independent namespaces by `principal_type` and by `on_behalf_of`; conflict; NULL
  canonical duplicate refused by the DB; correlation retained on proposal/event/decision;
  cross-tenant audit insert refused by FK) — 39 passed, 0 failed.
- API suite `crates/ledger-api/tests/pg_api.rs`: 10 passed, 0 failed — authentication
  (no token, forged signature, expired, wrong audience/issuer, nbf in future, missing
  tenant/principal/type/exp/aud, unrecognised role, `alg:none` with otherwise valid claims,
  RS256 header on the HS256 authenticator, identity headers ignored, non-Bearer schemes),
  table-driven capability matrix (5 routes × read/propose/review/admin, exact FORBIDDEN
  set) plus the review flow, persisted identity (proposal/decision/ref-event/idempotency
  rows carry the token's tenant, prefixed principal, type, delegation and the client
  correlation id; delegation is its own idempotency namespace), foreign tenant ≡
  nonexistent (identical envelope modulo correlation id for reads and for prepare/accept/
  reject; commit of another graph via own or foreign graph path; malformed/unknown commit;
  row counts unchanged), lost-response replay for prepare and accept with raw JSON in a
  different key order, reversed operations, reordered/duplicated evidence and the same
  instant in another offset (identical durable ids; conflict on a different body;
  independent actor namespace; ref advanced once with exactly one outbox row; stale accept
  → `HEAD_CHANGED`; competing candidate refused without moving the ref), strict shape
  (missing/empty/oversized/control-character key, eight client-supplied identity or
  server-only fields, blank node, malformed quad, bad op, empty patch, 65 evidence refs,
  empty activity/message, control characters, empty/oversized ref, `BASE_MISMATCH`,
  reason bounds on accept/reject, malformed candidate, non-JSON content type still
  enveloped, correlation echo in header and body, invalid correlation replaced),
  fail-closed `VALIDATION_REQUIRED` with no decision/event/idempotency row written and
  reject still working, limits (ops, term, metadata, transport body isolated by whitespace
  padding, reconstruction quads refused at prepare with a replacement at capacity accepted
  and the boundary readable, depth refused at prepare with the boundary readable, bytes
  refused at prepare, export limit on read) → `RESOURCE_LIMIT`, readiness/OpenAPI and
  `/v1/commits` → enveloped `NOT_FOUND`. An earlier draft assumed the same content by a
  different actor yields the same candidate; corrected (provenance is v2 identity).
- Docker integration `./scripts/test-integration.sh` (image build, fresh volumes) on the
  final code: exit 0, `INTEGRATION OK`. All six PostgreSQL suites re-ran green
  (1/9/7/8/14/10); then in the container: `ledger-admin graph create` provisioned
  `it-graph-…` for `tenant-integration`; unauthenticated read → 401 `UNAUTHENTICATED`,
  foreign-tenant read → 404, `POST /v1/commits` → 404; prepare, then the same prepare again
  → 200 with the identical candidate and `replayed`; accept, then the same accept again →
  200 replaying the same head and `ref_version`; a second prepare/accept round;
  `docker compose restart ledger`; `/health` and `/ready` back; the authenticated ref read
  returned the second candidate and the bounded state read returned exactly the two
  expected quads; foreign-tenant state read → 404; SQL verification: 2 commits in
  `immutable_objects`, exactly 4 new objects overall (2 commits + 2 patches), 2
  `commit_index` rows with `version = 2` under the graph and no non-v2 commit anywhere,
  `refs.version = 2` with 2 `ref_events`, 2 accepted `decisions`, 2 `projection_outbox`
  rows, 4 `idempotency` rows (retries added none), correlation ids on every accepted
  decision and event, 0 node-local object files. Earlier attempts failed for
  environmental reasons only: `.dockerignore` excluded `docs/` (fixed by admitting
  `docs/api/openapi.json`) and a full disk stopped `docker compose build`.
- Independent read-only reviews (security, storage/concurrency, invariant, test; Opus)
  on the first complete draft. Agreed P1s, all fixed in this change: (1) migration 0007
  bound accept/reject idempotency rows to the proposer instead of the reviewer (would
  have blocked every four-eyes upgrade, or silently mis-scoped rows) → per-kind binding
  through `decisions`, guards extended, test seeds a reviewer decision and asserts the
  actor, unchanged results, re-enabled write-once trigger (snapshot now records
  `tgenabled`), repository replay for the bound actor, and populated-upgrade schema
  convergence; (2) configured reconstruction limits never reached `prepare` →
  `PostgresLedgerStore::with_limits` applied in `AppState::new`; (3) a branch could be
  accepted past the read limits and then never read or extended → prepare checks
  candidate depth and resulting quads/bytes (API test now proves the third commit is
  refused at prepare and the boundary is readable); (4) OIDC path had no executable test →
  five OIDC tests against a local JWKS server (RSA+EC, unknown kid then rotation, rate
  limiting and max key age, alg confusion/missing kid/missing claims/forged signature,
  unreachable key source → `KeySourceUnavailable` without URL leak); (5) `alg:none` test
  was vacuous → real claims plus an RS256-header token; (6) persisted identity never read
  back → API test asserts proposal/decision/ref-event/idempotency rows carry the token's
  tenant, principal, type, delegation and the client correlation id; (7) cached JWKS keys
  never expired → one-hour maximum age. P2s fixed: unique-constraint column order for
  index use, reconstruction keeps patch ids only, semaphore below pool size and pool
  timeouts → 503 `DEPENDENCY_UNAVAILABLE`, store-enforced validation policy after the
  replay lookup, evidence/field checks before any lock or reconstruction, JWKS client
  hardening, principal-type claim vs configuration conflict refused, unvalidated switch
  refused with production auth, readiness checks the schema level, OpenAPI documents
  413/500/503, unknown routes/methods use the envelope, integration script asserts exact
  quads, replay ids, outbox/idempotency counts, correlation ids and global non-v2 count,
  FK assertions pin constraint names, body-limit test isolates the transport limit,
  capability matrix table-driven, foreign accept covered with unchanged row counts,
  depth/bytes/export limits and idempotency-key/reason/ref bounds covered, `unique()`
  collision-free. Decisions recorded: `max_depth` is an operational branch ceiling until
  checkpoints (ADR-0013 note, tech-debt, Plan 0005); `sculpin-ledger-request/v1` frozen by
  ADR-0015; 0007 is an offline (stop-all-replicas) upgrade (migrations README). Accepted
  P3 risks are listed in `docs/exec-plans/tech-debt.md` (semaphore saturation and live
  Entra issuer untested until Plan 0005; client correlation ids stored verbatim; audit
  `tenant_id` doubles as graph tenant; candidates readable by any tenant reader).
- Fluree live differential: DEFERRED by policy (BUSL-1.1), not passed.

Security assumptions and dev-only switches: see `docs/quality/security.md`. Production
qualification: **NO** until Plan 0005 (P1.5) passes.

### Phase 1 closure pass (2026-09-26, PR #1 merge readiness)

Scope: make PR #1 merge-ready without redesign. Changes:
- **OIDC stale-key expiry defect fixed.** `key_for` previously returned a cached key after
  `max_key_age` when the refresh attempt failed and a later request fell inside the
  `min_refresh_interval` throttle. `refresh_keys` now returns an explicit
  `RefreshOutcome::{Refreshed, Throttled}`; a key is served only from a cache that was just
  refreshed or is still within `max_key_age`, otherwise `KeySourceUnavailable` (503). Time
  comes from an injectable `Clock` (`with_clock`) so the test drives age and throttling
  deterministically without wall-clock sleeps: fresh key works during an outage; past
  max age the same outage is `KeySourceUnavailable`; a request inside the throttle window
  also fails and performs no fetch; a successful refresh resets the lifetime; unknown kids
  stay fail closed (`oidc_never_trusts_a_cached_key_beyond_max_key_age`).
- **JWK metadata honoured.** `verification_algorithm` loads a key only if `use` is absent
  or `sig`, `key_ops` (when present) contains `verify`, the family is RSA or P-256, and
  `alg` (when stated) is exactly RS256 / ES256; PS256, RS512, ES384, sign-only or
  encrypt-only keys and `use`/`key_ops` contradictions are skipped, never reinterpreted
  (`oidc_honours_jwk_algorithm_and_key_operation_restrictions`, 12 cases). Existing
  alg-confusion tests unchanged. `docs/quality/security.md` restated to match.
- **Supply chain.** `cargo tree --target all -e normal,build -i rsa` is empty: `rsa 0.9.10`
  is a lockfile-only optional dependency (`sqlx → sqlx-mysql → rsa`) that no workspace
  feature enables; removing the `macros` feature would not remove it (the `sqlx` facade
  crate itself lists `sqlx-mysql` optionally), and dropping the facade is not a reasonable
  trade for a lockfile-only entry. Exception recorded in `.cargo/audit.toml` with the path,
  proof, advisory and review trigger; `scripts/check-supply-chain.sh` re-proves the premise
  (unreachable in the build graph, only dependent `sqlx-mysql`, version on the 0.9 line,
  exactly one exception) before `cargo audit`, so a fixed `rsa`, an sqlx change or a new
  advisory fails the gate. `ci-security`'s audit job is now blocking (no
  `continue-on-error`). Local: `./scripts/check-supply-chain.sh` exit 0 (cargo-audit 0.22.2,
  1271 advisories, 282 dependencies).
- **Dependency graph** enabled on the repository (Dependabot alerts turned on via the API;
  SBOM endpoint now answers), so `dependency-review-action` can run.
- **CI hygiene**: `actions/checkout@v6` in all four workflows; pinning actions by commit
  SHA recorded for Plan 0005.
- **Old Codex threads** (walking-skeleton commit) verified against the current code and
  resolved with notes: directory fsync after rename (`sync_directory` in
  `put_object_sync`), `Quad`/`Patch` Serde deserialization through `FromStr`/`Patch::new`
  (`serde_cannot_bypass_*` tests), standards N-Quads ingress via `oxttl`.
- **Governance**: `main` has no protection or rulesets; a ruleset requiring `ci-fast`,
  `ci-integration`, `ci-security` and resolved conversations is recommended in the PR (not
  imposed).

Evidence for this pass is recorded in the PR description and the final report; CI on the
final head must show `ci-fast`, `ci-integration` and `ci-security` green.

## Test evidence
- 2026-09-26 `python3 scripts/golden/commit_v2_reference.py check`: exit 0, "all 18
  commit v2 vectors (positive and negative) match the reference encoder";
  `v2-linear.sha256` == `v2-evidence-reordered.sha256` (`sha256:979f8d93…eb11d`),
  pinning evidence order-independence.
- 2026-09-26 independent read-only reviews (invariant reviewer; static compile/clippy
  reviewer): byte layout, absent-vs-empty, set semantics, caps, and dual read confirmed
  sound; no compile or clippy failures found by inspection. Confirmed defects, all fixed
  in the same change: `LedgerTimestamp` kept sub-microsecond nanoseconds so equality
  disagreed with identity (now truncated at construction); UTC years outside 0001..=9999
  could be encoded but not decoded or could panic in offset conversion (now rejected via
  `checked_to_offset` + explicit range); offset hours/minutes and trailing-newline
  acceptance differed between the Python reference and Rust (both now bound 00–23/00–59
  and use full-match). Decisions recorded in `canonicalization.md`: tokens are opaque
  byte strings (no Unicode normalization, no IRI grammar in identity); core reads no
  clock (`try_from_offset_date_time` takes the service layer's instant).
- 2026-09-26 `scripts/check-doc-links.py` and `scripts/check-architecture.py`: exit 0.
- 2026-09-26 `./scripts/check-fast.sh` (fmt check, doc links, architecture, clippy with
  `-D warnings`, `cargo test --workspace`): exit 0. `ledger-core` lib: 23 passed;
  `tests/golden_v2.rs`: 9 passed (Rust reproduces every v2 `.hex`/`.sha256` produced by
  the Python reference byte-for-byte, all 12 decodable negatives rejected with the
  expected error kind, unknown version fails closed, v1 vectors still read through
  `AnyCommit`); `tests/golden.rs` v1: 3 passed. The single ignored test is the
  pre-existing Docker-gated PostgreSQL CAS test. First run surfaced two unit tests that
  compared a decoded commit against an unsorted sample (structural `Eq` vs canonical
  order); the sample was corrected, no protocol change.

- 2026-09-26 bridging task: `cargo fmt`, `./scripts/lint.sh`, `cargo test --workspace`
  exit 0 (`ledger-core` 23 lib + 9 golden_v2 + 3 golden v1; `ledger-store` 8 lib incl.
  `existing_non_commit_object_cannot_become_head` and
  `store_holds_v1_and_v2_commits_side_by_side`). Real PostgreSQL (compose
  `postgres:17.2`, `LEDGER_TEST_DATABASE_URL`): `pg_cas_race` 1 passed;
  `pg_immutable_store` 3 passed — `replica_b_reconstructs_what_replica_a_committed`
  (two independent pools, B reconstructs A's commits and continues the line),
  `commit_index_is_verified_typed_and_idempotent`, `v1_binding_policy_is_enforced_on_write`.
  Full `./scripts/test-integration.sh` (image build, CAS race, replica suite, HTTP
  commit/restart with `LEDGER_IMMUTABLE_BACKEND=postgres`): exit 0, "INTEGRATION OK" —
  ref head and reconstructed state survived a ledger-container restart with no local
  objects, so the shared-backend topology holds end to end.

- 2026-09-26 P1.2 closure gates: `./scripts/check-fast.sh` exit 0 (fmt check, doc links
  44 files, architecture, clippy `-D warnings`, `cargo test --workspace`: ledger-core
  23+9+3, ledger-store 9 lib incl. `filesystem_store_is_strict_about_content_headers_and_layout`,
  ledger-server 4 backend-selection tests); `python3 scripts/golden/commit_v2_reference.py
  check` all 18 vectors match; `git diff HEAD -- crates/ledger-core fixtures` touches only
  error variants/docs — no protocol identity change. Real PostgreSQL 17.2 (fresh compose
  volume): `pg_cas_race` 1, `pg_immutable_store` 8, `pg_graphs_migration` 6,
  `pg_fs_migration` 7 — 22 passed, 0 failed (one first-run failure was the blocked-path
  test deadlocking on its own single-connection poll pool; fixed in the test, store
  unchanged). Full `./scripts/test-integration.sh` exit 0 with the default backend:
  "shared backend confirmed: 2 commits in PostgreSQL, 2 indexed under 'default', 0
  node-local object files"; HEAD and state survived the ledger-container restart.
  `./scripts/test-differential.sh` exit 0 seam-only: the live Fluree comparison is
  DEFERRED by policy (BUSL-1.1 sign-off), not passed.

## Completion criteria
The Phase 1 gate passes with recorded evidence, the carried-forward obligations each have
an executable test, and independent storage/concurrency, invariant, security, and test
reviews raise no unresolved P0/P1 findings.
