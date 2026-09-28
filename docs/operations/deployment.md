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
   — applies migrations 0001…0010 on one dedicated owner connection (60 s lock timeout,
   so a running replica or a held migration lock fails loudly instead of hanging), grants
   the role (idempotent; the role must be a plain identifier, must not be a superuser and
   must not hold `CREATE` on the schema), then prints `schema at 0010 (required 0010)`.
   The owner identity should itself not be a superuser in production (an ordinary database
   owner suffices; `pg_least_privilege` exercises that shape). If a database was populated
   through `ledger-admin migrate-fs-to-pg`, pass `--runtime-role` there too or run
   `migrate --runtime-role` afterwards.
3. Provision graphs: `ledger-admin graph create --graph <id> --tenant <id> --status active`.
4. Start the servers with `LEDGER_DATABASE_URL` (runtime role), `LEDGER_AUTH_MODE=oidc`,
   issuer/audience/JWKS, limits. Startup connects, applies the session limits, **verifies**
   the schema is exactly 0010 with contiguous, checksum-matching history and every
   integrity trigger enabled, and refuses otherwise (behind: "run ledger-admin migrate";
   ahead: "deploy a newer build"; absent/corrupt metadata or a disabled guard: refuse), then
   verifies its own identity (not a superuser, not the owner, no `CREATE`, exactly the
   runtime grants) and refuses with `RUNTIME_IDENTITY` otherwise. `/ready` repeats the
   schema verification live. Connect directly or through a session-mode pooler: the session
   limits are set per connection.
5. Terminate TLS in front of the service; bearer tokens travel in clear otherwise.
6. Semantic validation (Phase 2): configure `LEDGER_VALIDATOR_URL` (https),
   `LEDGER_VALIDATOR_SERVICE_ID` and optionally `LEDGER_VALIDATOR_TOKEN_FILE`; map the
   validator workload's role to `validate` (`LEDGER_AUTH_ROLE_MAP`, default `ledger.validate`)
   and reviewers to `review`. Without a validator the service runs, acceptance stays
   fail-closed. The Sculpin prerequisites (aggregate KB revision, content-identifying
   ontology/shape versions, Virtual A-Box identification, the endpoint, publishing the current
   environment) are listed in `docs/design/sculpin-validation-service.md`.

## Upgrades
1. Take a backup first (below). There is no rollback: once 0008/0009 are recorded the
   previous binary refuses to start (`VersionMissing`), so the only way back is a restore,
   which loses everything written after the upgrade.
2. Run `ledger-admin verify` against the current database and fix anything it reports;
   migration 0009 validates every stored object hash under `ACCESS EXCLUSIVE` and aborts
   on a corrupt row without naming it.
3. Stop every replica (0007 note in `migrations/README.md`).
4. Upgrading from Phase 1 (single identity): create the runtime role with a managed
   password (`CREATE ROLE <role> LOGIN PASSWORD …`; it owns nothing and has no `CREATE`),
   and prepare the replicas' `LEDGER_DATABASE_URL` to use it — the owner URL moves to
   `LEDGER_MIGRATION_DATABASE_URL` on the operator host only. Rotate the owner password
   afterwards: Phase-1 replicas held it in their environment and secret stores. Confirm the
   runtime role has `CONNECT` on the database.
5. Run `ledger-admin migrate --runtime-role <role>` with the new `ledger-admin` under the
   owner identity.
6. Start the new build. A build started against another schema level, or with an identity
   that owns tables, refuses to serve; nothing upgrades implicitly.

The path from the previous release is exercised by `scripts/upgrade.sh`
(previous image built from git, data written through its API, upgrade in this order, then:
schema at the required level with the checksums of already-applied migrations untouched,
identical heads/versions and reconstructed states, verbatim replay of the previous
release's idempotency keys, new writes, `ledger-admin verify`, and a `pg_dump --schema-only`
diff between the upgraded database and a clean install plus an object-ownership comparison,
both of which must be empty — role attributes, database-level ACLs, sequence values and seed
rows are not covered). Run it before every release; its evidence is recorded in the active
Plan 0005 document. The Phase-2 transition from the P1.5 release (schema 0009 → 0010) is
exercised by `scripts/upgrade-p2.sh` (additionally: `pg_dump -Fc` backup restored and
compared, byte-identical pre-upgrade rows, runtime write probes, validation and validated
acceptance on upgraded graphs, ahead/behind refusal of both binaries, migration 0010's
pre-existing-validation-id guard; evidence in Plan 0006). Always run `ledger-admin verify`
from the same build as the servers: the P1.5 verifier does not check the schema level.

