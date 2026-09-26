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
`ledger-admin` (`migrate --runtime-role <role>` under the owner identity), start the new
build. A build started against another schema level refuses to serve; nothing upgrades
implicitly. The path from the previous release is exercised by `scripts/upgrade.sh`
(previous image built from git, data written through its API, upgrade in this order, then:
schema at the required level with the checksums of already-applied migrations untouched,
identical heads/versions and reconstructed states, verbatim replay of the previous
release's idempotency keys, new writes, `ledger-admin verify`, and a `pg_dump --schema-only`
diff between the upgraded database and a clean install, which must be empty). Run it
before every release; its evidence is recorded in the active Plan 0005 document.

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
the runtime identity and the schema level is exactly the required one. The runtime image
(`gcr.io/distroless/cc-debian12:nonroot`, digest-pinned in the Dockerfile) has no shell or
curl: probe with `ledger-admin probe http://127.0.0.1:8080/ready` (exit 0 on 2xx, 1
otherwise; the URL must be plain http to a loopback address or `localhost` without
credentials, and is never printed), as `compose.yaml` does. Both binaries live in `/usr/local/bin`; the process runs as uid 65532.

## Invariant verification
`LEDGER_MIGRATION_DATABASE_URL=… ledger-admin verify [--json]` runs the read-only invariant
suite (Plan 0005 §20: content addressing, index/parent consistency, ref ↔ event ↔ decision
↔ outbox agreement, contiguous fast-forward event chains, tenant agreement, idempotency
references, no v1 commit under a production graph) and exits non-zero on any violation. It
never repairs. Run it after every upgrade, restore or incident; the integration harness runs
it after every end-to-end scenario.

## Backup, restore, failure recovery
- **Logical backup:** `pg_dump -Fc` of the ledger database (a consistent snapshot even
  under writes). Restore with `pg_restore --no-owner` into a new database as the owner
  identity, then `ledger-admin verify --database-url <owner url of the restored database>`
  must print `VERIFY OK` before any server is pointed at it. Grants for the runtime role
  are part of the dump (the role must exist in the target cluster; it is cluster-wide).
- **Physical backup:** `pg_basebackup -c fast -X stream` by a role with `replication`
  privilege (the development compose has no network replication entry in `pg_hba.conf`;
  production grants it to a dedicated backup role over TLS). A new instance started from
  the copy is verified the same way.
- **What restore preserves:** every ref's event chain is an exact prefix of the live chain
  as of the snapshot, and a server on the restored database serves the restored heads and
  reconstructs states identical to the live server for the same commit ids (content
  identity makes the comparison exact). `scripts/backup-restore.sh` proves both variants
  under live load; run it per release.
- **Failure recovery:** a killed replica is restarted; a killed PostgreSQL recovers from
  its WAL and running replicas reconnect without restart (`/ready` returns 200 again).
  After any recovery run `ledger-admin verify`; clients retry ambiguous requests with their
  original `Idempotency-Key` and receive the durable outcome (`replayed: true`) or a fresh
  execution, never a duplicate. Evidence: `docs/quality/evidence/fault-injection-*.md`.

## Development-only switches (never in production)
`LEDGER_AUTH_MODE=dev-hs256`, `LEDGER_ALLOW_INSECURE_NON_LOOPBACK=allow-insecure-non-loopback-development-only`,
`LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only`, the compose
credentials, `deploy/postgres-init/` (development runtime role with a baked password).
The server refuses the unvalidated-acceptance switch together with production auth.

## Required gates before a release
`./scripts/check-fast.sh`, `./scripts/check-supply-chain.sh`, the real PostgreSQL suites
(including `pg_least_privilege`), `./scripts/test-integration.sh`, `./scripts/fuzz.sh`
(bounded, also in CI); per release the qualification runs `scripts/stress.sh`,
`scripts/fault.sh`, `scripts/backup-restore.sh`, `scripts/upgrade.sh` and `scripts/bench.sh`
(baselines in `docs/quality/performance-baselines.md`), each recorded in the active plan.
The live identity-provider smoke test is a release prerequisite as long as it is pending.
