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

### Slice 3 — 1,000-writer gate (§2, 2026-09-26): executed, PASS
- `apps/ledger-stress` (workspace binary, never shipped; refuses non-loopback targets) +
  `compose.stress.yaml` (second replica `ledger-b`, same runtime identity, port 8081, own
  compose project) + `scripts/stress.sh`. 1,000 concurrent writers (one HS256 subject each)
  against two containerised replicas chosen per request; every tenth writer duplicates each
  prepare/accept to a second replica under the same `Idempotency-Key`. A pair counts as
  *compared* only when both replicas answered (then exactly one original execution and
  identical durable fields are required); one side refused by admission control is
  reported separately; any other asymmetry is a disagreement. Phase A: 60 s time-boxed on
  one graph (worst case, all writers on one ref); phase B: 3 commits per writer over 100
  graphs. Pass conditions per phase: per-graph SQL equalities (`refs.version` =
  `count(ref_events)` = distinct versions = max version = accepted decisions = outbox rows
  = client-observed landings, each landing present in `ref_events` with the same version and
  head), no pair disagreement, no unexpected error class (anything but transport,
  `503 RESOURCE_LIMIT`, `503 DEPENDENCY_UNAVAILABLE`; a `DEPENDENCY_TIMEOUT` — how a 40P01
  deadlock surfaces — fails), no malformed response, `pg_stat_database.deadlocks` unchanged
  (read after the 10 s statistics flush interval), a minimum number of commits, every writer
  at its target (phase B), and the p99 of *successful* operations under a 5 s budget;
  then `ledger_store::verify` and `ledger-admin verify`.
- Evidence (`docs/quality/evidence/stress-1000-writers-2026-09-26.md`, rerun after the
  review fixes): contended 320,066 requests / 60.6 s (5,278 req/s), 282 commits, 13,686
  CAS conflicts, successful p99 ref-read 99 ms / prepare 327 ms / accept 112 ms (refused
  prepares p99 45 ms); independent 47,116 requests / 8.7 s, 3,000 commits (346/s),
  successful p99 ≤ 157 ms. Duplicated pairs: 188 + 371 compared with both answers (one
  original each, identical fields), 1,833 both-conflict, 1,981 one side refused, 16,802
  neither side answered; 0 disagreements. 0 deadlocks, 0 unexpected error classes, max 32
  runtime sessions (= 2 × pool 16), all landings found as ref events, verifier clean,
  `STRESS GATE OK`. `503 RESOURCE_LIMIT` (168,601) is the 12-slot expensive-operation
  admission control refusing immediately under overload (intended backpressure; a bounded
  queue is noted in tech-debt). Store-level concurrency is therefore capped at 2 × 12
  expensive slots and 32 sessions; "1,000 concurrent writers" is the client-side load.

### Slice 4 — kill fault injection (§3, 2026-09-26): executed, PASS
- `ledger-stress fault` + `scripts/fault.sh`: 200 sustained writers over 20 graphs; eight
  `docker kill -s KILL` of alternating replicas and three of PostgreSQL, each triggered after
  ≥30 observed new commits, `docker wait` before restart, recovery detected by `/ready`
  polling; replicas' `StartedAt` asserted unchanged across the PostgreSQL kills (pools
  reconnected within ≈2 s of crash recovery, no restart); `ledger-admin verify` after every
  recovery (11 × `VERIFY OK`). In every quiet window the writers pause, in-flight requests
  drain, and every in-doubt response (connection died while served, `503 DEPENDENCY_*` while
  PostgreSQL was down, or connection refused while a replica was down — reported apart as
  `never-sent`) is replayed verbatim (same key, body, actor) with retry/backoff and classified
  against the database: durable-before-crash (replayed, exactly one ref event), executed on
  retry (one event), refused `HEAD_CHANGED` (zero events); `LINEAGE_MISMATCH`, any other
  answer or an unresolved replay is inconsistent. Pass conditions: per-graph equalities
  incl. landings matched to `ref_events`, 0 inconsistent, no unexpected error class, no
  malformed response, verifier clean, and at least `--min-durable-accepts` observations
  (default 0: the count is reported, see below).
