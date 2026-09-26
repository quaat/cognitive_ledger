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

## Assumptions and status
- The identity provider is trusted for the claims it signs; the ledger does not verify
  that an `on_behalf_of` human consented.
- The PostgreSQL runtime role still owns the schema and migrations run on connect; the
  role split, kill-based fault injection, dependency/container scanning and live-issuer
  tests are P1.5. **The service is not production-qualified until P1.5 passes.**

## Supply chain
`scripts/check-supply-chain.sh` (blocking in `ci-security`) runs `cargo audit` with the
single exception documented in `.cargo/audit.toml` — RUSTSEC-2023-0071 (`rsa 0.9`), a
lockfile-only optional dependency of sqlx's MySQL driver that no feature of this workspace
enables — and first re-proves the premise: `rsa` must be unreachable in the feature-resolved
build graph of every target, have `sqlx-mysql` as its only lockfile dependent, and stay on
the advisory's 0.9 line; any change fails the gate so the exception is re-evaluated (trigger:
every sqlx upgrade, Plan 0005 supply-chain slice). GitHub dependency review runs on every
pull request (Dependency graph enabled 2026-09-26).

Dependency and license findings must be classified rather than ignored. Fluree's BSL image
is optional test infrastructure and is not shipped. The intended runtime dependency policy
permits common permissive licenses; strong copyleft additions require review.
