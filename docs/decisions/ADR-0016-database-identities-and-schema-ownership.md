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

## Amendment: Phase 2 grant set (2026-09-27, Plan 0006, migration 0010)
Migration 0010 re-issues `ledger_grant_runtime` (same safety rules: owner-only, pinned
`search_path`, revoke-then-grant, refuses superusers and CREATE holders) with the Phase-2
columns derived from `ValidationRepository`'s and `WorkflowRepository`'s SQL:

- `SELECT` on the new tables `semantic_execution_contexts`, `semantic_virtual_contexts`,
  `validation_records`, `validation_violations`, `decision_validations`;
- column-level `INSERT` naming exactly the columns the store's INSERT statements name
  (content ids, canonical bytes, bounded provenance columns, `recorded_at` on
  `validation_records` because it is part of the hashed record — never `created_at`);
- `INSERT (…, result_validation_id)` added on `idempotency`;
- no `UPDATE`/`DELETE` on any of them (write-once triggers as a second line); no new
  sequences (content ids are text; detail rows are keyed by `(id, position)`).

The runtime role therefore may record legitimate validation contexts and records but cannot
rewrite them. A Sculpin validator never holds a database identity: it answers the ledger's
outbound call, and the ledger's runtime identity records the result. A separate
validation-service database identity was evaluated and not introduced: it would gain
nothing while the ledger is the only writer, and would widen the trust surface.
`verify_runtime_identity`'s table model, sequence model, guard-trigger, constraint and CHECK
inventories cover the new objects; `pg_least_privilege` exercises them on PostgreSQL 15
and 17.

## Amendment: Phase 3 projector identity and grant set (2026-09-28, Plan 0007, migration 0011)
Migration 0011 adds a **third** database identity, the projector
([ADR-0021](ADR-0021-projection-state-leases-and-projector-identity.md)), under the same rules
as the runtime identity: a login role that owns nothing, is no member of (and cannot `SET
ROLE` to) the owner, the runtime role or any privileged role, and is granted only by the
owner-only, idempotent `ledger_grant_projector(role)` (`ledger-admin migrate
--projector-role`, which refuses the runtime role's name). Its model is exact and exhaustive:

- `SELECT` on `graphs`, `refs`, `immutable_objects`, `commit_index`, `commit_parents`,
  `ref_events`, `projection_outbox`, `projection_state`, `_sqlx_migrations`;
- column `UPDATE` on `projection_outbox (delivered_at, attempts)` and on the progress,
  lease, backoff, error and status columns of `projection_state` — never its identity
  columns; status changes into or out of `disabled` are owner-only (guard trigger);
- no `INSERT`, `DELETE`, `TRUNCATE`, `REFERENCES`, `TRIGGER`, `MAINTAIN` (PostgreSQL 17), no
  sequence privileges, no `EXECUTE` on either grant function.

`ledger_grant_runtime` is re-issued by 0011 with one addition, `SELECT` on
`projection_state` (status reads); the runtime identity gains no write on projection state
or on the outbox delivery columns, so the HTTP server can never mark projection progress.
Verification is one parameterized routine (`IdentityModel`: table model, sequence model,
exhaustive "no unlisted table privilege" check — exhaustive for both identities since the
Phase-3 Codex review; before, the runtime check covered only its listed tables) used for
both identities at start-up, and by
the projector's readiness as well (the runtime server's readiness compares definition
fingerprints). Since 0011 it also refuses CREATE on the database and on any schema, not just
`public`, and the 0011 guard functions pin `search_path` like the 0009 ones: an identity
that could create objects could shadow what an unpinned function resolves.
`pg_projection` and `pg_least_privilege` exercise it on PostgreSQL 15 and 17, including
drift in either direction, CREATE on the database or an owned schema, projector readiness
after a grant or definition drift, and weakened 0011 guards, indexes, FKs and CHECKs.

## Amendment: Phase 4 branch grant set (2026-09-28, Plan 0008, migration 0012)
Migration 0012 ([ADR-0022](ADR-0022-named-branches-lifecycle-and-policy.md)) re-issues
`ledger_grant_runtime` with the branch tables and changes one existing grant; the projector
model is unchanged (it never reads branch lifecycle).

- `refs`: column `INSERT` now includes `protected` (branch creation records the policy flag;
  `refs_main_protected` CHECK keeps `main` protected, and the guard triggers keep
  `protected` immutable after insert). `UPDATE (head, version, updated_at)` unchanged.
- `branches`: `SELECT`; column `INSERT` of the identity, origin, source and policy columns
  plus `status`/`lifecycle_version`; column `UPDATE (status, lifecycle_version, updated_at)`
  only. `branches_guard` forbids `DELETE`, any change of identity or policy, any
  `lifecycle_version` step other than +1 on a status change, and origin `adopted` from any
  role but the owner.
- `branch_events`: `SELECT`; column `INSERT` (never `event_id`/`recorded_at`, which are
  database-assigned); write-once trigger; `USAGE` on `branch_events_event_id_seq`.
- `idempotency_keys`: column `INSERT` additionally covers `result_branch_event_id`.

The deferred constraint triggers (`branches_lifecycle_audited`, `branch_events_current`,
`refs_are_branches`) make "every status change is exactly one event and every event describes
the current state" a commit-time fact for the runtime as for any role. **Residual (same
trusted-writer class as ADR-0016):** a compromised runtime can still write a consistent,
fabricated lifecycle event (a delete or restore it was never asked for) within its tenants;
it cannot remove history, move a deleted branch's head, un-protect `main` or rewrite a policy.
`pg_least_privilege` asserts the exact 0012 model (drift in `refs.updated_at`,
`branch_events.recorded_at`, `branches.require_validation` refused) on PostgreSQL 15 and 17.