- Evidence (`docs/quality/evidence/fault-injection-2026-09-26.md`, rerun after the review
  fixes): 93,835 requests, 4,514 commits during the run, 1,967 in-doubt responses (33–83
  in-flight and 139–271 never-sent per server kill, 24–28 in-flight per PostgreSQL kill);
  replays: 3 accepts and 1 prepare durable before the crash (replayed identically, one ref
  event each), 549 accepts refused `HEAD_CHANGED` with zero ref events, 1,413 prepares
  `HEAD_CHANGED` on retry, 1 prepare executed on retry, 0 unresolved, **0 inconsistent**;
  Σ refs.version = 4,517 = Σ ref_events = accepted decisions = outbox rows = client-observed
  landings, all found as ref events; verifier clean; `FAULT GATE OK`. Honesty note: the
  durable-before-crash case is observed by chance (sub-millisecond window; an earlier
  30-kill run observed 0 accepts), so the deterministic proof of "lost response after COMMIT
  replays" remains the `FailPoint` unit test; a feature-gated crash switch in a separate
  qualification build would make it deterministic at the HTTP level (tech-debt).

### Slice 5 — multi-replica authentication and idempotency (§4, 2026-09-26): executed except the live issuer
- `pg_api::two_replicas_share_one_key_source_and_replay_identically_across_rotation`: two
  in-process replicas (own runtime pools and `OidcAuthenticator`s) over one real local JWKS
  endpoint on real PostgreSQL: concurrent identical prepare and accept on different replicas
  (one original, identical durable fields, one ref event), rotation to a new `kid` picked up
  by each replica on first sight, withdrawal of the old key taking effect after a refresh,
  `exp`/`nbf` boundaries inside and beyond the 30 s leeway (±25/±35 s pin the value), the
  accept replayed after rotation with the new key on the other replica (idempotency is bound
  to the complete actor, not the token), and — with the production refresh policy and an
  injected clock — a `kid` published after the last fetch refused until the 60 s refresh
  interval elapses, then accepted (documented rotation latency; tech-debt). Scope caveats:
  the replicas are in-process routers addressed explicitly (no shared address / load
  balancer) and rotation happens between steps, not under load; cross-replica replay at
  scale is the 559 both-answered duplicated pairs of slice 3 (containerised replicas,
  HS256), not the whole 21,000 pairs.
- **Live Entra ID issuer smoke test: PENDING** (no tenant or credentials available to
  these runs; it is not marked passed). Blocker for production qualification.

### Slice 6 — fuzzing (§5, 2026-09-27): executed, bounded run clean; ASan deferred
- `fuzz/` (own cargo-fuzz workspace, excluded from the root; nightly only for this job) with
  seven libFuzzer targets over every untrusted-input parser and canonical encoder:
  `quad_parse` (N-Quads single quad; canonical text is a fixed point), `patch_canonical`
  and `commit_decode` (v1 + v2; accepted bytes must re-encode identically, so one identity
  per object), `prepare_body` / `accept_body` (strict JSON → handler normalization →
  request identity, deterministic), `request_identity` (structured `arbitrary` bodies:
  operation order and duplicated evidence never change the identity), `timestamp` (canonical
  form satisfies the strict parser). Corpora seeded from the golden vectors (valid and
  invalid commits, requests, patches) and hand-written cases, then grown and minimized by
  libFuzzer (`cargo fuzz cmin`); committed under `fuzz/corpus/`. `scripts/fuzz.sh
  [seconds]` runs all targets and fails on any crash; `ci-fuzz` runs 45 s per target on
  every pull request and weekly and uploads logs and artefacts.
- Executed (2026-09-27, `scripts/fuzz.sh 60`, sanitizer `none`): 0 crashes across
  ≈87 M executions — quad_parse 8.4 M (cov 1621), patch_canonical 4.6 M (1720),
  commit_decode 13.7 M (636), prepare_body 9.1 M (2620), accept_body 17.0 M (859),
  request_identity 2.4 M (1661), timestamp 32.0 M (203). **Deferred:** the AddressSanitizer
  build crashes at start-up on the qualification host (SIGSEGV before the first input, all
  targets alike; the same binaries run when built without the sanitizer), so ASan runs are
  not claimed; the workspace forbids `unsafe`, so ASan would only observe dependencies.
  Validate `FUZZ_SANITIZER=address` on a compatible host and record a longer (hours) run
  before the final qualification decision.

