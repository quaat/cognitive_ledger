# Sculpin Cognitive Ledger

A narrow, content-addressed history service for RDF knowledge evolution. It owns immutable changes, commit history, provenance, reconstruction, and CAS refs—not SPARQL, reasoning, SHACL, or query projection.

Start with [the documentation index](docs/README.md), [authoritative specification](docs/product-specs/cognitive-ledger.md), the [phased development plan](product_development_plan.md) (P0–P8), and [execution plans](docs/exec-plans/README.md).

```bash
./scripts/check-fast.sh
cargo run -p ledger-server                 # filesystem-only development mode (read-only, loopback)
# shared PostgreSQL refs + content + workflow, authenticated API (no unauthenticated mode):
LEDGER_DATABASE_URL=postgres://… LEDGER_AUTH_MODE=oidc LEDGER_AUTH_ISSUER=… \
  LEDGER_AUTH_AUDIENCE=… LEDGER_AUTH_JWKS_URL=https://… cargo run -p ledger-server
LEDGER_DATABASE_URL=postgres://… cargo run -p ledger-server --bin ledger-admin -- \
  graph create --graph <id> --tenant <id> --status active
cargo run -p ledger-server --bin ledger-admin -- migrate-fs-to-pg --source ./data --json
```

The HTTP contract is [`docs/api/openapi.json`](docs/api/openapi.json) (graph-scoped
prepare/accept/reject, ref and bounded state reads; `Idempotency-Key` required on writes).
Runtime variables and the security boundary are documented in
[storage boundaries](docs/design/storage-boundaries.md) and [security](docs/quality/security.md);
migrations in [`migrations/README.md`](migrations/README.md). `docker compose up` runs a
development-only configuration (HS256 test authenticator, unvalidated acceptance) that
must never be copied into a production deployment.
