# Plan 0005: P1.5 — production qualification of the Phase 1 ledger

Status: **in progress** (started 2026-09-26 after PR #1 merged). Branch
`claude/p1.5-production-qualification` from `main` at `f027fbf9f06c6c642dca16a0caf9746571fd76a7`
(the PR #1 merge). Execution slices, in order: (1) least privilege and migration separation
(ADR-0016, migration 0008, `ledger-admin migrate`, verify-only startup, `pg_least_privilege`);
(2) supply chain (cargo-deny, pinned Actions, SBOM, image scan); (3) 1,000-writer stress;
(4) crash/fault injection; (5) multi-replica auth/idempotency and live issuer; (6) fuzzing;
(7) adversarial resource limits; (8) upgrade qualification; (9) backup/restore; (10)
performance baselines; then the qualification decision. No gate below is reported as passed
until it is executable and has run; status per slice is recorded under "Evidence".

## Goal
Turn the functionally complete Phase 1 service (shared PostgreSQL persistence, atomic
workflow, authenticated graph-scoped API) into a deployment that an operator may run
outside a development network: least-privilege database roles, no schema mutation from the
request path, demonstrated behaviour under process/container failure and concurrent load,
supply-chain and upgrade evidence, and measured performance baselines. Until this plan's
gate passes the service is **not production-qualified** (Plan 0004 P1.4 report).

## Scope
1. **PostgreSQL role split and migration removal from the runtime path.**
   Owner/migration role runs DDL through `ledger-admin migrate` (new subcommand wrapping
   `ledger_store::schema::migrate_all`); the runtime role has `SELECT`/`INSERT` on
   immutable and audit tables, `SELECT`/`INSERT`/`UPDATE(head, version, updated_at)` on
   `refs`, `UPDATE(delivered_at, attempts)` on `projection_outbox`, and no `ALTER`,
   `DISABLE TRIGGER`, `UPDATE`/`DELETE` on write-once rows. `PostgresLedgerStore::connect`
   stops migrating; startup verifies the schema version and refuses to serve a database
   that is behind or ahead. Runtime role gets `statement_timeout` and
   `idle_in_transaction_session_timeout`. Needs an ADR (persistent atomicity/identity
   surface: who may change the schema).
2. **1,000-writer gate.** Real PostgreSQL, ≥1,000 concurrent prepare/accept clients across
   ≥2 server replicas against one graph and across many graphs: every ref version is
   consumed exactly once, `ref_events` count equals `refs.version`, no duplicate
   publication under lost responses, no deadlock, bounded p99 latency recorded.
3. **Process and container kill fault injection.** SIGKILL the server mid-transaction and
   `docker kill` PostgreSQL during sustained writes; verify no partial acceptance (decision
   without ref move, outbox without decision), replay after restart, and that the
   `FailPoint`-based unit evidence agrees with real crashes.
4. **Multi-replica authentication and idempotency stress.** Two replicas behind one
   address: JWKS rotation mid-run (new `kid`), token expiry boundaries, concurrent identical
   retries landing on different replicas replay identically; live-issuer smoke test against
   a real Entra ID tenant (dev tenant) for the OIDC path (P1.4 tests the OIDC
   authenticator only against a local JWKS server).
5. **Fuzzing.** `cargo fuzz` targets for the N-Quads/quad parser, patch canonicalization,
   commit v1/v2 decoding, request-body deserialization and the request identity encoder;
   corpora checked in; a bounded run in CI, longer runs recorded as evidence.
6. **Supply chain.** `cargo deny` (licenses, bans, advisories, sources) alongside the
   already-blocking `cargo audit` gate (`scripts/check-supply-chain.sh`; re-evaluate the
   RUSTSEC-2023-0071 lockfile-only exception on every sqlx move), SBOM (CycloneDX) for the
   workspace and the container image, container image scan, and pinning every third-party
   GitHub Action by immutable commit SHA; findings classified (fix / accept with reason /
   false positive), never ignored.
7. **Upgrade from the previous release.** Build the last tagged image, load data through
   its API, upgrade to the new image + migrations, verify history, refs, idempotency
   replay and reads; clean install vs upgrade schema convergence for every migration.
8. **Backup/restore smoke.** `pg_dump`/`pg_restore` (and a base-backup variant) of a live
   database; the restored database serves identical heads and reconstructed states and
   passes the DB invariant queries.
9. **Resource-limit adversarial tests.** Includes the expensive-operation semaphore
   (`RESOURCE_LIMIT` on saturation, which P1.4 left without an executable test) and the
   request timeout under a slow database rather than a slow authenticator. Oversized bodies at the transport limit boundary,
   deep ancestry chains beyond `max_depth`, large states beyond `max_quads`/`max_bytes`,
   many concurrent expensive reads beyond the semaphore, slow-loris request bodies against
   the timeout; each yields `RESOURCE_LIMIT` (or connection close) without memory growth or
   connection-pool exhaustion measured over the run.
10. **Performance baselines.** Reproducible benchmark harness (prepare, accept, ref read,
    state read at depth 1/100/10,000) with recorded hardware, dataset and numbers;
    checkpoint policy proposal if reconstruction depth dominates (Phase 4/5 input).

## Non-goals
Semantic validation (Phase 2), projection (Phase 3), branch policies (Phase 4), merge
(Phase 5), checkpoints/snapshots as a feature, any protocol or golden-vector change, any
graph lifecycle transition or HTTP graph administration API.

## Invariants
All Phase 0/1 invariants (immutable commits, content identity, verified index, same-graph
ancestry, CAS refs with monotonic versions, one-transaction acceptance, complete-actor
idempotency, tenant isolation, fail-closed acceptance without validation). This plan adds
executable evidence; it changes no invariant.

## Migration impact
No content migration. A role/grant migration (new file 0008, additive) creates the runtime
role grants; existing 0001–0007 are untouched. Deployments must switch the server's URL to
the runtime role and run `ledger-admin migrate` with the owner role before starting the
new image.

## Affected crates
`ledger-store` (connect without migrate, schema-version check), `apps/ledger-server`
(`ledger-admin migrate`, startup refusal), new `fuzz/` targets, `scripts/` (stress, fault,
supply-chain, upgrade, backup harnesses), `docs/` (operations runbook, security).

## Acceptance evidence (all executable; deferred ≠ pass)
- Role split: integration test connecting as the runtime role proves each forbidden
  statement is refused by PostgreSQL and every public API operation still succeeds; server
  refuses a stale schema.
- 1,000-writer run log with the invariant queries and latency percentiles.
- Kill-injection run log (server SIGKILL ×N, PostgreSQL kill ×N) with invariant queries
  after each recovery.
- Multi-replica auth/idempotency run log incl. JWKS rotation; live-issuer smoke log.
- Fuzz targets built and run for a recorded duration with zero crashes; corpora committed.
- `cargo audit`/`cargo deny` clean or classified; SBOM and image-scan artefacts attached.
- Upgrade and backup/restore logs with matching heads and state digests before/after.
- Adversarial limit run with memory/pool metrics.
- Benchmark report committed under `docs/quality/performance-baselines.md`.
- Independent security, storage/concurrency, and test reviews with no open P0/P1.

## Evidence

### Slice 1 — least privilege and migration separation (2026-09-26): complete; all gates green on the reviewed content
- ADR-0016; migration 0008 (`ledger_grant_runtime(role)`: owner-only, pinned `search_path`,
  revoke-then-grant, column-level INSERT, refuses superusers and CREATE-holders, EXECUTE
  revoked from PUBLIC, no role creation); migration 0009 (audited fast-forward-only ref
  movement, serialized status changes, content-addressed objects, `ledger_lock_key`);
  `schema::verify` (exact `REQUIRED_SCHEMA_VERSION = 9`, contiguity, checksums against the
  embedded migrations, failed/unknown/absent metadata and disabled guard triggers refused)
  and `schema::verify_runtime_identity` (not superuser/owner/CREATE-holder, exact privilege
  matrix; `RUNTIME_IDENTITY` refusal); SHA-256 advisory-lock keys; `PostgresLedgerStore::connect`
  and `PostgresImmutableStore::connect` are verify-only with `DbSessionLimits`
  (`statement_timeout` 30 s, `lock_timeout` 10 s, `idle_in_transaction_session_timeout` 60 s,
  configurable `LEDGER_DB_*`); `connect_and_migrate`/`from_pool` documented tests-and-tooling
  only; `ledger-admin migrate [--runtime-role]` on a dedicated owner connection
  (`LEDGER_MIGRATION_DATABASE_URL`), `graph create` and the cutover moved to the owner URL and
  verify instead of migrate; runtime row locks that need `UPDATE` privilege replaced by
  advisory locks (`graph-status:` shared; `proposal-decision:` exclusive in
  bound_undecided_proposal and mark_superseded); `DependencyTimeout` (57014/55P03) and
  `SchemaIncompatible` errors mapped to retryable 503s; compose runs PostgreSQL → one-shot
  `migrate` (owner) → server (runtime role from `deploy/postgres-init`), and the integration
  script provisions graphs through the owner service and asserts the runtime container holds
  no owner URL and cannot `DISABLE TRIGGER`.
- Executed on the final, review-fixed content (real PostgreSQL 17.2, fresh volumes):
  `./scripts/check-fast.sh` exit 0; `./scripts/check-supply-chain.sh` exit 0; `pg_cas_race`
  1, `pg_immutable_store` 9, `pg_graphs_migration` 7, `pg_fs_migration` 8, `pg_workflow` 14,
  `pg_api` 10 (served store proven to run as `ledger_rt_api`, a granted non-superuser role),
  `pg_least_privilege` 5 — whole workflow incl. replay/accept/reject/supersede and reads
  under a per-test runtime role proven by `current_user`/`rolsuper`; 39 denied statements
  (DDL, trigger disable, TRUNCATE, every UPDATE/DELETE on immutable and audit tables, graph
  provisioning, `session_replication_role`, `setval`, `protected=false` refs, pre-delivered
  outbox rows, back-dated decisions) each refused with 42501 while a full-table snapshot
  stays identical; a pending migration refused for the runtime role with 42501 and not
  recorded; lock_timeout through `prepare` → `DependencyTimeout` with no idempotency row and
  a fresh retry; idle-in-transaction termination and pool recovery; statement_timeout →
  57014; privilege matrix via `has_table/any_column/sequence/schema_privilege` exactly as
  documented, column-level INSERT excluding ids/timestamps/`protected`/`delivered_at`;
  grant function owner-only, idempotent, refuses unknown roles, superusers and CREATE
  holders; startup/readiness refuse absent, behind (0007 → "requires 0009"), ahead (9999),
  tampered checksum, failed record, non-contiguous history, a disabled guard trigger, and
  the owner identity (`RUNTIME_IDENTITY`); a NOSUPERUSER/NOCREATEROLE database owner can
  migrate, grant and provision while being refused as runtime; migration 0009 rules hold
  even for the owner (rewind without event refused, forged event with a non-descendant head
  refused, mislabelled object refused, status change waits on the graph-status lock with
  55P03, archived graph refused by the workflow, SQL/Rust lock-key derivation identical) —
  54 passed, 0 failed. `./scripts/test-integration.sh` exit 0 `INTEGRATION OK`: owner
  `migrate` service reports `schema at 0009` and the grant; server sessions are
  `ledger_runtime` with no owner session; the runtime container holds no owner credentials;
  `ALTER TABLE … DISABLE TRIGGER` as the runtime role fails with "must be owner"; `/ready`
  answers 503 while a future migration row exists; a server started against a
  never-migrated database and one started with the owner URL both exit non-zero with the
  actionable reason; the v2 prepare/accept/replay/restart/read scenario and its SQL
  invariants pass as before. Development note: editing the unreleased 0008 while a dev
  database had already applied it produced the intended "previously applied but modified"
  refusal; the throwaway compose volume was reset.
- Reviews (storage/concurrency, security, invariant, test; Opus, read-only, on the first
  complete draft): no P0. Agreed P1 decisions, all implemented before commit: (1) a runtime
  holding `UPDATE (head, version)` on `refs` could move a ref to any indexed commit without
  an event → migration 0009 deferred constraint trigger (matching `ref_events` row in the
  same transaction, fast-forward only; bootstrap/importing exempt for the owner raw path);
  (2) `ledger_grant_runtime` ran unqualified with the owner's `search_path` and never
  checked `CREATE` on the schema (CVE-2018-1058 pattern) → pinned `search_path`,
  schema-qualified names, refuses superusers / CREATE-holders / non-owner callers;
  (3) the server never checked its own privileges, so a Phase-1 owner URL would keep
  serving with owner rights → `verify_runtime_identity` at startup (`RUNTIME_IDENTITY`
  refusal; process-level check in the Docker harness). P2s implemented: `BEFORE UPDATE OF
  status` trigger taking the exclusive graph-status lock (raw operator SQL can no longer
  interleave), advisory-lock keys from SHA-256 instead of `hashtextextended` (chosen
  `Idempotency-Key` collision search), column-level INSERT grants matching the store's
  INSERT statements (no back-dated rows, no `protected=false` refs, no pre-delivered
  outbox rows), revoke-then-grant so the set is exact, `mark_superseded` takes the shared
  graph-status lock and checks graph ownership before locking, contiguity and enabled-
  trigger checks in `verify`, 42501 mapped to an actionable "not granted" error,
  `DEPENDENCY_TIMEOUT` as a distinct API code (40001/40P01 retryable too), session limits
  on the immutable store, bounded `LEDGER_DB_*` values, `--runtime-role` validated as a
  plain identifier and never echoed, `ledger-admin migrate` lock timeout, cutover
  `--runtime-role`, compose readiness probe on `/ready` and TCP `pg_isready`, content-
  addressed CHECK on `immutable_objects`. Tests added for every item (privilege matrix via
  `has_*_privilege`, snapshot invariance across 39 denied statements, pending-migration
  refusal with 42501, lock/idle timeouts through the repository, failed/gap/disabled-
  trigger/owner-identity refusals, non-superuser owner path, 0009 integrity, HTTP suite
  under a granted runtime role). Documented accepted residual risk: the runtime remains the
  trusted writer of new audit rows (fabricated consistent forward moves within its tenants;
  `SECURITY DEFINER` write path is tech-debt); session limits require a direct or
  session-mode connection.

### Slice 2 — supply chain and build artefacts (2026-09-26): complete; all gates green on the reviewed content
- `deny.toml` (advisories on the resolved graph, permissive licence allow list, bans on
  Fluree / query engines / `rsa` / OpenSSL / native-tls, crates.io only, duplicates
  reviewed as warnings; workspace crates `publish = false`), `cargo deny check` wired into
  `scripts/check-supply-chain.sh` after the audit; CycloneDX 1.5 SBOMs for `ledger-server`
  and `ledger-admin` filtered to the feature-resolved build graph (205 components each; the
  script fails if `rsa`/`sqlx-mysql`/`sqlx-sqlite`/OpenSSL crates appear) generated by the
  script and uploaded by CI; every third-party Action pinned by SHA with release names and
  `persist-credentials: false`; Dependabot for Actions and Cargo (gated PRs only); builder
  and runtime images digest-pinned; new `ci-security` jobs `supply-chain` (audit + deny +
  SBOM) and `container` (image build, CycloneDX image SBOM and full JSON report uploaded
  before the gate, Trivy 0.74.0 pinned, gate on any CRITICAL/HIGH fixed or not, exceptions
  only via classified `.trivyignore`); runtime image moved from `debian:bookworm-slim` (106
  packages, 4 CRITICAL / 63 HIGH unfixed) to digest-pinned
  `gcr.io/distroless/cc-debian12:nonroot` (11 packages, ≈47 MB, 0 CRITICAL / 0 HIGH);
  `ledger-admin probe` replaces curl for health checks (parsed loopback-only URL, no
  userinfo, no proxy, no redirects, never echoed); compose and the integration script
  adapted to a shell-less container (env and user via `docker inspect`, volume resolved from
  the container's mounts, contents via a throwaway busybox); `.dockerignore` excludes
  credential-like files; findings classified in `docs/quality/security.md` (`.trivyignore`
  present but empty).
- Executed (2026-09-26): `./scripts/check-supply-chain.sh` exit 0 — rsa premise holds
  (`rsa 0.9.10` lockfile-only via `sqlx-mysql`), `cargo audit` clean (290 dependencies, 1
  documented exception), `cargo deny check advisories licenses bans sources` → "advisories
  ok, bans ok, licenses ok, sources ok" (9 duplicate-version warnings reviewed: base64,
  getrandom, hashbrown, rand, rand_chacha, rand_core, syn, untrusted, webpki-roots — all
  upstream-driven, none security-relevant), CycloneDX 1.5 SBOMs for `ledger-server_bin`
  and `ledger-admin_bin` (205 components each after filtering 32 lockfile-only crates,
  none of the forbidden ones present); `./scripts/check-fast.sh` exit 0 (incl. the probe
  URL bypass test); `./scripts/test-integration.sh` exit 0 `INTEGRATION OK` with the
  distroless image (probe-based health check, shell-absence and non-root user asserted,
  volume read via busybox; all eight PostgreSQL suites green; final `ledger-admin verify`
  → `VERIFY OK`). Local Trivy 0.74.0 scan of the rebuilt image: 0 CRITICAL, 0 HIGH, 17
  MEDIUM, 16 LOW, 1 UNKNOWN across 11 packages (classified per finding in
  `docs/quality/security.md`); the CI `container` job enforces the CRITICAL/HIGH gate on
  every push and pull request. Not executed here: the GitHub-hosted `ci-security` jobs
  themselves (they run on push; recorded when the branch is pushed).
- Review (security; Opus, read-only, on the first complete draft): no P0/P1. P2s
  implemented before commit: the SBOM named `rsa`/`sqlx-mysql` because cargo-cyclonedx
  reads `cargo metadata` → filtered against the resolved build graph with a hard failure
  on forbidden crates; the container job wrote into directories that did not exist on a
  fresh runner and ran the JSON report after the gate → `mkdir -p`, report and SBOM before
  the gate, uploads `if: !cancelled()`, scanner version pinned; `ignore-unfixed` would have
  silently dropped unfixed CRITICAL/HIGH → removed, exceptions only via classified
  `.trivyignore`; `probe` accepted `http://localhost.evil.com` / `http://127.0.0.1@evil.com`
  by prefix and echoed the URL → parsed loopback-only check, no proxy, bounded, never
  echoed, usage exit 1, bypass-string test; LOW count in the classification was off by one
  (libssl3 has 5 LOW, one unfixed) → corrected; the harness hardcoded the volume name →
  resolved from the container's mounts; builder image now digest-pinned; Dependabot added;
  `.dockerignore` credential patterns; `cognitive_ledger.zip` gitignored; OpenSSL/native-tls
  banned in `deny.toml`; `persist-credentials: false` on every checkout. Harness finding
  fixed in the same change: three tamper tests (`pg_immutable_store`, `pg_fs_migration`)
  seeded corrupt objects into the shared compose database, which `ledger-admin verify` then
  correctly reported → they run on throwaway databases (the verifier's first real catch).

### Continuous invariant verification (§20, 2026-09-26): implemented
- `ledger_store::verify` (22 read-only checks + table counts; inspect only) and
  `ledger-admin verify [--json]`; `pg_verify` proves a clean workflow history passes and
  that six representative bypasses (ref version drift, missing outbox, missing accepted
  decision, wrong parent count, broken event chain, dangling idempotency reference) are each
  detected inside a rolled-back owner transaction while counts stay identical; the Docker
  harness runs `verify` after the end-to-end scenario and requires `VERIFY OK`.

## Sub-agent decomposition (§42)
storage/concurrency (role split, kill injection), API/security (multi-replica auth,
adversarial limits, live issuer), testing/fault-injection (1,000-writer harness, upgrade,
backup/restore), performance (baselines), supply-chain (audit/deny/SBOM/scan). Protocol
and canonicalization files stay owned by the main session and are not touched.
