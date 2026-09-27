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

### Slice 6 — fuzzing (§5, 2026-09-27): executed, bounded runs clean under both sanitizers on the hosted runner
- `fuzz/` (own cargo-fuzz workspace, excluded from the root; nightly only for this job) with
  seven libFuzzer targets over every untrusted-input parser and canonical encoder:
  `quad_parse` (N-Quads single quad; canonical text is a fixed point), `patch_canonical`
  (the decoder itself refuses non-canonical bytes; crash-only in practice), `commit_decode`
  (v1 + v2; accepted bytes must re-encode identically, one identity per object),
  `prepare_body` / `accept_body` (strict JSON → handler normalization → request identity;
  the identity must not change under JSON key order, whitespace, operation or evidence
  order), `request_identity` (structured `arbitrary` bodies; asymmetric acceptance only when
  the duplicated evidence crosses the real metadata budget), `timestamp` (canonical form
  satisfies the strict parser). Corpora seeded from the golden vectors and grown/minimized
  by libFuzzer (`cargo fuzz cmin`), committed under `fuzz/corpus/` and used read-only by
  runs (new inputs go to `target/`). `scripts/fuzz.sh [seconds]` runs every declared target
  with debug assertions (`-a`) and fails on any crash or on an empty target list; `ci-fuzz`
  runs 45 s per target on every pull request and weekly with the pinned nightly.
- Executed (2026-09-27, `scripts/fuzz.sh 60`, sanitizer `none`, debug assertions on):
  0 crashes across ≈80 M executions — quad_parse 8.8 M (cov 1645), patch_canonical 4.2 M
  (1768), commit_decode 14.6 M (655), prepare_body 7.3 M (2891), accept_body 14.9 M (1017),
  request_identity 2.1 M (1718), timestamp 28.2 M (203). **Deferred:** the AddressSanitizer
  build crashes at start-up on the qualification host (SIGSEGV before the first input, all
  targets alike; the same binaries run without the sanitizer), so ASan runs are not claimed;
  the workspace forbids `unsafe`, so ASan would only observe dependencies. Validate
  `FUZZ_SANITIZER=address` on a compatible host and record a longer (hours) run before the
  final qualification decision. Not fuzzed: the `Idempotency-Key` header parser and the
  path `CommitId`.
