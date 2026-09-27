# Security

Treat RDF, metadata, IDs, paths, and HTTP bodies as untrusted. Reject malformed IDs/terms
and blank nodes; derive object paths only from parsed digests; cap input and ancestry work.
Preserve object digest verification and atomic ref updates. Never commit secrets.

## Authentication boundary (P1.4, ADR-0011)
- Every public route except `/health`, `/ready` and `/openapi.json` requires a bearer token.
  There is **no unauthenticated mode and no header-trusting mode**: the server does not
  accept tenant, principal, principal type or roles from request headers or bodies.
- `LEDGER_AUTH_MODE=oidc` (production): tokens are validated cryptographically
  (`OidcAuthenticator`): signature against the issuer's JWKS (`LEDGER_AUTH_JWKS_URL`, https
  only, no redirects, 256 KiB cap), issuer, audience, `exp`/`nbf` (30 s leeway; `exp`, `iss`
  and `aud` are required claims). JWK selection honours the key's own metadata: a key is
  loaded only if it has a `kid`, is not `use=enc`, has no `key_ops` list lacking `verify`
  (a `use`/`key_ops` contradiction skips the key), and its `alg`, when stated, is exactly
  the one algorithm supported for its family (RSA → RS256, P-256 → ES256); a key stating
  any other algorithm (PS256, RS512, ES384, …) or of another family/curve is skipped, never
  reinterpreted. The token's `alg` must equal the selected key's algorithm. Keys are cached
  by `kid` for at most one hour and refreshed on an unknown `kid` or when the cache has aged
  out, at most once a minute, under a single-flight lock. A cached key is never trusted
  beyond the maximum age: after expiry a request authenticates only once a refresh has
  succeeded, and while the issuer is unreachable (including inside the retry throttle) the
  request fails with `DEPENDENCY_UNAVAILABLE` rather than with stale keys. An unknown key
  fails closed; a withdrawn key stops validating within the cache age. A JWKS fetch failure
  is never a bypass. The service speaks plain
  HTTP: **TLS termination in front of it is a deployment requirement** because bearer
  tokens are sent in clear otherwise.
- `LEDGER_AUTH_MODE=dev-hs256` (development/CI only): HS256 against a ≥32-byte shared
  secret with the same issuer/audience/expiry rules. It reports itself as not production
  grade; the server **refuses a non-loopback bind** with it unless
  `LEDGER_ALLOW_INSECURE_NON_LOOPBACK=allow-insecure-non-loopback-development-only` is set
  (the compose harness sets it inside an isolated container network).
- Identity mapping (`ClaimsPolicy`) is explicit configuration: tenant claim (`tid`),
  principal claim (`oid`), principal type from `sculpin_principal_type` or from configured
  agent/service client-id lists (`LEDGER_AUTH_AGENT_CLIENT_IDS`,
  `LEDGER_AUTH_SERVICE_CLIENT_IDS` matched against `azp`/`appid`), roles claim (`roles`) →
  capabilities via `LEDGER_AUTH_ROLE_MAP` (default `ledger.read|propose|review|admin`). A
  token whose tenant, principal, type or any recognised role cannot be determined is
  `UNAUTHENTICATED`; if the type claim and the configured client lists disagree the token is
  refused, so a claim the identity provider lets clients influence cannot override
  configuration. The type claim must be issued under tenant-administrator control (Entra
  optional claims / claims mapping), never a user-editable attribute.
- Filesystem-only mode has no authentication and is therefore **read-only** and
  loopback-only.

## Authorization and tenant isolation
- Capabilities: `read` (ref/state reads), `propose` (prepare), `review` (accept/reject),
  `admin` (reserved; graph provisioning is the `ledger-admin` operator command, not HTTP).
- Every route is graph-scoped. The graph must exist **and** belong to the token's tenant;
  otherwise `NOT_FOUND` with a body identical to a nonexistent graph. State reads also
  require the commit to be indexed under that graph. Migration 0007's composite
  `(graph_id, tenant_id)` foreign keys make tenant/graph agreement a database invariant.

## Mutation semantics
- `Idempotency-Key` is required; idempotency is scoped by tenant, complete actor
  (principal id, type, on-behalf-of), graph, operation and key, and by the server-computed
  canonical request digest `sculpin-ledger-request/v1` (golden-pinned; clients never supply
  it). Retries replay the durable result; a different request under the same key is
  `IDEMPOTENCY_CONFLICT`.
- Unknown JSON fields are rejected, so a client cannot smuggle `tenant_id`, `principal_*`,
  `on_behalf_of`, `request_digest` or `recorded_at`.
- `accept` fails closed with `VALIDATION_REQUIRED` until Phase 2 supplies semantic
  validation, unless `LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only`
  is set (startup warning; CI/development only). The switch is refused together with
  `LEDGER_AUTH_MODE=oidc`, so a copied development environment cannot enable it in
  production. The store itself enforces the policy (after the idempotent-replay lookup, so
  a durable earlier acceptance still replays). No validation record is ever fabricated.
