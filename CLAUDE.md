# Sculpin Cognitive Ledger

Read `docs/exec-plans/active/` before implementation, then `docs/product-specs/cognitive-ledger.md` and `ARCHITECTURE.md`. Use `docs/README.md` as the map and `docs/decisions/` for accepted architecture. Update plan evidence and relevant docs with meaningful work.

## Boundaries
The ledger owns immutable RDF changes/commits, history, CAS refs, provenance, reconstruction, and later merge coordination. Jena owns reasoning/SHACL/domain semantics; Fuseki owns queries/projection; the virtual A-box owns transient context. Never add a query/reasoning engine. Fluree is test-reference-only: do not copy its source or add a runtime/compiled dependency.

Canonical bytes/hashes and hard invariants are persistent protocol; see the product specification and `skills/ledger-invariants/`. Changes need an ADR and golden-vector review. Core crates stay free of HTTP/database/container clients. Never rewrite Git or ledger history destructively, disable a quality gate, weaken an assertion, or invent test evidence.

## Commands
- `./scripts/format.sh`
- `./scripts/check-fast.sh`
- `./scripts/lint.sh`
- `./scripts/test.sh`
- `./scripts/test-integration.sh`
- `./scripts/test-differential.sh`
- `./scripts/quality-gate.sh full`
- `./scripts/compose-up.sh` / `./scripts/compose-down.sh`

Canonical skills are exposed at `.claude/skills -> ../skills`. Use the narrow specialist agents in `.claude/agents/` when independent failure-finding helps; deep reviews use the stable `opus` alias. Keep implementation ownership in the main session. Path-specific rules live under `.claude/rules/`.