- Corpus size (PR #2 review, 2026-09-27): `cargo fuzz cmin` on a copy of each checked-in
  corpus (pinned nightly, explicit `x86_64-unknown-linux-gnu`) removes 78 of 8,148 files
  (≈1 %) with identical coverage on every target (quad_parse 1621, patch_canonical 1720,
  commit_decode 636, prepare_body 2712, accept_body 950, request_identity 1661, timestamp
  203 edges); the corpora were already minimized when committed, so they are left unchanged
  rather than churned. Hand-written boundary seeds and golden-vector seeds are preserved in
  place; a crashing input would be added as a regression seed with its fix.
- Hosted runs (2026-09-27, after the workflow fix — explicit `rustup toolchain install
  nightly-2026-09-25 --component rust-src`, fuzz `--target` derived from `rustc -vV host`,
  never from the prebuilt cargo-fuzz binary's own platform, sanitizer matrix `none` /
  `address`, `workflow_dispatch` with `seconds_per_target` ≤ 1800 and `sanitizer`, weekly
  900 s campaign): on head `985e789` (run 36307859547) both matrix jobs passed with every
  declared target executed — `none`: quad_parse 9.1 M (cov 1708), patch_canonical 4.2 M
  (1777), commit_decode 9.5 M (645), prepare_body 7.7 M (2916), accept_body 14.2 M (1032),
  request_identity 2.1 M (1745), timestamp 27.7 M (203); `address`: quad_parse 3.6 M (cov
  2299), patch_canonical 1.5 M (2509), commit_decode 4.4 M (1008), prepare_body 2.9 M
  (4165), accept_body 5.1 M (1602), request_identity 0.7 M (2411), timestamp 11.2 M (397);
  no crash under either sanitizer; toolchain `nightly-2026-09-25 (rustc 1.100.0-nightly
  f7575a9da)`, target `x86_64-unknown-linux-gnu`, debug assertions on, 45 s per target.
  **The ASan start-up crash is therefore host-specific (local Debian/5.10 box); ASan
  fuzzing is no longer pending.** Long campaign (`workflow_dispatch`, run 36308371828, 900 s
  per target, both sanitizers, same toolchain/target): no crash across ≈1.56 G executions
  without sanitizer (accept_body 285 M, commit_decode 269 M, patch_canonical 42 M,
  prepare_body 118 M, quad_parse 148 M, request_identity 37 M, timestamp 664 M; cov
  1049/666/1994/3592/1750/2667/203) and ≈0.69 G under AddressSanitizer (78 M / 121 M / 19 M
  / 47 M / 70 M / 17 M / 340 M; cov 1623/1047/2873/5015/2345/3399/397) — 3.5 h of fuzzing in
  total; artefacts `fuzz-none` / `fuzz-address` on the run.

### Slice 7 — upgrade from the previous release (§7, 2026-09-27): executed, PASS
- `scripts/upgrade.sh [rev] [commits]`: builds the previous release (`f027fbf`, the merged
  Phase-1 baseline at schema 0007; the repository has no tags yet) from git, runs it against
  a fresh PostgreSQL from which the development runtime role was dropped (a Phase-1 cluster
  has a single identity), writes commits through its API recording keys/bodies/answers/states,
  then upgrades in the runbook order (stop → create the runtime role → owner `ledger-admin
  migrate --runtime-role` → new image as the runtime identity) and checks: schema at the
  required level; recorded checksums of the already-applied migrations unchanged, equal to
  the SHA-384 of the previous release's files, which are byte-identical at HEAD; runtime
  identity connected; identical head/version and states; verbatim replay of every old
  prepare/accept key (`replayed: true`, identical identifiers, ref unchanged); a new commit on
  top; `ledger-admin verify`; clean-install vs upgraded convergence of normalized DDL plus
  grants and of object ownership (not covered: role attributes, database-level ACLs,
  sequence values, seed rows).
- Evidence (`docs/quality/evidence/upgrade-2026-09-27.md`, rerun after the review fixes):
  25 commits before, schema 7 → 9, 25 states identical, 50 keys replayed identically,
  version 26 after, verifier clean, 598 DDL lines and 55 owned objects identical;
  `UPGRADE OK`. Data set is one graph on the happy path (undecided/rejected proposals and
  imported v1 data not carried across; recorded).

### Slice 8 — backup/restore smoke (§8, 2026-09-27): executed, PASS
- `scripts/backup-restore.sh`: under a sustained 50-writer load over 10 graphs, take
  `pg_dump -Fc` and `pg_basebackup -c fast -X stream` (local socket; production uses a
  `replication` role over TLS — runbook), each after recording the commits already
  acknowledged to clients; restore the dump by the documented path (`pg_restore --no-owner
  --no-acl` as the owner, `GRANT CONNECT`, `ledger-admin migrate --runtime-role` re-deriving
  the grants) and start a second PostgreSQL instance from the base backup; for each:
  `ledger-admin verify` → `VERIFY OK`, graph set equal to live, restored ref events ≥ the
  pre-backup watermark, every ref's `(version, head)` chain an exact contiguous prefix of
  the live chain, every restored decision/outbox/idempotency/proposal row present
  identically in live, and a server on the restored database (runtime identity) serving the
  restored heads with matching versions and reconstructing states identical to the live
  server at the same commit ids.
- Evidence (`docs/quality/evidence/backup-restore-2026-09-27.md`, rerun after the review
  fixes): dump at 275 commits (≥ 212 acknowledged before), base backup at 630 (≥ 275), live
  876 when the load stopped; dump restore 249 events / 2609 audit rows, base-backup
  instance 361 events / 3693 audit rows, all prefixes and present in live; no PUBLIC
  execute on any ledger function after the grant step; 10 + 10 restored heads served, states
  identical; both verifies clean; `BACKUP RESTORE OK`.
  Four drifted restores (trigger, CHECK, column grant, sequence grant removed) were each
  refused at server start-up (review round 2, §10). Restore forks history from the snapshot
  (versions reissued); ADR-0017 decides the recovery semantics. The smoke does not write to
  a restore.

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

### Slice 10 — performance baselines (§10, 2026-09-27): executed, recorded
- `ledger-stress bench` + `scripts/bench.sh`: single client, linear history, prepare /
  accept / ref read / state read at depths 1, 100, 1,000, 10,000 (prepare/accept on the last
  20 commits before each depth, reads at exactly that depth), plus a `--constant-state`
  control (add one quad, delete the previous) to separate history depth from state size.
- Evidence (`docs/quality/performance-baselines.md`): accept 3–4 ms and ref read ≈1 ms flat;
  prepare 11 → 18 → 216 → 1,905 ms p50 and state read 0.7 → 30 → 188 → 1,983 ms p50 from
  depth 1 to 10,000 (2.8 h to build the history); the constant-state control reproduces the
  growth with a one-quad state (200 ms at depth 1,000), so the per-ancestor fetch dominates
  (≈0.19–0.20 ms per ancestor). The document proposes batching/caching the ancestor fetch
  first and a checkpoint policy constrained by ADR-0008/0012/0013/0016 (verified-at-write
  or owner-written snapshots, never used by `prepare` unless verified, never dropped, new
  hashed format = protocol with ADR and golden vectors) — a decision for Phase 4.

### Review record — slices 6, 7, 8, 10 (test, storage/concurrency; Opus, read-only, on commit 9b60d23)
- No P0. P1s: (1) the depth benchmark sampled *after* each target depth, so the depth-10,000
  row could never be produced under the development `max_depth` (the run was stopped and
  the sampling changed to the last commits *before* each target; reads at exactly the
  target); (2) the baseline conflated history depth with state size (one quad per commit) →
  a `--constant-state` control run (add one / delete previous) is part of the baseline and
  the conclusions are stated per experiment; (3) the checkpoint proposal would have let a
  runtime-written cache decide the ADR-0008 effective delta and thus commit identity, and
  "dropping" checkpoints conflicted with ADR-0013's readability guarantee and 0005's
  write-once guard → rewritten: verified-at-write or owner-written snapshots, never used by
  `prepare` unless verified, never dropped, new hashed format = protocol (ADR + golden
  vectors); (4) restore forks history and reissues versions → runbook states the
  consequences and the open PITR/fencing decisions (tech-debt). P2s fixed: fuzz targets
  run with debug assertions (`-a`), pinned nightly honoured in CI, non-empty target list
  asserted, checked-in corpora read-only during runs, `prepare_body`/`accept_body` assert
  JSON-shape and element-order independence instead of comparing a digest with itself,
  `request_identity` tolerance uses the real metadata budget formula; restore checks require
  the graph set, a pre-backup watermark of acknowledged commits and identical audit rows
  (decisions/outbox/idempotency/proposals) in live; the documented restore path
  (`--no-owner --no-acl`, `GRANT CONNECT`, `migrate --runtime-role` re-deriving grants) is the
  one exercised; the upgrade harness emulates a Phase-1 cluster (no runtime role) and
  creates it as the runbook step, proves recorded checksums are sha384 of the previous
  release's files and compares object ownership beside the DDL diff (with `\restrict`
  lines filtered for newer `pg_dump`); evidence files record the exact build revision.
  Accepted/recorded: narrow upgrade data set (one graph, happy path), no write to a restore
  during the smoke, ASan validation deferred, remaining untested inputs (`Idempotency-Key`
  header parser, path `CommitId`).

