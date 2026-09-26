# Database identities, schema ownership and runtime least privilege

## Status
Accepted (2026-09-26, Plan 0005 / P1.5 slice 1). Affects persistent integrity: the
database privilege boundary is part of the ledger's production integrity model.

## Context
Through Phase 1 the service connected with one PostgreSQL identity that owned every table,
ran migrations on connect and could therefore `ALTER`/`DROP` tables, `DISABLE TRIGGER`
and rewrite rows. The write-once triggers (0005/0006), tenant binding and CAS refs were
"accident guards, not a defence against a table-owning role" (migrations README). A
runtime identity with that authority cannot be trusted to protect immutable history: a bug,
an injected statement or a compromised replica could silently rewrite commits, decisions
or idempotency results. Plan 0005 item 1 requires a real boundary before any non-
development deployment.

## Decision
Two database identities with distinct responsibilities, supplied by the operator:

**Schema owner / migration identity** (`LEDGER_MIGRATION_DATABASE_URL`)
- owns every ledger object (tables, sequences, functions, triggers);
- is the only identity that runs DDL: `ledger-admin migrate` applies the embedded,
  monotonic, checksummed migrations 0001…N on a dedicated single connection (never a
  pooled runtime connection) and then applies the runtime grants;
- performs administrative DML the runtime must not: graph provisioning
  (`ledger-admin graph create`), the filesystem→PostgreSQL cutover, future lifecycle
  transitions and history imports.

**Runtime application identity** (`LEDGER_DATABASE_URL`)
- has exactly the DML the request path executes (derived from the store's SQL, not from
  prose): `SELECT` on all ledger tables and `_sqlx_migrations`; column-level `INSERT`
  naming exactly the columns the store's INSERT statements name on `immutable_objects`,
  `commit_index`, `commit_parents`, `refs` (no `protected`, no timestamps), `proposals`,
  `ref_events`, `decisions`, `projection_outbox` (no `delivered_at`/`attempts`),
  `idempotency` (ids and timestamps keep their defaults); `UPDATE (head, version,
  updated_at)` on `refs` only; `USAGE` on the audit sequences; `USAGE` on the schema;
- has **no** `CREATE` on the schema, no ownership, no `ALTER`, `DROP`, `TRUNCATE`,
  `DISABLE TRIGGER`, no `UPDATE`/`DELETE` on immutable or audit tables, no `INSERT` or
  `UPDATE` on `graphs`, no `UPDATE`/`DELETE` on `projection_outbox` (Phase 3 grants the
  consumer its own delivery columns), and cannot run migrations;
- runs every session with bounded `statement_timeout`, `lock_timeout` and
  `idle_in_transaction_session_timeout` set by the pool at connect (configurable;
  `LEDGER_DB_*_TIMEOUT_MS`).

**Grants are versioned schema.** Migration 0008 installs `ledger_grant_runtime(role text)`
(EXECUTE revoked from PUBLIC; `search_path` pinned to `pg_catalog, public`; refuses a
superuser, a role holding `CREATE` on the schema, and any caller that is not the owner of
the ledger tables), a reviewable SQL function that first `REVOKE`s everything the role holds
on the schema's tables, sequences and functions and then applies exactly the grant set
above with `format('%I')`. The migration itself creates no role and assumes no
`CREATEROLE`: managed PostgreSQL offerings frequently deny it to application owners, and
credential lifecycle belongs to the operator. `ledger-admin migrate --runtime-role <name>`
(the role name must be a plain SQL identifier) calls the function after migrating;
re-running is idempotent.