- A prepare is refused (`RESOURCE_LIMIT`) when the candidate's depth or resulting state
  would exceed the reconstruction limits, so nothing can be accepted that cannot later be
  read or extended under the same limits.
- `propose`/`review` imply learning the ref's current head (`HEAD_CHANGED`) and whether a
  given quad is in the base state (`BASE_MISMATCH`, `NO_EFFECTIVE_CHANGE`); the effective
  delta design makes this oracle inherent within the caller's own graph. Any `read`
  principal of the tenant may reconstruct prepared, rejected or superseded candidates of
  its graphs (they are indexed commits), which review requires.

## Limits and errors
- Configurable limits (`LEDGER_LIMIT_*`): body bytes, patch operations, term bytes,
  metadata bytes, reconstruction depth/quads/bytes (shared `ReconstructionLimits`, also
  bounding the workflow's base reconstruction), export bytes, request seconds, concurrent
  expensive operations. Exceeding any yields `RESOURCE_LIMIT`.
- Errors are the envelope `{code, message, correlation_id}` with a fixed code set
  (`docs/api/openapi.json`). Messages never carry SQL, storage paths, foreign graph or
  tenant identifiers, or lineage details (`LINEAGE_MISMATCH` is generic; storage-level
  errors such as a graph-binding conflict or cross-graph parent are reported as `INTERNAL`
  with a fixed message; the full error is logged under the correlation id). 503 (`DEPENDENCY_UNAVAILABLE`, or `RESOURCE_LIMIT` from the time or
  concurrency limit) means the outcome is unknown to the client: **retry with the same
  `Idempotency-Key`**; a completed request replays, an aborted one runs once.

## Database privilege boundary (ADR-0016)
The runtime identity (`LEDGER_DATABASE_URL`) holds exactly the DML the request path
executes (migration 0008: `SELECT`, column-level `INSERT`, `UPDATE (head, version,
updated_at)` on `refs`) and cannot `ALTER`, `DROP`, `TRUNCATE`, `DISABLE TRIGGER`, modify
or delete existing immutable or audit rows, back-date or pre-mark new rows, provision
graphs or run migrations. Migration 0009 makes ref movement itself a database fact: a head
move needs a matching ref event in the same transaction and must be a fast-forward, status
changes serialize against in-flight workflows, and objects must be content-addressed.
Migrations run only through `ledger-admin migrate` with the owner identity; the server
verifies the exact schema level, contiguity, checksums and enabled guards, and verifies
that its own role is a least-privilege identity, refusing otherwise. Every runtime session
is bounded by `statement_timeout`, `lock_timeout` and `idle_in_transaction_session_timeout`
(set per connection; requires a direct or session-mode connection). **Accepted residual
risk:** the runtime is the trusted writer of new audit rows, so a compromised runtime can
still fabricate a consistent forward move within its tenants; `SECURITY DEFINER` write
functions would close this (tech-debt). `pg_least_privilege` proves the denied statement
set, the privilege matrix, the schema-level refusals, the identity refusal and the 0009
integrity rules against real PostgreSQL, including a non-superuser owner.

## Assumptions and status
- The identity provider is trusted for the claims it signs; the ledger does not verify
  that an `on_behalf_of` human consented.
- Executed P1.5 evidence so far: least privilege (slice 1), supply chain (slice 2), the
  1,000-writer and kill-injection runs (`docs/quality/evidence/`), two-replica JWKS
  rotation and adversarial limits (`pg_api`), bounded fuzzing of the RDF/patch/commit
  decoders, the request bodies and the request-identity encoder (`fuzz/`, `ci-fuzz`; not
  yet fuzzed: the `Idempotency-Key` header parser, the path `CommitId`, JWT/JWKS parsing,
  which the `jsonwebtoken` crate owns), upgrade and backup/restore smoke runs, and the depth
  baselines (`docs/quality/evidence/`, `performance-baselines.md`). ASan fuzz runs, a
  multi-hour fuzz campaign and the **live Entra ID issuer smoke test (pending: no tenant
  credentials available to the runs; never mark it passed)** remain. **The service is not
  production-qualified until Plan 0005 passes in full.**
  performance baselines and the **live Entra ID issuer smoke test (pending: no tenant
  credentials available to the runs; never mark it passed)** remain. **The service is not
  production-qualified until Plan 0005 passes in full.**

## Supply chain (Plan 0005 slice 2)
`scripts/check-supply-chain.sh` (blocking in `ci-security`) runs, in order:
1. the RUSTSEC-2023-0071 premise proof (`rsa 0.9` is a lockfile-only optional dependency
   of sqlx's MySQL driver: unreachable in the feature-resolved graph of every target, only
   dependent `sqlx-mysql`, still on the 0.9 line) and `cargo audit` with that single
   documented exception (`.cargo/audit.toml`);
2. `cargo deny check advisories licenses bans sources` against `deny.toml`: RustSec
   advisories on the feature-resolved graph (no exception needed there; `rsa` is banned so
   its becoming reachable is a hard failure), a permissive-only licence allow list (MIT,
   Apache-2.0, BSD-2/3, ISC, Zlib, Unicode-3.0, Unlicense, CC0-1.0,
   BSL-1.0, CDLA-Permissive-2.0; strong copyleft requires review), bans on Fluree and on
   SPARQL/query engines (boundaries), duplicates reported as warnings and reviewed,
   crates.io as the only source. Workspace crates are `publish = false`;
3. CycloneDX 1.5 SBOMs of the two shipped binaries (`cargo cyclonedx --describe binaries`),
   filtered to the feature-resolved build graph of `cargo tree -e normal,build` for
   `x86_64-unknown-linux-gnu` (cargo-cyclonedx reads `cargo metadata`, which also lists the
   lockfile-only `rsa`, `sqlx-mysql`, `sqlx-sqlite`, Windows/wasm and dev-only crates), and
   failed if `rsa`, `sqlx-mysql`, `sqlx-sqlite`, `openssl`, `openssl-sys` or `native-tls`
   appear; sanity-checked and uploaded as CI artefacts (never committed). `deny.toml` bans
   the OpenSSL and native-tls crates outright (TLS is rustls, JWT crypto is aws-lc-rs).
GitHub dependency review runs on every pull request (Dependency graph enabled 2026-09-26);
its first run found GHSA-h395-gr6q-cpjc in `jsonwebtoken 9.3.1` (a malformed `exp`/`nbf`
JSON type was treated as an absent claim), fixed by upgrading to `jsonwebtoken 11.1.0` on
the `aws-lc-rs` backend (chosen over `rust_crypto`, which would have made the `rsa` crate
reachable). Every third-party GitHub Action is pinned to an immutable commit SHA with the
release name in a comment and checks out without persisting the token; Dependabot
(`.github/dependabot.yml`) proposes weekly updates for Actions and the Cargo lockfile as
ordinary gated pull requests (never auto-merged). Both container images the build depends on
are digest-pinned: the `rust:1.89-bookworm` builder and the distroless runtime.

### Container image
The runtime image is `gcr.io/distroless/cc-debian12:nonroot` pinned by digest (glibc,
libgcc, libstdc++, openssl libs, tzdata, ca-certificates; no shell, package manager or
curl; uid 65532; 11 OS packages, ≈47 MB). Health probes use `ledger-admin probe` (a loopback
GET that accepts only a parsed plain-http URL whose host is a loopback address or exactly
`localhost`, without userinfo; no proxy, no redirects, 2 s bound, URL never echoed).
`ci-security`'s `container` job builds the image, emits its CycloneDX SBOM and the full JSON
report first (uploaded even when the gate fails), then fails on any CRITICAL/HIGH finding
whether or not a fix exists: an unfixed one must be classified in `.trivyignore` with
rationale and review trigger (it is empty), never dropped by `ignore-unfixed`. The scanner
version is pinned to the one this classification used.
Classification of the 2026-09-26 scan (Trivy 0.74.0; 0 CRITICAL, 0 HIGH, 17 MEDIUM, 16 LOW,
1 UNKNOWN):
- `libc6` 2.36 — 15 MEDIUM and 7 LOW, all `affected`/`fix_deferred` in Debian 12 (no fix
  available): **accepted**; the ledger does not expose glibc parsing surfaces to untrusted
  input beyond what Rust's std uses (no `wordexp`, `strfmon`, iconv, nscd, getaddrinfo-
  driven DNS on untrusted names — the only outbound connections are PostgreSQL and the
  configured JWKS URL). Review trigger: each distroless base refresh.
- `libssl3` 3.0.20 — 2 MEDIUM + 4 LOW with a fix in 3.0.22 (`fixed`) plus CVE-2025-27587
  (LOW, `affected`, no fix): **fix pending base refresh** / **not applicable** respectively;
  the ledger links `aws-lc-rs` and `rustls`, not OpenSSL (`deny.toml` bans the OpenSSL
  crates), so the library is unused by the process. Refresh the distroless digest when the
  base ships 3.0.22; the CI gate would fail on it only if a finding reached HIGH.
- `gcc-12-base`/`libgcc-s1`/`libgomp1`/`libstdc++6` CVE-2022-27943 (LOW, `affected`, a
  libiberty demangler issue): **not applicable** (no demangling at runtime).
- `tzdata` DLA-4792-1 (UNKNOWN, data update): **fix pending base refresh**.
The previous `debian:bookworm-slim` base carried 4 CRITICAL and 63 HIGH unfixed findings
(perl, util-linux, curl, systemd, zlib) in 106 packages; moving to distroless removed them
rather than accepting them.

Dependency and license findings must be classified rather than ignored. Fluree's BSL image
is optional test infrastructure and is not shipped. The intended runtime dependency policy
permits common permissive licenses; strong copyleft additions require review.