### Review round 2 (PR #2 Codex threads, 2026-09-27): schema and identity verifier redesigned
- Codex found (P1) that `schema::verify` matched guard triggers by name only, anywhere in the
  database; (P1) that the content-address CHECK of 0009 was not verified at all; (P1) that
  `has_any_column_privilege` accepted any single column, so a role with `INSERT (id)` only,
  or an inherited table-level `INSERT` on `refs` (making `protected` writable), passed;
  (P2) that sequence privileges were not verified.
- Redesign: one requirement — a server may start only when every database integrity
  primitive the privilege model depends on is present, enabled, attached to the intended
  object and semantically what this build expects. `schema::verify` now checks each of the
  13 guard triggers (0004–0009) by *(table, name)* in `public` against an explicit
  expectation: function schema/name, `tgenabled = 'O'`, `tgtype` (row-level, BEFORE/AFTER,
  exact INSERT/UPDATE/DELETE set), the `UPDATE OF` column list from `tgattr`, and the
  constraint / deferrable / initially-deferred flags, all from catalog metadata (no
  `pg_get_triggerdef` string matching). It checks `immutable_objects_content_addressed`
  exists on `public.immutable_objects`, is a validated CHECK whose deparsed definition
  normalizes to the content-address condition, and probes it semantically inside a
  rolled-back transaction (a mislabelled object must be refused with 23514). Rust still
  verifies every object's digest on read; the CHECK is defence in depth.
  `verify_runtime_identity` now holds the role to an explicit per-table model
  (`RUNTIME_TABLE_MODEL`: whole-table SELECT; the exact INSERT and UPDATE column sets of
  migration 0008; never table-level INSERT/UPDATE, DELETE, TRUNCATE, TRIGGER or
  REFERENCES; nothing but SELECT on `_sqlx_migrations`), checking every column of every
  table with `has_column_privilege` (inherited grants included) and requiring `USAGE` on
  exactly the five audit sequences and no privilege on any other sequence.
