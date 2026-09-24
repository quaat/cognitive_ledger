# Plan 0002: PostgreSQL CAS and container integration

## Goal
Replace host-local mutable-ref coordination with PostgreSQL CAS and establish executable container integration evidence, while keeping immutable objects filesystem-backed and core infrastructure-free.

## Scope
- Add additive PostgreSQL migrations and a SQL-backed `RefStore`.
- Prove a real two-connection race where exactly one writer advances.
- Exercise commit/state HTTP behavior and clean service restart in Compose.
- Pin maintained Fuseki/PostgreSQL images by policy; resolve and pin the official Fluree reference image and implement its version-specific comparison transport if feasible.

## Non-goals
General branching, merge commits, Jena semantic validation, S3, checkpoints, authentication, or performance optimization.

## Relevant invariants
Product invariants 1–10 and 12–14, especially CAS, existing targets, reconstructability, and projection isolation.

## Assumptions
Docker-capable CI is available. PostgreSQL is mutable metadata only; object bytes remain immutable filesystem content.

## Work breakdown
- [x] Harden the unreleased bootstrap: directory fsync, validated RDF/Serde construction, standards N-Quads parsing, ordered v1 parents, ledger recording time, and restored normative detail.
- [ ] Verify current image versions/digests and SQLx release from primary sources.
- [ ] Write and test additive ref-schema migrations.
- [ ] Implement PostgreSQL `RefStore` behind the existing core trait.
- [ ] Add simultaneous two-connection CAS integration test.
- [ ] Add HTTP commit/state/restart Compose scenario.
- [ ] Resolve Fluree pin/API or record a specific external blocker.
- [ ] Run independent architecture, test, and security reviews.
- [ ] Execute and record all applicable gates.

## Dependency/order constraints
Verify dependencies before adoption; migration precedes adapter; adapter precedes race test; Docker evidence precedes closure.

## Current status
PR #1 hardening precondition complete; PostgreSQL/container milestone work not started.

## Decisions made
None yet; update ADR-0004 only if implementation changes its accepted storage split.

## Discoveries
Bootstrap's local environment had no Docker executable, so all runtime container behavior remains unverified.
Review found that the abbreviated product specification omitted important planned requirements. Those requirements are now normative in the authoritative specification rather than recoverable only from deleted planning prose. Cargo registry metadata confirmed `oxttl` 0.2.4 (MIT OR Apache-2.0, Rust 1.87) as the maintained standards parser used at RDF ingress.

## Risks
Transaction isolation/SQL shape may permit lost updates; container health may overstate readiness; image pinning may be blocked by registry access.

## Quality gates
Fast gate plus migration checks, real PostgreSQL two-writer CAS, HTTP/restart scenario, `docker compose config`, and Docker smoke tests.

## Test evidence
PR #1 hardening evidence is recorded here but is not PostgreSQL evidence: `./scripts/quality-gate.sh fast` passed on 2026-09-24; the three commit golden vectors passed three repeated runs; targeted tests cover Serde bypasses, malformed/canonical N-Quads, directory-publication paths, and server-controlled recording time. An independent invariant review found the four P1 fixes sound and prompted additional one-/two-parent vectors, validated `Commit` deserialization, and precise recording-time semantics. PostgreSQL/container evidence remains absent.

## Deferred work
Semantic validation/Jena integration, auth/resource limits, full RDF/skolemization, and benchmarks remain in `../tech-debt.md`.

## Completion criteria
PostgreSQL is the tested runtime ref store; exactly one real concurrent writer wins; HTTP state survives clean Compose restart; configurations and documentation match executed evidence.