### Slice 7 — upgrade from the previous release (§7, 2026-09-27): executed, PASS
- `scripts/upgrade.sh [rev] [commits]`: builds the previous release (`f027fbf`, the merged
  Phase-1 baseline at schema 0007; the repository has no tags yet) from git, runs it against
  a fresh PostgreSQL (owner URL, self-migrating), writes commits through its API recording
  keys/bodies/answers/states, then upgrades in the documented order (stop → owner
  `ledger-admin migrate --runtime-role` → new image as the runtime identity) and checks:
  schema at the required level with the checksums of the already-applied migrations
  untouched, runtime identity connected, identical head/version and states, verbatim replay
  of every old prepare/accept key (`replayed: true`, identical identifiers, ref unchanged),
  a new commit on top, `ledger-admin verify`, and clean-install vs upgraded schema
  convergence (`pg_dump --schema-only`, normalized, diff empty — grants included).
- Evidence (`docs/quality/evidence/upgrade-2026-09-27.md`): 25 commits before, schema
  7 → 9, 25 states identical, 50 keys replayed identically, version 26 after, verifier
  clean, schemas identical (592 lines); `UPGRADE OK`.

### Slice 8 — backup/restore smoke (§8, 2026-09-27): executed, PASS
- `scripts/backup-restore.sh`: under a sustained 50-writer load over 10 graphs, take
  `pg_dump -Fc` and `pg_basebackup -c fast -X stream` (local socket; a production
  deployment uses a `replication` role over TLS — runbook), restore the dump into a new
  database (`pg_restore --no-owner`) and start a second PostgreSQL instance from the base
  backup; for each: `ledger-admin verify` → `VERIFY OK`, every ref's `(version, head)`
  chain an exact contiguous prefix of the live chain, and a server on the restored database
  (runtime identity) serving the restored heads with matching versions and reconstructing
  states identical to the live server at the same commit ids. The load generator's own
  gate (invariants, landings matched to `ref_events`, verifier) applied to the live database.
- Evidence (`docs/quality/evidence/backup-restore-2026-09-27.md`): dump at 209 commits,
  base backup at 599, live at 861 when the load stopped (899 events at comparison time);
  dump restore 10 refs / 184 events, base-backup instance 10 refs / 313 events, both exact
  prefixes; 10 + 10 restored heads served, states identical; both verifies clean;
  `BACKUP RESTORE OK`. First attempt failed on the missing network replication entry in
  `pg_hba.conf` (procedure corrected to the local socket and documented).

### Slice 9 — adversarial resource limits (§9, 2026-09-26): executed
- `pg_api::expensive_operations_are_admission_controlled_under_a_slow_database`: owner
  holds `ACCESS EXCLUSIVE` on `immutable_objects`; two expensive reads occupy both slots
  (barrier: two runtime sessions observed waiting on the lock); the third read and a
  prepare get `503 RESOURCE_LIMIT` immediately, cheap ref reads and `/ready` still succeed;
  the blocked reads end in `503 DEPENDENCY_TIMEOUT` (PostgreSQL `lock_timeout`), slots are
  released, a new read blocks (not refused) and succeeds after the lock is released;
  runtime sessions never exceed the 16-connection pool (sampled at the peak of the episode
  and after it), 20 follow-up reads succeed; whole-process RSS is printed as a measurement
  only (other tests share the process), the memory evidence is the stress run.
- `pg_api::edge_timeout_slow_loris_and_body_boundary_are_bounded`: edge timeout fires as
  `RESOURCE_LIMIT` under a blocked database when the store's own timeout is longer; a
  slow-loris body is cut by the same timeout (through the router's middleware; the test
  uses `oneshot`, so socket-level connection closing is not exercised); exactly
  `body_bytes` is accepted and one more byte is refused before parsing. Depth beyond `max_depth` and states beyond `max_quads`
  were already covered by `resource_limits_are_enforced_with_a_stable_code` and the
  reconstruction tests. Both new tests run on throwaway databases (a table lock disturbs
  every session); the first version of the suite exposed exactly that interference.

