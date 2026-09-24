# Sculpin Cognitive Ledger

A narrow, content-addressed history service for RDF knowledge evolution. It owns immutable changes, commit history, provenance, reconstruction, and CAS refs—not SPARQL, reasoning, SHACL, or query projection.

Start with [the documentation index](docs/README.md), [authoritative specification](docs/product-specs/cognitive-ledger.md), and [current execution plan](docs/exec-plans/active/0002-postgresql-cas-and-container-integration.md).

```bash
./scripts/check-fast.sh
cargo run -p ledger-server
```
