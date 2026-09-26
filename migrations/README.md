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