### Review record — slices 3, 4, 5, 9 (storage/concurrency, test, security; Opus, read-only, on the first complete draft)
- No P0. P1s, all fixed before commit and the runs regenerated: (1) the duplicated-pair
  evidence counted pairs where one side never reached the store (`503 RESOURCE_LIMIT`) and
  accepted any one-sided failure → pairs are classified (`compared` / both-conflict /
  one-side-refused / both-failed / disagreement), only both-answered pairs are claimed, and
  a one-sided `409 IDEMPOTENCY_CONFLICT`, 500, 401 or `DEPENDENCY_TIMEOUT` is a
  disagreement (unit-tested); (2) server answers were only counted, never checked → every
  client landing `(version, head)` is joined to `ref_events`, head ≠ candidate is a
  malformed response, and any unexpected error class fails the run; (3) p99 mixed immediate
  refusals with successes and no bound was enforced → per-outcome histograms, successful
  p99 held to a budget; (4) the fault gate could pass without ever observing a lost response
  after COMMIT → the count is reported explicitly, replays run in every quiet window with
  retry/backoff, `--min-durable-accepts` can demand observations; the deterministic proof
  remains the `FailPoint` unit test (the COMMIT-to-response window is sub-millisecond, 30
  random kills produced 0–1 observations), stated as such rather than claimed.
- P2s fixed: loopback-only guard for replicas and the owner database (`--allow-non-loopback`
  override); the HTTP-level "two replicas" and slow-loris claims narrowed (in-process
  routers, `oneshot`); prepare pair checks the durable proposal row; pool bound sampled at
  the peak against the configured pool size; RSS demoted to a measurement; harness setup
  migrates the shared database before granting (test-order independence); production
  refresh-throttle rotation test with an injected clock; `never-sent` (connection refused)
  separated from in-flight in-doubt responses; deadlock counter read after PostgreSQL's
  flush interval and unreadable → fail; `saturating_mul`; `docker wait` after each kill;
  replicas' `StartedAt` asserted unchanged across PostgreSQL kills (restart count was
  meaningless without a restart policy); isolated compose projects (`-p`), preflight for
  required tools, `--locked` builds, owner DSN via environment, container logs captured in
  the exit trap; `compose.stress.yaml` labelled as a non-deployment artefact; the busybox
  digest in `scripts/test-integration.sh` replaced by the registry's real digest and the
  unpinned fallback removed; quality-gates wording made consistent.
- Accepted / recorded in tech-debt: `accept` takes no expensive slot and an edge timeout
  releases the slot while the PostgreSQL statement runs on (bounded by session limits);
  rotation latency under the 60 s refresh throttle; a bounded admission queue as a possible
  refinement; the live Entra ID issuer test remains pending.

### Slice 10 — performance baselines (§10, 2026-09-27): first run recorded, depth 10,000 in progress
- `ledger-stress bench` + `scripts/bench.sh`: single client, linear history, prepare / accept /
  ref read / state read at depths 1, 100, 1,000, 10,000 (20 samples each). First run aborted
  at depth 5,902 by client-token expiry (the build takes hours because prepare reconstructs
  the parent state; fixed with a 12 h token) after recording depths 1–1,000:
  accept ≈3–4 ms and ref read ≈1 ms flat; prepare 4.8 → 20.5 → 194.7 ms p50 and state read
  11 → 30 → 211 ms p50, i.e. ≈0.19 ms per ancestor commit. `docs/quality/performance-baselines.md`
  holds the table and the checkpoint policy proposal (content-addressed snapshots every k
  commits, written after COMMIT, verifiable by digest, cache-only for correctness; ADR needed
  before Phase 4). The depth-10,000 rerun is running; its row is appended when it completes.

## Qualification decision (§22–23, interim, 2026-09-27)
**Production-qualified: NO.** Executed and passing: slices 1 (least privilege), 2 (supply
chain, image), 3 (1,000 writers), 4 (kill injection), 5 except the live issuer, 6 bounded
fuzzing, 7 (upgrade), 8 (backup/restore), 9 (adversarial limits), §20 verifier, §21 runbook.
Open before the gate can pass: **live Entra ID issuer smoke test (pending, no tenant
credentials available to the runs)**; AddressSanitizer fuzz runs and a multi-hour fuzz
campaign (deferred: ASan runtime crashes at start-up on the qualification host); depth-10,000
baseline row (running); final independent reviews of slices 6–10 with no open P0/P1. Deferred
design work recorded in tech-debt (checkpoints, admission budget for accept, cancellation of
abandoned statements, deterministic post-commit crash switch).

## Sub-agent decomposition (§42)
storage/concurrency (role split, kill injection), API/security (multi-replica auth,
adversarial limits, live issuer), testing/fault-injection (1,000-writer harness, upgrade,
backup/restore), performance (baselines), supply-chain (audit/deny/SBOM/scan). Protocol
and canonicalization files stay owned by the main session and are not touched.
