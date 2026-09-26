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

**P1.4 (authenticated HTTP boundary) is next.** The API must route writes through
`WorkflowRepository::prepare`/`accept` with the authenticated principal, define the
canonical request bytes for the digest, retire the bootstrap v1 write path and flip the
shared store to `V1Binding::Reject`.

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
