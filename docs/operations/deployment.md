# Deployment and operations (Phase 1 / P1.5)

**Status: not production-qualified.** Plan 0005 (P1.5) is in progress; this document is the
operator procedure the qualification slices are verifying. `docker compose` in this
repository is a development harness, not a deployment example: it bakes credentials,
enables development-only switches and disables TLS.

## Identities (ADR-0016)
| Identity | Environment variable | Used by | May |
|---|---|---|---|
| Schema owner / migration | `LEDGER_MIGRATION_DATABASE_URL` | `ledger-admin` only | run DDL/migrations, grant the runtime role, provision graphs, run the filesystem cutover |
| Runtime application | `LEDGER_DATABASE_URL` | `ledger-server` only | the DML the request path executes (migration 0008 grant set); nothing else |

The runtime role is created by the operator (managed secret); the ledger never creates
roles. Never give a serving container the owner URL (the server warns if it sees one).

## Sequence
1. PostgreSQL 15+ up; create the runtime role: `CREATE ROLE ledger_runtime LOGIN PASSWORD …`.
2. `LEDGER_MIGRATION_DATABASE_URL=… ledger-admin migrate --runtime-role ledger_runtime`
   — applies migrations 0001…0009 on one dedicated owner connection (60 s lock timeout,
   so a running replica or a held migration lock fails loudly instead of hanging), grants
   the role (idempotent; the role must be a plain identifier, must not be a superuser and
   must not hold `CREATE` on the schema), then prints `schema at 0009 (required 0009)`.
   The owner identity should itself not be a superuser in production (an ordinary database
   owner suffices; `pg_least_privilege` exercises that shape). If a database was populated
   through `ledger-admin migrate-fs-to-pg`, pass `--runtime-role` there too or run
   `migrate --runtime-role` afterwards.
3. Provision graphs: `ledger-admin graph create --graph <id> --tenant <id> --status active`.
4. Start the servers with `LEDGER_DATABASE_URL` (runtime role), `LEDGER_AUTH_MODE=oidc`,
   issuer/audience/JWKS, limits. Startup connects, applies the session limits, **verifies**
   the schema is exactly 0009 with contiguous, checksum-matching history and every
   integrity trigger enabled, and refuses otherwise (behind: "run ledger-admin migrate";
   ahead: "deploy a newer build"; absent/corrupt metadata or a disabled guard: refuse), then
   verifies its own identity (not a superuser, not the owner, no `CREATE`, exactly the
   runtime grants) and refuses with `RUNTIME_IDENTITY` otherwise. `/ready` repeats the
   schema verification live. Connect directly or through a session-mode pooler: the session
   limits are set per connection.
5. Terminate TLS in front of the service; bearer tokens travel in clear otherwise.

## Upgrades
Stop every replica (0007 note in `migrations/README.md`), run step 2 with the new
`ledger-admin`, start the new build. A build started against another schema level refuses
to serve; nothing upgrades implicitly.

## Runtime limits
- HTTP: `LEDGER_LIMIT_*` (body, operations, terms, metadata, reconstruction depth/quads/
  bytes, export bytes, request seconds, concurrent expensive operations).
- Database session (every runtime connection): `LEDGER_DB_STATEMENT_TIMEOUT_MS` (30000),
  `LEDGER_DB_LOCK_TIMEOUT_MS` (10000), `LEDGER_DB_IDLE_IN_TRANSACTION_TIMEOUT_MS`
  (60000), `LEDGER_DB_MAX_CONNECTIONS` (16); values are milliseconds up to 2³¹−1. A
  cancelled statement, lock wait, deadlock or serialization failure is rolled back and
  reported as a retryable 503 `DEPENDENCY_TIMEOUT`; a terminated idle transaction as 503
  `DEPENDENCY_UNAVAILABLE`. Clients retry with the same `Idempotency-Key`.

## Health and readiness
`/health` is process liveness. `/ready` answers 200 only when the database answers under
the runtime identity and the schema level is exactly the required one.

## Backup, restore, failure recovery
Plan 0005 items 8–9 (pending): `pg_dump`/`pg_restore` and base-backup qualification with
head/state digests before and after; kill-injection recovery evidence. Until they land,
treat the documented invariant queries (`docs/quality/test-strategy.md`) as the post-
recovery check and retry ambiguous requests with their original idempotency keys.

## Development-only switches (never in production)
`LEDGER_AUTH_MODE=dev-hs256`, `LEDGER_ALLOW_INSECURE_NON_LOOPBACK=allow-insecure-non-loopback-development-only`,
`LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only`, the compose
credentials, `deploy/postgres-init/` (development runtime role with a baked password).
The server refuses the unvalidated-acceptance switch together with production auth.

## Required gates before a release
`./scripts/check-fast.sh`, `./scripts/check-supply-chain.sh`, the real PostgreSQL suites
(including `pg_least_privilege`), `./scripts/test-integration.sh`; and, once Plan 0005 is
complete, its stress, fault, multi-replica, fuzz, upgrade, backup/restore and performance
evidence.
