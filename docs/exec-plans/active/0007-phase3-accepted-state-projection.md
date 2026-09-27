# Plan 0007: Phase 3 — accepted-state projection

Status: **in progress** (started 2026-09-27). Branch `claude/p3-accepted-state-projection` from
`main` at `0a56092484ba890df3cf43f297e690db1132cc4b` (the PR #7 merge; Phase 2 complete, see
[Plan 0006](../completed/0006-phase2-semantic-validation.md)). No gate is reported as passed
until it is executable and has run.

## Goal
Reliably project the ledger's **accepted** cognitive state into Sculpin's query environment
(Fuseki) while the ledger stays authoritative, and make projection lag, failure and recovery
explicit. Not general event streaming: a narrow projection subsystem consuming the existing
`projection_outbox`.

## Scope (slice 1, then slice 2)
1. ADR-0020 (projection protocol: cognitive graph IRI, marker, conditional full-state write,
   decision table, error classes) and ADR-0021 (projection state, stream leases, projector
   identity).
2. `crates/ledger-projection` (infrastructure-free): target graph identity, marker model and
   parsing, the ADR-0020 decision function, `ProjectionClient` trait, error taxonomy, frozen
   vectors.
3. Migration 0011: `projection_state`, guard triggers, `ref_events` FK target,
   `ledger_grant_projector`, runtime grant re-issued with SELECT on `projection_state`; schema
   verifier (tables, constraints, CHECKs, NOT NULL, triggers, functions) and a projector
   identity model; PostgreSQL 15 + 17.
4. `ProjectionRepository` (`ledger-store`): enable/disable (owner), claim with lease,
   acknowledge / fail under lease fencing, status and lag.
5. `crates/ledger-projection-fuseki`: hardened HTTP adapter (configured endpoints only,
   https in production, no redirects, no proxy, timeouts, bounded bodies, credentials from a
   file and never logged, error classification), SPARQL builders for the conditional write,
   rebuild, marker read and the transactional probe.
6. `apps/ledger-projector`: startup verification (schema, projector identity, target
   transactional probe), bounded worker concurrency, claim loop, backoff, graceful shutdown,
   `/health`, `/ready`, `/metrics`; `rebuild`, `verify` and `status` subcommands.
7. `ledger-admin projection enable | disable` (owner).
8. Tests: unit (IRI vectors, marker parsing, decision table), PostgreSQL (claim race, lease
   expiry, fencing, ordering, least privilege, verifier drift), real Fuseki (genesis,
   advance, duplicate, outage and catch-up, crash before/after target commit with failpoints,
   lost/corrupt marker → rebuild, two workers, multiple graphs, tenant isolation).
9. Slice 2: upgrade 0010 → 0011 harness from the Phase-2 release with a populated backlog;
   compose integration with Fuseki; reviews.

## Non-goals
General branches, branch APIs, merge / merge preview / conflicts, checkpoints, S3, GC,
incremental RDF-patch projection, projecting anything other than accepted state, a public
projection HTTP API (status is CLI + metrics first), live Sculpin integration.

## Invariants
All Phase 0–2 invariants unchanged; existing canonical goldens unchanged. Added: (a) projection
never changes ledger history and acceptance never waits for projection; (b) the target marker
never moves backwards and a version is marked delivered only when the target represents it or
a later version; (c) a stream projects only into its own cognitive graph, and no two streams
share one; (d) nothing but accepted state at the stream's ref is written; (e) the ledger never
reads state back from Fuseki.

## Persistent data changes
Migration 0011 only (ADR-0021). No content migration; existing outbox rows stay pending until
a stream is enabled.

## Security boundary
Projection targets are deployment configuration (endpoint, dataset, credential file), never
request data. Distinct `ledger_projector` database role (ADR-0021). The HTTP server gains no
projection write privilege.

## Failure semantics
Target unavailable → retry with backoff; the ledger keeps accepting, the backlog grows and
is observable. Permanent target errors → stream `blocked` with a stable code. Marker ahead of
the ledger → `rebuild_required`. Crash anywhere → lease expiry and idempotent re-run.

## Migration impact
Stop projectors (none exist before) → owner `ledger-admin migrate --runtime-role …
--projector-role …` → new server and projector builds. A 0010 build refuses 0011 (`ahead`),
a 0011 build refuses 0010 (`behind`).

## Affected crates
new `ledger-projection`, `ledger-projection-fuseki`, `apps/ledger-projector`; `ledger-store`
(migration, verifier, repository); `apps/ledger-server` (`ledger-admin projection`);
`scripts/check-architecture.py`; docs.

## Quality gates
`check-fast`, `check-supply-chain`, PostgreSQL 15 + 17 suites, real Fuseki integration,
projection fault suite, compose integration, upgrade 0010 → 0011; Phase-2 suites stay green.

## Discoveries
- 2026-09-27, pinned `stain/jena-fuseki:5.1.0`, TDB2 dataset from an explicit assembler
  (`fuseki-server --config`): a multi-operation SPARQL Update whose last operation fails
  (`LOAD <urn:…>`) returns 500 and leaves no trace of its earlier `INSERT` (one transaction);
  the ADR-0020 guarded replace moved v1 → v2, and a stale v1 write and a duplicate v2 write
  were no-ops. Anonymous update on the named endpoint → 401; update on the dataset root with
  only named endpoints → 400; anonymous query → 200.
- The image's entrypoint creates TDB1 datasets and the default dataset templates add
  unnamed update/GSP endpoints on the dataset root; the ledger's compose/test Fuseki
  therefore uses its own assembler (TDB2, named `query` and `update` endpoints only).
- `/fuseki/configuration` must be writable in the webapp build; the explicit `--config`
  file avoids it.

## Evidence
(filled as slices land)

## Sub-agent decomposition (§42)
Main session owns the protocol crate, the migration, the verifier and the repository.
Bounded read-only reviewers per slice (architecture, projection correctness,
storage/concurrency, security, fault recovery, tests, Sculpin boundary).