- Tests (`pg_least_privilege`, real PostgreSQL, throwaway databases): same-named trigger
  moved to another table, right table with the wrong function, INSERT-only events, `UPDATE
  OF version` only, not deferrable, plain BEFORE trigger, disabled trigger, disabled and
  dropped write-once guards; CHECK dropped, `CHECK (true)`, `NOT VALID`, restored; one
  required INSERT column revoked, one of several columns granted, table-level INSERT on
  `refs`, `INSERT (protected)`, `UPDATE (reason)` on `ref_events`, `UPDATE (protected)`,
  inherited table-level UPDATE through role membership, revoked sequence USAGE, excess
  sequence UPDATE, a stray sequence with USAGE — each refused by `schema::verify` /
  `verify_runtime_identity`, by `PostgresLedgerStore::connect` (server start-up) and by
  `ready()` (readiness) with `SchemaIncompatible` / `RuntimeIdentity`; the exact role serves
  again after every re-grant.

### Review round 3 (fresh Codex review of `985e789`, 2026-09-27): two new P1s, fixed
- **Trigger function bodies.** Binding a trigger to its function by name and OID does not
  stop an owner from `CREATE OR REPLACE FUNCTION … $$ BEGIN RETURN NULL; END $$` — the trigger
  stays present, enabled and correctly attached while the guard is gone. Fix: the verifier
  derives the expected definition of every guard function (`graphs_identity_is_immutable`,
  `ledger_rows_are_write_once`, `refs_identity_is_immutable`, `refs_version_is_monotonic`,
  `outbox_identity_is_immutable`, `refs_movement_is_audited`,
  `graphs_status_change_serializes`, `ledger_lock_key`) from the *embedded migration SQL*
  (last `CREATE OR REPLACE FUNCTION` per name: body, language, return type, pinned
  `search_path`) and compares `pg_proc.prosrc`, `pg_language`, `format_type(prorettype)`,
  `proconfig` and `prosecdef = false` at start-up and readiness (catalog-only, lock-free).
  Tests: no-op body, `SECURITY DEFINER` variant, dropped `search_path`, permissive
  write-once guard — each refused by `schema::verify`, start-up and readiness; restored
  definitions serve again.
- **Settable owner-role memberships.** A `GRANT owner TO runtime WITH INHERIT FALSE, SET
  TRUE` leaves every direct grant exact yet lets the runtime credentials `SET ROLE` into the
  owner. Fix: `verify_runtime_identity` walks `pg_auth_members` transitively from the
  current role and refuses any membership — inherited or settable — in a superuser, a table
  owner, a CREATE holder on `public`, a role with CREATEROLE/CREATEDB/REPLICATION/BYPASSRLS,
  or a predefined `pg_*` role; it also refuses those attributes on the runtime role itself.
  Tests: settable owner membership, transitive membership through an intermediate role,
  `pg_read_all_data`, `CREATEDB` — each refused at start-up; the plain role serves again.

