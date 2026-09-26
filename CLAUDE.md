# Sculpin Cognitive Ledger

Read `docs/exec-plans/active/` before implementation, then `docs/product-specs/cognitive-ledger.md` and `ARCHITECTURE.md`. Use `docs/README.md` as the map and `docs/decisions/` for accepted architecture. `product_development_plan.md` is the phased roadmap (P0–P8); follow its sequence — consolidation/decisions (P0) before persistence/auth (P1), validation (P2), projection (P3), branches (P4), merge (P5). Update plan evidence and relevant docs with meaningful work.

## Development discipline (product_development_plan.md §42)
Operate as planner/orchestrator, not one long session mutating every layer. Per phase: read `AGENTS.md`/`CLAUDE.md`/the spec/relevant ADRs/the last completed plan; write a new execution plan with scope, non-goals, invariants, migration impact, affected crates, and acceptance evidence; use bounded specialist sub-agents for disjoint tasks (protocol/invariant, storage/concurrency, API/security, semantic/Sculpin integration, testing/fault-injection, performance). Keep canonicalization/protocol files owned by the main session. Never combine a protocol-identity change with unrelated feature work. Update docs in the same change as behaviour. Any decision affecting persistent identity or atomicity needs an ADR before implementation. A phase is complete only when its acceptance conditions are executable and pass — never mark placeholders or seam-only tests as done.

## Boundaries
The ledger owns immutable RDF changes/commits, history, CAS refs, provenance, reconstruction, and later merge coordination. The Sculpin semantic validation/reasoning layer (currently pySHACL/Python; Jena is an implementation detail, not a dependency) owns reasoning/SHACL/domain semantics; Fuseki owns queries/projection; the virtual A-box owns transient context. Never add a query/reasoning engine. Fluree is test-reference-only: do not copy its source or add a runtime/compiled dependency.

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
