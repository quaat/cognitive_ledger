# Migrations

PostgreSQL migrations are applied in order by `sqlx::migrate!` at store start-up. Released
migrations are immutable: never edit one, add the next number. Every migration is tested
for clean install and for the supported upgrade path against a real PostgreSQL
(`scripts/test-integration.sh`, `crates/ledger-store/tests/pg_graphs_migration.rs`).

| # | Contents | Notes |
|---|---|---|
| 0001 | `refs` (graph_id, branch, head) | Mutable ref CAS point (ADR-0004/0007). Bootstrap topology `('default','main')`. |
| 0002 | `immutable_objects` | Content-addressed, write-once bytes for patches and commits (ADR-0012). |
| 0003 | `commit_index`, `commit_parents` | Derived, verified index written in the same transaction as the commit bytes (ADR-0012). |
| 0004 | `graphs` + FKs | Graph authority (ADR-0010): globally unique `graph_id`, immutable `tenant_id` (trigger), many graphs per KB; `refs.graph_id` and `commit_index.graph_id` reference `graphs` with `ON DELETE RESTRICT`. |
| 0005 | write-once triggers | `immutable_objects`, `commit_index`, `commit_parents` refuse UPDATE/DELETE; `refs` identity columns are immutable (only `head` moves). Accident guard, not a defence against a table-owning role. |
| 0007 | actor scope, correlation, tenant integrity | `idempotency` gains `principal_type`/`on_behalf_of` (backfilled from each row's proposal; fails closed on unbindable or mismatched rows), a surrogate primary key and `UNIQUE NULLS NOT DISTINCT (tenant_id, principal_id, principal_type, on_behalf_of, graph_id, operation, idempotency_key)`; bounded `correlation_id` (1–128 bytes, nullable, audit only) on `proposals`, `ref_events`, `decisions`; `graphs UNIQUE (graph_id, tenant_id)` and composite `(graph_id, tenant_id) → graphs` FKs from `proposals`, `ref_events`, `decisions`, `idempotency`. |
| 0008 | runtime least privilege (ADR-0016) | Installs `ledger_grant_runtime(role text)` (owner-only, pinned `search_path`, refuses superusers and roles with `CREATE` on the schema): revokes everything the role holds on the schema, then grants `USAGE` on the schema, `SELECT` on every ledger table and `_sqlx_migrations`, column-level `INSERT` matching the store's INSERT statements, `UPDATE (head, version, updated_at)` on `refs`, `USAGE` on the audit sequences. Creates no role. Applied by `ledger-admin migrate --runtime-role <name>`. |
| 0009 | ref movement integrity (ADR-0016) | Deferred constraint trigger: a head move on an `active`/`archived` graph needs a matching `ref_events` row in the same transaction and must be a fast-forward; `graphs` status changes take the exclusive `graph-status:` advisory lock; `immutable_objects.id` must be the SHA-256 of `bytes`; `ledger_lock_key(text)` mirrors `ledger_store::lock_key`. |
| 0010 | semantic validation (ADR-0018/0019) | `semantic_execution_contexts` and `validation_records` (content-addressed: id = SHA-256 of `canonical_bytes`, CHECK-enforced), `semantic_virtual_contexts`, `validation_violations`, `decision_validations` (composite FKs prove a decision cites validations of its own graph and candidate; the record→context FK binds graph, candidate, state digest and validator); `idempotency` gains operation `validate`, result `validated` and `result_validation_id` (composite FK to a record of the same graph and candidate, shape CHECK); `decisions_identity UNIQUE (decision_id, graph_id, candidate_commit)`; write-once triggers; `ledger_grant_runtime` re-issued with column-level INSERT on the new tables. Guard: refuses if any decision already cites validation ids. |
| 0006 | workflow persistence | `refs.version` (monotonic, trigger-enforced) and `refs.protected`; composite FK `refs(graph_id, head) → commit_index(graph_id, id)`; append-only `proposals`, `ref_events`, `decisions`; `projection_outbox` (identity immutable, delivery columns mutable); `idempotency` results. Guard: fails with the offending `(graph, branch → head)` list if any existing ref head is not an indexed commit of its graph. |

## Upgrade semantics of 0004
- The bootstrap graph `default` gets an explicit row `(tenant_id='bootstrap',
  status='bootstrap')` on clean install and upgrade alike. That tenant is **non-production
  legacy state** for the pre-v2 write path, not a real tenant; it is retired when the v2
  write path lands.
- Any other `graph_id` already present in `refs` or `commit_index` has no derivable owner.
  The migration then **fails** with an actionable error naming the graphs, and applies
  nothing (no `graphs` table, no FK). Register those graphs through the audited graph
  import path first; the ledger never guesses a tenant.
- Clean install and upgrade converge on the same schema (columns, constraints, indexes,
  triggers, function bodies), which the upgrade test compares literally; the guard's
  refusal is also tested for the `commit_index` half and for a successful re-run after the
  unowned rows are removed.

## Upgrade semantics of 0006
- The composite FK makes "a ref never points to missing content" and "a ref's head belongs
  to its graph" schema invariants. Existing rows must already satisfy them; the guard names
  every violating `(graph, branch → head)` and applies nothing.
- Consequence: the legacy topology *filesystem objects + PostgreSQL ref* cannot exist at
  0006 or later, and the server refuses `LEDGER_IMMUTABLE_BACKEND=filesystem` with a
  database URL. Cut such a database over in this order (which `ledger-admin
  migrate-fs-to-pg` performs): migrate the schema up to 0005 only
  (`schema::migrate_up_to(pool, CONTENT_SCHEMA_VERSION)`), import and verify the filesystem
  content against the existing shared ref, then apply the remaining migrations.
- Existing refs receive `version = 1`; every later head movement must bump it by exactly one
  (trigger), including the raw `PgRefStore` primitive.

## Who runs migrations (ADR-0016)
Only the schema owner, through `ledger-admin migrate` on a dedicated connection
(`LEDGER_MIGRATION_DATABASE_URL`). The server connects with the runtime identity and
**verifies** the schema instead: it refuses to start (and `/ready` refuses) when the
recorded level is behind or ahead of `REQUIRED_SCHEMA_VERSION`, when the migration table is
absent, when a recorded migration failed, or when a recorded checksum differs from the
embedded migration (released migrations are immutable). The runtime role cannot run DDL,
so a misconfigured deployment cannot migrate by accident.

## Upgrade semantics of 0010
- Additive: five tables, one FK-target UNIQUE on `decisions`, two re-issued CHECKs on
  `idempotency` (the runtime's `validate` operation), one nullable FK column on
  `idempotency`, write-once triggers and the re-issued grant function. No content is
  rewritten. Stop the replicas first (a pre-0010 build refuses a 0010 database as *ahead*,
  a 0010 build refuses 0009 as *behind*), then `ledger-admin migrate --runtime-role <role>`
  so the Phase-2 column grants are applied.
- Content identity is a schema fact: `context_id`/`validation_id` must equal the SHA-256 of
  the stored canonical bytes (ADR-0018), probed at start-up like the object CHECK.
- `decisions.validation_ids` keeps its meaning and is populated identically to
  `decision_validations`; the relation is the enforced one (`ledger-admin verify` checks
  they agree).

## Upgrade semantics of 0008 and 0009
- 0008 is additive: one function and a `REVOKE`. Requires the operator-created runtime role
  to exist before `ledger-admin migrate --runtime-role` is run; re-running is idempotent
  (the function revokes and re-grants). The caller must own the ledger tables.
- 0009 adds a deferred constraint trigger on `refs`, a `BEFORE UPDATE OF status` trigger on
  `graphs`, a CHECK on `immutable_objects` (validated against existing rows; every stored
  object is already content-addressed) and the `ledger_lock_key` function. Existing history
  is unaffected; from 0009 on, raw ref moves on `active`/`archived` graphs are refused
  without a matching event, and any status change waits for in-flight workflow
  transactions. The runtime's graph-status check and proposal decisions use advisory locks
  (`ledger_lock_key('graph-status:' || graph_id)`, `'proposal-decision:' || proposal_id`)
  instead of `FOR SHARE`/`FOR NO KEY UPDATE`, which require `UPDATE` privilege.

## Upgrade semantics of 0007
- Requires PostgreSQL 15 or later (`UNIQUE NULLS NOT DISTINCT`); compose pins 17.2.
- **Stop every replica before applying 0007.** A pre-0007 binary inserts idempotency rows
  without `principal_type` (NOT NULL violation → 500) and scopes lookups by the incomplete
  actor. The migration also takes ACCESS EXCLUSIVE locks on the workflow tables while it
  validates four foreign keys, so it is an offline step: stop → migrate → start.
- Each idempotency row is bound to the actor of the request it recorded: a `prepared` row
  through its proposal (the proposer), an `accepted`/`rejected` row through its decision
  (the reviewer, who may differ from the proposer). The backfill temporarily disables the
  `idempotency` write-once trigger inside the migration transaction and re-enables it in
  the same transaction, so a failure rolls everything back and leaves the guard in force.
  Guards run first: a row that cannot be bound, a row whose proposal/decision actor,
  tenant or graph disagrees with it, or any audit row whose `(graph_id, tenant_id)` does
  not match `graphs`, fails the migration with the offending identifiers and applies
  nothing.
- The unique constraint lists the equality-searched columns first
  `(tenant_id, graph_id, operation, idempotency_key, principal_id, principal_type,
  on_behalf_of)` so the repository's `IS NOT DISTINCT FROM` lookup on `on_behalf_of` is
  bounded by the key, not by an actor's whole history.
- After 0007 an idempotency key is a namespace per complete actor: the same key used by
  the same principal id as a `service` rather than an `agent`, or on behalf of a different
  human, is a different request, not a replay.

## Filesystem → PostgreSQL content migration
Schema migrations never move content. Immutable objects move with the administrative
`ledger-admin migrate-fs-to-pg` command (see `docs/design/storage-boundaries.md`), which is
idempotent, resumable, verification-first and never overwrites a ref that already differs.
A failed sqlx migration run keeps its session-level advisory lock on the pooled connection
that ran it; retry from a fresh process (or a fresh pool), which the admin tool does.

Before applying 0009 to an existing database run `ledger-admin verify` (owner identity): 0009 validates the content-addressed CHECK by hashing every stored object under `ACCESS EXCLUSIVE` and aborts on a corrupt row with a raw `23514` that names no id; the offline window grows with the store size.