### Review round 4 (fresh Codex review of `9f22942`, 2026-09-27): two new P1s, fixed
- **`session_replication_role`.** PostgreSQL 15+ lets an owner `GRANT SET ON PARAMETER
  session_replication_role` to the runtime without touching any role attribute or table
  privilege; setting it to `replica` silences every ordinary (`tgenabled = 'O'`) guard
  trigger. Fix: `verify_runtime_identity` requires `current_setting('session_replication_role')
  = 'origin'`, refuses SET or ALTER SYSTEM privilege on it for the runtime role, and refuses
  any transitive membership in a role that holds the SET privilege. Tests: direct grant and
  grant through a settable membership, each refused at start-up; healthy after revocation.
- **Guard function ownership.** Ownership was checked for tables only; a guard function
  transferred to the runtime (or to a role it can become) could be dropped with CASCADE,
  removing its trigger while body and grants still verified. Fix: `verify_guard_functions`
  requires every guard function to be owned by the ledger's schema owner (the owner of
  `refs`), and the membership walk refuses roles that own any function in `public`. Test:
  `ALTER FUNCTION refs_movement_is_audited() OWNER TO <runtime>` refused by `schema::verify`,
  start-up and readiness; healthy after restoring the owner.

### Review round 5 (fresh Codex review of `d92a179`, 2026-09-27): three new P1s, fixed
- **PostgreSQL 15 floor.** The membership walk used `pg_auth_members.inherit_option` /
  `set_option`, which exist only from PostgreSQL 16, so on the documented floor every
  start-up would fail. Fix: the query is version-adaptive (`server_version_num`); on 15
  every membership is treated as inheritable and settable (which is what 15 does). Evidence: the CHECK fingerprint also strips the node-tree `location` offsets so a
  re-added identical definition keeps its fingerprint on every version; the
  `pg_least_privilege`, `pg_verify`, `pg_workflow`, `pg_immutable_store` and
  `pg_graphs_migration` suites run against `postgres:15-bookworm` (15.19) in addition to
  the compose PostgreSQL 17.2, with the tests using PostgreSQL-15 grant syntax where 16's
  `WITH INHERIT FALSE, SET TRUE` is unavailable (results in the round-5 gate log).
