# Plan 0005: P1.5 — production qualification of the Phase 1 ledger

Status: **planned, not started** (created 2026-09-26 at P1.4 closure). Implementation begins
only after Plan 0004's P1.4 evidence is accepted. Nothing in this plan is implemented yet;
no gate below may be reported as passed until it is executable and has run.

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
6. **Supply chain.** `cargo audit`, `cargo deny` (licenses, bans, advisories, sources),
   SBOM (CycloneDX) for the workspace and the container image, container image scan;
   findings classified (fix / accept with reason / false positive), never ignored.
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

## Sub-agent decomposition (§42)
storage/concurrency (role split, kill injection), API/security (multi-replica auth,
adversarial limits, live issuer), testing/fault-injection (1,000-writer harness, upgrade,
backup/restore), performance (baselines), supply-chain (audit/deny/SBOM/scan). Protocol
and canonicalization files stay owned by the main session and are not touched.
