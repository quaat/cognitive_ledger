# Sculpin Cognitive Ledger

A narrow, content-addressed history service for RDF knowledge evolution. It owns immutable changes, commit history, provenance, reconstruction, and CAS refs—not SPARQL, reasoning, SHACL, or query projection.

Start with [the documentation index](docs/README.md), [authoritative specification](docs/product-specs/cognitive-ledger.md), the [phased development plan](product_development_plan.md) (P0–P8), and [execution plans](docs/exec-plans/README.md).

```bash
./scripts/check-fast.sh
cargo run -p ledger-server                 # filesystem-only development mode
LEDGER_DATABASE_URL=postgres://… cargo run -p ledger-server   # shared PostgreSQL refs + content
cargo run -p ledger-server --bin ledger-admin -- migrate-fs-to-pg --source ./data --json
```

Runtime variables are documented in [storage boundaries](docs/design/storage-boundaries.md); migrations in [`migrations/README.md`](migrations/README.md).