- **Privileges reachable only through `SET ROLE`.** A `NOINHERIT, SET TRUE` parent holding,
  e.g., `UPDATE (status)` on `graphs` is invisible to the `current_user` checks yet one
  `SET ROLE` away (and `status = importing` unlocks 0009's importing exemption). Fix: every
  transitively assumable role is audited against the same table, column and sequence model
  as the runtime — as a *subset* (no privilege beyond the model) — plus no EXECUTE on
  `ledger_grant_runtime` for the runtime or any assumable role. Tests: parent with
  `UPDATE (status)`, with EXECUTE on the grant function, with USAGE on a stray sequence —
  each refused; a SELECT-only parent (within the model) passes.
- **Conditional triggers.** `pg_trigger.tgqual` was not checked, so `WHEN (false)` kept
  every structural property while the guard never fired. Fix: every guard trigger must be
  unconditional (`tgqual IS NULL`). Test: `refs_movement_audited` recreated `WHEN (false)`
  is refused by `schema::verify`, start-up and readiness.

### Review round 6 (fresh Codex review of `8dd0a7d`, 2026-09-27): two new P1s, fixed
- **Referential integrity not verified.** A restore that lost `commit_index.id REFERENCES
  immutable_objects(id)` (0003) passed with intact migration metadata, letting the runtime's
  permitted inserts create a commit-index row without bytes and a ref pointing at it. Fix:
  `schema::verify` verifies the complete inventory of migrations 0001–0009's FOREIGN KEY,
  PRIMARY KEY and UNIQUE constraints by *shape* (table, key columns, referenced table and
  columns — resolved from `conkey`/`confkey`, never by name), each validated and
  non-deferrable; the three partial/plain unique indexes on `decisions`; and presence +
  validation of every named CHECK constraint (their definitions remain the Rust layer's
  domain rules, except the content-address CHECK which is checked by definition and probe).
  Catalog-only and lock-free, so it runs on readiness. Test:
  `pg_least_privilege::lost_referential_and_uniqueness_constraints_are_refused_at_startup_and_readiness`
  — dropped `commit_index_id_fkey`, the same FK `NOT VALID`, a deferrable `refs_head_fk`,
  dropped `ref_events_version_unique`, dropped `decisions_one_per_candidate`, dropped
  `ref_events_genesis_shape` — each refused by `schema::verify`, start-up and readiness;
  healthy after restoration.
- **`ALTER SYSTEM` reachable through membership.** The assumable-role walk checked only the
  SET privilege on `session_replication_role`; a settable parent with `ALTER SYSTEM` could
  change the cluster default. Fix: both SET and ALTER SYSTEM are refused for every assumable
  role (the direct check already covered both).

### Admission-control decision (§15, 2026-09-27)
Measured: under 1,000 concurrent clients on two replicas the 12-slot expensive semaphore per
replica refuses the excess prepares immediately (`503 RESOURCE_LIMIT`), successful prepare p99
stayed ≤ 327 ms, cheap paths (ref reads, readiness) kept answering, max 32 runtime sessions
(= 2 × pool), 0 deadlocks; `accept` (≈4 ms, one short transaction) takes no slot and is bounded
only by the 16-connection pool and the session timeouts; an edge timeout drops the handler and
its slot while the PostgreSQL statement runs on until `lock_timeout`/`statement_timeout`.
Decision for P1.5: **keep immediate rejection and the current budgets; no API or protocol
change.** Rationale: the goals (bounded work, bounded connection use, predictable overload
semantics, no starvation of cheap operations) are met by the measurements; a bounded wait
queue would trade immediate, retryable refusals for latency without changing the bound.
Follow-ups recorded as later-phase work, to be measured under an accept-heavy load before
any change: a shared or separate bounded budget for `accept`, and cancellation of the
abandoned statement (`pg_cancel_backend` or a cancellation-aware driver call) when the edge
timeout fires. Not a production blocker: the pool and session limits already bound the
damage, and the runtime identity cannot exceed them.

### Live issuer smoke test (§13): PENDING_EXTERNAL
`scripts/live-issuer-smoke.sh` runs `ledger-server` in `LEDGER_AUTH_MODE=oidc` against a real
issuer/JWKS configuration with an externally issued bearer token (environment or file, never
committed, printed or logged) and checks: no token → 401; tampered signature → 401;
authenticated graph-scoped read of the token's tenant (404 NOT_FOUND on an empty ref *after*
authentication); a foreign tenant's graph indistinguishable (404); prepare → 201 with the
proposal row recording the expected tenant / principal / principal type when the token maps
to `propose` (else 403 FORBIDDEN); accept never 200 under production auth. Required
configuration (Entra ID): `LEDGER_LIVE_ISSUER=https://login.microsoftonline.com/<tenant>/v2.0`,
`LEDGER_LIVE_AUDIENCE=<application id URI or client id>`,
`LEDGER_LIVE_JWKS_URL=https://login.microsoftonline.com/<tenant>/discovery/v2.0/keys`,
`LEDGER_LIVE_TENANT=<tid>`, `LEDGER_LIVE_PRINCIPAL=<oid>`, `LEDGER_LIVE_TOKEN_FILE=<path>` (or
`LEDGER_LIVE_TOKEN`), optionally `LEDGER_AUTH_ROLE_MAP`, `LEDGER_AUTH_AGENT_CLIENT_IDS`,
`LEDGER_LIVE_EXPECT_PROPOSE=1`, `LEDGER_LIVE_EXPECT_PRINCIPAL_TYPE`. Without a dev tenant and
token the script exits with `LIVE_ISSUER=PENDING_EXTERNAL` and nothing is claimed.