Phase 2 validator configuration: set `LEDGER_VALIDATOR_SERVICE_ID` (the trusted validation
service; required with production authentication) and, to allow new validations,
`LEDGER_VALIDATOR_URL` (+ optional `LEDGER_VALIDATOR_TOKEN_FILE`). Removing only the URL
during a validator outage keeps earlier trusted validations acceptable (ADR-0019
amendment).

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
  under writes). Restore with `pg_restore --no-owner --no-acl` into a new database as the
  owner identity, `GRANT CONNECT` on it to the runtime role, then run `ledger-admin migrate
  --runtime-role <role> --database-url <owner url of the restored database>` so the runtime
  grants are re-derived from migration 0008 rather than trusted from the dump (the role is
  cluster-wide; it must exist in the target cluster), and `ledger-admin verify` on it must
  print `VERIFY OK` before any server is pointed at it.
- **Physical backup:** `pg_basebackup -c fast -X stream` by a role with `replication`
  privilege (the development compose has no network replication entry in `pg_hba.conf`;
  production grants it to a dedicated backup role over TLS). A new instance started from
  the copy is verified the same way.
- **What restore preserves:** every ref's event chain is an exact prefix of the live chain
  as of the snapshot, and a server on the restored database serves the restored heads and
  reconstructs states identical to the live server for the same commit ids (content
  identity makes the comparison exact). `scripts/backup-restore.sh` exercises both variants
  under live load with development settings and checks these properties (verifier clean,
  graph set, pre-backup watermark, exact prefixes, audit rows present in live, identical
  states, no PUBLIC execute on ledger functions); run it per release.
- **Restore procedure (ADR-0017):** (1) stop every replica and projection consumer (no
  writer may touch old and new database); (2) restore (PITR to the last committed transaction
  where WAL archiving is configured — required in production; otherwise the dump or base
  backup); (3) `GRANT CONNECT`, `ledger-admin migrate --runtime-role <role>` on the restored
  database, `ledger-admin verify` → `VERIFY OK`; (4) record the declared restore point
  (`SELECT graph_id, branch, version, head FROM refs`) with the backup identity and reason;
  (5) reset or rebuild every projection to that point: `ledger-admin migrate` also takes
  `--projector-role <role>`, then `ledger-projector rebuild --graph <id>` per stream (their
  markers are now ahead of the restored heads: `MARKER_AHEAD` until rebuilt); a restored
  **copy** that runs next to the original (staging, forensics) must use its own
  `LEDGER_PROJECTION_TARGET_ID` and dataset, never the original's — the dataset binding
  keys on the target id only (ADR-0020); (6) start the replicas; (7) publish the restore
  point to clients.
- **What restore does not preserve — decide before you need it:** everything acknowledged
  after the snapshot is gone, and the restored ledger will issue the same `(graph, branch,
  version)` numbers again for different commits. Before a restore: fence all writers, record
  the restore point (last restored version per ref), rebuild any projection (Phase 3
  consumers may have projected commits that no longer exist), and expect clients holding a
  lost head to receive `HEAD_CHANGED`; idempotency keys issued after the snapshot will run
  fresh. The physical backup has no WAL archive here, so the recovery point is the backup
  time; whether PITR/WAL archiving is required is an open deployment decision recorded in
  `docs/exec-plans/tech-debt.md`.
- **Failure recovery:** a killed replica is restarted; a killed PostgreSQL recovers from
  its WAL and running replicas reconnect without restart (`/ready` returns 200 again).
  After any recovery run `ledger-admin verify`; clients retry ambiguous requests with their
  original `Idempotency-Key` and receive the durable outcome (`replayed: true`) or a fresh
  execution, never a duplicate. Evidence: `docs/quality/evidence/fault-injection-*.md`.

## Accepted-state projection (Phase 3, ADR-0020/0021)
Readers' contract: [reading the projection](../design/sculpin-projection.md).
- **Target**: an Apache Jena Fuseki (or other SPARQL 1.1 Protocol server) dataset that is
  transactional with abort — TDB2 or the transactional in-memory dataset — exposing named
  `query` and `update` endpoints only (`deploy/fuseki/ledger-projection.ttl`), **without** a
  union default graph, updates protected by credentials over https. Give the projector a
  dedicated update account and make it the only writer of the dataset's cognitive and
  marker graphs (the marker is trusted as far as the target is). The projector proves the
  dataset rolls a failed update back (probe at start-up and every
  `LEDGER_PROJECTOR_PROBE_SECONDS`, default 300; claiming pauses and `/ready` fails while it
  fails) and binds the dataset to its `LEDGER_PROJECTION_TARGET_ID` on first use: a dataset
  bound to another target id refuses the projector (`TARGET_CONFLICT`). One dataset, one
  target id.