**Integrity the runtime cannot bypass (migration 0009).** Because the runtime holds
`UPDATE (head, version)` on `refs`, privileges alone would still let a compromised runtime
rewind or jump a ref without an event. A deferred constraint trigger therefore makes every
head movement on an `active`/`archived` graph require, in the same transaction, a matching
`ref_events` row (same graph, branch, old/new head and versions) and a fast-forward (the new
head's first parent is the old head); `bootstrap`/`importing` graphs stay exempt for the
owner-only raw ref path. `graphs` gets a `BEFORE UPDATE OF status` trigger that takes the
exclusive `graph-status:` advisory lock, so any lifecycle transition — including raw
operator SQL — waits for in-flight workflow transactions. `immutable_objects` gains a CHECK
that `id` is the SHA-256 of `bytes`. Advisory-lock keys are derived on both sides as the
first eight bytes of SHA-256 over a domain-separated key string (`ledger_store::lock_key`
and SQL `ledger_lock_key`), replacing `hashtextextended`, whose collisions an attacker could
search for with a chosen `Idempotency-Key`.

**Startup verifies the identity, not only the schema.** After `schema::verify`, the server
checks the connected role: not a superuser, not the owner of any ledger table, no `CREATE`
on the schema, none of the forbidden privileges, all of the required grants. A deployment
that kept the Phase-1 owner URL in `LEDGER_DATABASE_URL` therefore refuses to start
(`RUNTIME_IDENTITY`) instead of serving with owner rights.

**Startup is verify-only.** `PostgresLedgerStore::connect_with` (the server's constructor)
connects as the runtime identity, applies the session limits and verifies the schema:
`_sqlx_migrations` must exist, every recorded migration must have succeeded, its checksum
must equal the embedded migration's, the recorded history must be contiguous, the highest
version must equal `REQUIRED_SCHEMA_VERSION` exactly, and every integrity trigger must be
present and enabled. Behind, ahead, absent, corrupt or disabled-guard states each fail
startup with a distinct, actionable `SchemaIncompatible` message and make `/ready` answer
503 (the specific reason goes to the log; the HTTP body stays generic); the server never
upgrades a database and never needs owner privileges. `connect_and_migrate`, `from_pool`
and `PgRefStore::with_ref` exist for tests and tooling only.

**Locking without UPDATE privilege.** PostgreSQL grants `SELECT … FOR SHARE/UPDATE` only to
identities holding `UPDATE` on the table, which the runtime lacks on `graphs` and
`proposals`. The workflow therefore serializes on advisory locks instead of row locks
where it does not update the row: every operation (prepare, accept, reject,
mark_superseded) takes the shared `graph-status:` lock before reading the graph, and every
decider (accept, reject, mark_superseded) takes the exclusive `proposal-decision:` lock
before recording a terminal decision; the `decisions` unique indexes remain the integrity
guarantee. Lock order is fixed — idempotency scope → graph-status (shared) → `refs FOR
UPDATE` (accept only) → proposal-decision — so no cycle exists. `refs` keeps `FOR UPDATE`
because the runtime updates it.

**Deployment sequence.** PostgreSQL up → operator creates the runtime role (credential
managed outside the ledger) → `ledger-admin migrate --runtime-role <name>` with the owner
URL → start servers with the runtime URL → `/ready` reports the schema level. Upgrades:
stop replicas (0007 note), migrate as owner, start the new build; a build started against
a different schema level refuses to serve.

## Alternatives considered
- **Migration creates the runtime role.** Requires `CREATEROLE`/password handling inside
  migrations; not portable to managed PostgreSQL; rejected.
- **Column-level `UPDATE` grant on `graphs` to keep `FOR SHARE`.** Would let the runtime
  edit graph metadata; rejected in favour of advisory locks.
- **Keep migrate-on-connect with a privileged runtime.** The status quo the plan exists to
  remove.

## Consequences
- A compromised runtime cannot change the schema, disable an integrity control, modify or
  delete any existing immutable or audit row, provision or re-home graphs, rewind or jump a
  ref, or store an object under the wrong id. **Residual risk, accepted for P1.5:** the
  runtime is, by design, the trusted writer of *new* audit rows, so a compromised runtime
  can still fabricate a consistent forward move (a fast-forward ref move together with its
  event, decision and outbox rows) or pre-seed idempotency results within the tenants it
  serves. Closing that requires `SECURITY DEFINER` write functions as the only write path
  (tech-debt; a later phase). The write-once triggers are a second line, not the only one.
  A confirmed defect in the grant set is a P0.
- Session limits are set per session at connect; a transaction-mode connection pooler
  (PgBouncer in transaction mode) would drop them, so the runtime must connect directly or
  through a session-mode pooler.
- `ledger-admin` and the compose harness gain a migration step; `graph create` and the
  cutover use the owner URL. `docs/operations/deployment.md` is the operator procedure.
- Plan 0005 items 2–10 run under the restricted identity so their evidence reflects
  production privileges.
