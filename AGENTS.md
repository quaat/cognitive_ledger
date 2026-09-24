# Sculpin Cognitive Ledger agent map

## Start here
Sculpin Cognitive Ledger is a narrow, Git-like RDF history service—not a graph database. Before implementation, read the one plan in `docs/exec-plans/active/`, then the authoritative `docs/product-specs/cognitive-ledger.md` and `ARCHITECTURE.md`.

- Documentation index: `docs/README.md`
- Architectural decisions: `docs/decisions/README.md`
- Current work/evidence: `docs/exec-plans/active/`
- Deferred work: `docs/exec-plans/tech-debt.md`
- Provider-neutral workflows: `skills/`

## Non-negotiable boundaries
- Ledger owns immutable changes/commits, DAG history, refs, provenance, reconstruction, and merge coordination.
- Sculpin/Jena owns semantics, SHACL, reasoning, and domain rules. Fuseki owns queryable projections. The virtual A-box owns transient context.
- Never add SPARQL, OWL/SHACL engines, general graph indexing, or other database scope here.
- Fluree is an external differential-test reference only: no copied source, runtime, or compiled dependency.
- Never rewrite history unsafely. Every ref movement is CAS and must target existing immutable content.
- Canonical serialization and hashing are persistent protocol. Changes require an ADR, compatibility analysis, and explicit golden-vector review.
- Core crates must not depend on Axum, SQLx/PostgreSQL, Fuseki, Docker, or object-store clients.

## Workflow
1. Read/update the active plan before coding; distinguish milestone work from tangents.
2. Architectural/invariant changes require an ADR. Meaningful work updates docs and plan evidence.
3. Tests and docs are implementation. Never weaken a gate or fabricate a result; label unavailable checks.
4. Use bounded subagents for independent research/review when it improves quality (invariants, protocol, tests, security). The main session owns architecture and integration; do not concurrently edit the same core files.
5. Do not use destructive Git operations or force pushes.

## Commands
- Format: `./scripts/format.sh`
- Fast check: `./scripts/check-fast.sh`
- Lint: `./scripts/lint.sh`
- Unit/workspace tests: `./scripts/test.sh`
- Integration: `./scripts/test-integration.sh`
- Differential: `./scripts/test-differential.sh`
- Full local gate: `./scripts/quality-gate.sh full`
- Compose: `./scripts/compose-up.sh` / `./scripts/compose-down.sh`

Check modes do not modify source. Docker commands may be unavailable locally; record that honestly and rely on Docker-capable CI. Completed work must update the plan's exact test evidence before moving it to `completed/`.