- **Identity**: create the role, then `ledger-admin migrate --runtime-role <runtime>
  --projector-role <projector>` (distinct roles). The projector connects with it and refuses
  to start unless the role holds exactly the projector model (ADR-0021 / ADR-0016 amendment).
- **Enable a stream** (owner): `ledger-admin projection enable --graph <id> --target
  <target_id>`. Refused unless the graph is `active`, has a `knowledge_base_id`, the ref is
  `main` (v1) and its head was reached by an accepted change (a bootstrap/imported head has
  no projection event: accept a change first). The cognitive graph is
  `urn:sculpin:kb:<pct(kb_id)>:cognitive`; no two non-disabled streams of a target may share
  one. Existing outbox backlog is consumed as soon as the stream is enabled. `projection
  disable` (owner only — the projector role cannot enable or disable) sets `disabling`: a
  running projector fences the target (rotates the marker's write id so no write of that
  stream still in flight can land), then the stream is `disabled` and its cognitive graph is
  free — so a projector must be running for a disable to complete (`projection status`
  shows it); `--unfenced` disables at once when the target is gone for good, waiving that
  guarantee; to switch a KB's feed graph: disable the old stream, enable the new one, then
  `ledger-projector rebuild` it (the target holds the old feed's marker until then:
  `TARGET_CONFLICT`); a write of the old stream still in flight cannot land afterwards
  (ADR-0020 compare-and-swap). Re-enabling a disabled stream reactivates it with its
  original cognitive graph; if the graph's `knowledge_base_id` changed meanwhile, enable is
  refused (a stream's cognitive graph never changes). `ledger-admin` must connect as the
  role that **owns** the ledger tables itself (not a member of it, not a superuser): the
  guard compares `current_user` with the table owner for every enable/disable.
- **Run** `ledger-projector` (one or more replicas per target; they partition work by
  stream lease) with `LEDGER_PROJECTOR_DATABASE_URL`, `LEDGER_PROJECTION_TARGET_ID`,
  `LEDGER_PROJECTION_QUERY_URL`, `LEDGER_PROJECTION_UPDATE_URL` (https) and credentials from
  regular files (`LEDGER_PROJECTION_USERNAME` + `LEDGER_PROJECTION_PASSWORD_FILE`, or
  `LEDGER_PROJECTION_TOKEN_FILE`). `LEDGER_PROJECTOR_LEASE_SECONDS` (default 180) must be at
  least 4 × `LEDGER_PROJECTOR_TARGET_TIMEOUT_SECONDS` + 30 (refused otherwise).
  `LEDGER_PROJECTOR_RECONCILE_SECONDS` (default 300) is how often an idle stream is
  re-observed and repaired if the target lost or changed it. `/health`, `/ready` (schema,
  identity, probe) and `/metrics` bind `LEDGER_PROJECTOR_ADDR` (default loopback).
- **Observe**: `ledger-admin projection status [--target <id>] [--json]` (head vs projected
  version, lag, pending, oldest pending age, last success, last error, rebuilds, lease) and
  metrics `projection_lag_versions`, `projection_pending`,
  `projection_oldest_pending_seconds`, `projection_failures_total{class}`,
  `projection_failures_by_code_total{code}`, `projection_rebuilds_total`,
  `projection_lease_lost_total`, `projection_superseded_total`,
  `projection_duration_seconds`. Alert on lag growth and on any `blocked` /
  `rebuild_required` stream.
- **Failures**: retryable target or database errors back off (1 s doubling, 5 min cap,
  jitter); a permanent one (auth, protocol, named-graph state, size) sets the stream
  `blocked`; a marker ahead of the ledger (e.g. after restoring an older ledger backup) or a
  marker of another stream (`TARGET_CONFLICT`) sets `rebuild_required`. Acceptance is never
  affected. After fixing the cause: `ledger-projector rebuild --graph <id>` (replaces the
  cognitive graph with the accepted state at the ref head — a compare-and-swap on what it
  observed, so it can never overwrite something that changed meanwhile — verifies marker and
  containment, reactivates the stream);
  `ledger-projector verify --graph <id>` compares target and ledger content without writing
  (the only check that also finds a count-preserving out-of-band edit). A stream blocked with
  `TARGET_PROTOCOL` "cannot be named exactly" holds a hand-edited marker the write
  precondition cannot express (blank node, > 64 values, unusual IRI or language tag):
  delete the marker subject by hand (`DELETE WHERE { GRAPH
  <urn:sculpin:ledger-projection:v1:markers> { <G> ?p ?o } }`), then rebuild. A Fuseki that lost
  its data is repaired by reconciliation or the same rebuild (the ledger is authoritative;
  nothing is ever read back into it).

## Named branches (Phase 4, ADR-0022)
- **Upgrade 0011 → 0012**: stop every replica and projector, take a backup, run
  `ledger-admin migrate --runtime-role <runtime> --projector-role <projector>` (no new role);
  every existing ref is adopted as an active branch with one `adopted` lifecycle event
  (principal `urn:sculpin:ledger:migration:0012`). A 0011 build refuses 0012 (ahead) and a
  0012 build refuses 0011 (behind); exercised end to end by `scripts/upgrade-p4.sh`.
- **Behaviour change**: a genesis acceptance creates only `main`. Any other ref must be
  created first with `POST /v1/graphs/{graph}/branches` (`name`, `source`, optional
  `from_commit` reachable from the source head, optional `policy`); an acceptance or prepare
  on an unknown non-`main` ref answers `404 BRANCH_NOT_FOUND`. Clients that relied on
  implicit ref creation (Phase 1–3) must add the create call.
- **Capabilities**: `read` lists/inspects (`GET …/branches`, `…/branches/status|history|log?name=`);
  `propose` creates unprotected branches; `admin` creates protected branches and deletes
  (`POST …/branches/delete`) or restores (`POST …/branches/restore`) any non-`main` branch.
  Map an operator group to `admin` in `LEDGER_AUTH_ROLE_MAP` (`ledger.admin`).
- **Deletion is a tombstone**: the head stops moving and no new proposal is admitted
  (`409 BRANCH_DELETED`); pending proposals may still be rejected; history, proposals and
  decisions stay readable; restore resumes at the same head/version. There is no hard delete
  and no GC. `main` can never be deleted.
- **Policy v1 is immutable** after creation: `require_validation` (acceptance needs a
  matching validation even where the deployment allows unvalidated acceptance) and
  `require_distinct_reviewer` (the accepting principal must differ from the proposer).
  A branch's policy can only tighten the deployment floor.
- **Projection** is still `main` only. Branch acceptances write outbox rows (kept for later
  protocols) that are never delivered and never counted by `projection_unconfigured_pending`
  or `ledger-admin projection status`.
- `ledger-admin verify` additionally checks that every ref is a branch, lifecycle versions
  equal event counts, the latest event describes the current status, and a created branch
  starts at its recorded branch point. Qualification: `scripts/stress-branches.sh` (100
  branches, two replicas).

## Development-only switches (never in production)
`LEDGER_AUTH_MODE=dev-hs256`, `LEDGER_ALLOW_INSECURE_NON_LOOPBACK=allow-insecure-non-loopback-development-only`,
`LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only`, the compose
credentials, `deploy/postgres-init/` (development runtime and projector roles with baked
passwords), `deploy/fuseki/development-admin-password`,
`LEDGER_PROJECTOR_DEVELOPMENT=allow-insecure-development-only` (plain http to a loopback
target, no target credentials).
The server refuses the unvalidated-acceptance switch together with production auth.

## Required gates before a release
`./scripts/check-fast.sh`, `./scripts/check-supply-chain.sh`, the real PostgreSQL suites
(including `pg_least_privilege`), `./scripts/test-integration.sh`, `./scripts/fuzz.sh`
(bounded, also in CI); per release the qualification runs `scripts/stress.sh`,
`scripts/fault.sh`, `scripts/backup-restore.sh`, `scripts/upgrade.sh` (and the phase upgrades `scripts/upgrade-p2.sh`, `upgrade-p3.sh`, `upgrade-p4.sh`), `scripts/stress-branches.sh` and `scripts/bench.sh`
(baselines in `docs/quality/performance-baselines.md`), each recorded in the active plan.
The live identity-provider smoke test (`scripts/live-issuer-smoke.sh`, configuration in the active plan) is a release prerequisite as long as it is pending.