### Phase-2 handoff (§18): interfaces Phase 2 inherits from P1.5
ADR-0014 and the specification's "Validation and acceptance" section still describe the
architecture Phase 2 implements (two-phase prepare/accept, `ValidationRecord` referenced by
candidates and decisions and never embedded in hashed bytes, validator-agnostic ledger). P1.5
added operational controls Phase 2 must respect, none of which changes the semantic design:
- **Two database identities (ADR-0016):** Phase 2's validation-record persistence needs a new
  migration (0010+) with column-level grants for the runtime role, an entry in the verifier's
  `RUNTIME_TABLE_MODEL` / `GUARD_TRIGGERS` / `RUNTIME_SEQUENCES`, and `pg_least_privilege`
  coverage; the Sculpin adapter runs under the runtime identity (or as a separate service
  with its own identity) and never needs owner rights.
- **Acceptance is fail-closed in production:** `WorkflowRepository::accept` enforces
  `ValidationPolicy::Required`; `decisions.validation_ids` already exists for the record ids;
  the development switch `LEDGER_UNVALIDATED_ACCEPTANCE` is refused with production auth.
- **Proposal/decision persistence and idempotency:** `proposals`, `decisions`, `ref_events`,
  `projection_outbox` and `idempotency` are write-once audit tables with DB-enforced
  fast-forward ref movement (0009); Phase 2 adds validation outcomes as new rows/records, never
  updates.
- **Projection outbox:** one row per accepted decision, delivered by the Phase-3 consumer
  identity (grants for `delivered_at`/`attempts` are that phase's migration).
- **Resource and admission limits:** validation calls are expensive operations and must take
  the expensive-operation slot (or a dedicated budget) and respect `request_timeout`; the
  §15 decision applies.
- **Restore semantics (ADR-0017):** validation records are part of the single database and
  restore with it; consumers reconcile to the declared restore point.

## Qualification decision (§22–23, 2026-09-27)
**Production-qualified: NO.** Executed and passing on this branch: slices 1 (least
privilege, ADR-0016), 2 (supply chain, image), 3 (1,000 writers over two replicas),
4 (kill injection), 5 except the live issuer, 6 bounded fuzzing without ASan, 7 (upgrade),
8 (backup/restore), 9 (adversarial limits), 10 (baselines with the checkpoint proposal),
§20 verifier, §21 runbook; every slice reviewed independently with no open P0/P1.
Open before the gate can pass:
- **Live Entra ID issuer smoke test — PENDING** (no tenant credentials available to these
  runs; never marked passed). Release prerequisite.
- Fuzzing: complete for this gate — hosted matrix (none/address, 45 s per target on every
  pull request) and the 900 s-per-target campaign (run 36308371828) both clean; the weekly
  schedule repeats the campaign.
- Deployment decisions recorded in tech-debt that a production operator must take:
  restore semantics (PITR/WAL archiving, writer fencing, projection rebuild), checkpoint ADR
  before Phase 4, admission budget for `accept` and cancellation of abandoned statements.
- Final independent security review of the complete branch (2026-09-27, Opus, read-only):
  no P0/P1, nothing blocking the pull request. Fixed in the same change: the startup
  identity check now also forbids `TRUNCATE`/`TRIGGER`/`REFERENCES` on every table and any
  write to `_sqlx_migrations` (a later `GRANT TRUNCATE` could have wiped audit history
  without firing the row-level guards); `ledger-admin migrate --runtime-role` re-applies the
  migrations' `REVOKE … FROM PUBLIC` on the ledger functions, which `pg_restore --no-acl`
  drops, and the restore smoke asserts no PUBLIC execute remains; `scripts/test-integration.sh`
  runs in its own compose project (it could take over and delete a developer's default
  stack); the stress tool refuses `host`/`hostaddr` DSN parameters; CI tool versions pinned;
  documentation wording corrected ("exercises"/"checks" instead of "proves", fuzz coverage
  exceptions, diff exclusions, owner password rotation, checkpointer identity). Recorded in
  tech-debt: test-role teardown, zero timeouts, DSNs in process arguments.

## Sub-agent decomposition (§42)
storage/concurrency (role split, kill injection), API/security (multi-replica auth,
adversarial limits, live issuer), testing/fault-injection (1,000-writer harness, upgrade,
backup/restore), performance (baselines), supply-chain (audit/deny/SBOM/scan). Protocol
and canonicalization files stay owned by the main session and are not touched.
