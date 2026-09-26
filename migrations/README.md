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

## Filesystem → PostgreSQL content migration
Schema migrations never move content. Immutable objects move with the administrative
`ledger-admin migrate-fs-to-pg` command (see `docs/design/storage-boundaries.md`), which is
idempotent, resumable, verification-first and never overwrites a ref that already differs.
