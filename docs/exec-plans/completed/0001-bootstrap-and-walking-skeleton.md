# Plan 0001: Bootstrap and walking skeleton

## Goal
Establish the durable project/agent harness and prove a deterministic, persistent linear RDF ledger with CAS-protected `main` and a minimal HTTP surface.

## Scope
- Source-of-truth documents, ADRs, provider-neutral skills, Claude/Codex guidance, hooks, scripts and CI.
- Canonical RDF patch and commit identities, immutable filesystem objects, persistent single ref, reconstruction, and HTTP health/commit/state endpoints.
- Static Docker topology and a Docker-CI integration seam.

## Non-goals
Branch creation, merge commits, SPARQL, SHACL/reasoning, query indexing, S3, signing, GC, distributed consensus, and performance optimization.

## Relevant invariants
All 14 invariants in [the product specification](../../product-specs/cognitive-ledger.md#hard-invariants), especially immutable/content-derived objects, existing parents, CAS refs, deterministic reconstruction, no blank nodes, and stable protocol bytes.

## Assumptions
- Rust 1.89.0 is available; edition 2024 is selected.
- Filesystem durability is sufficient for this milestone; PostgreSQL CAS remains a production follow-up.
- The service has a single process writer in this slice; persistent refs use a lock file plus atomic rename.

## Work breakdown
- [x] Inspect repository and environment.
- [x] Establish specification, architecture, quality docs, and ADR framework.
- [x] Establish agent/Claude harness and shared safety scripts.
- [x] Establish Rust workspace, scripts, CI, and Compose definitions.
- [x] Implement protocol types and golden vectors.
- [x] Implement immutable filesystem storage and persistent CAS ref.
- [x] Implement linear reconstruction and acceptance scenario.
- [x] Implement minimal HTTP surface.
- [x] Obtain bounded independent reviews and resolve confirmed defects.
- [x] Execute applicable local gates and record evidence.

## Dependency/order constraints
Protocol decisions precede implementation. Storage depends on core/RDF abstractions. API depends on the ledger service. Reviews follow implementation; completion follows evidence.

## Current status
Complete and archived on 2026-09-24. Plan 0002 now covers PostgreSQL-backed CAS and live Docker integration.

## Decisions made
- Empty `main` is represented as no ref; the first commit has no parent.
- Patch bytes are a versioned UTF-8 line protocol over absolute-IRI/literal N-Quads, sorted by operation and quad.
- Commit bytes are a versioned, length-prefixed binary envelope, avoiding JSON ambiguity.
- Acknowledged objects and refs are filesystem-persistent; ref replacement is atomic within one filesystem.

## Discoveries
- Repository initially contained only `README.md` and `plan.md` on branch `work` at commit `803081b`.
- Rust/Cargo 1.89.0 were active; newer installed toolchains existed but 1.89 was used and pinned.
- Docker and Compose were absent, so runtime integration and Fluree tests could not run locally.
- Direct network access and the Codex manual helper were blocked (proxy 403/`ENETUNREACH`); available in-session Codex guidance confirms `AGENTS.md`, `.codex/config.toml`, skills, plugins, and hooks as distinct surfaces. Repository hooks were therefore not asserted as auto-active.
- Official Claude documentation could not be retrieved; configuration uses the documented project `.claude/settings.json`, rules, agents, and skill symlink conventions and is validated as JSON/filesystem structure.

## Risks
- Host-local OS file locking needs database-backed semantics before horizontal deployment.
- The restricted RDF parser deliberately supports a safe walking-skeleton subset, not general RDF syntax.
- Fluree image digest is unresolved and differential execution is blocked until it is pinned.

## Quality gates
See [quality gates](../../quality/quality-gates.md). Local required Rust, determinism, architecture, docs, hook, and static configuration checks passed. Docker-only gates are explicitly deferred.

## Test evidence
Executed 2026-09-24:
- `./scripts/quality-gate.sh fast` — passed (format, clippy, workspace tests, docs links, architecture checks, config checks).
- Independent reviews found and prompted fixes for missing patch reachability validation, strict literal escapes, idempotent concurrent object writes, stale crash locks, a true simultaneous-writer test, and plugin/evidence accuracy.
- `for i in 1 2 3; do cargo test -p ledger-core --test golden --quiet; done` — passed three times.
- `cargo test -p ledger-testkit --test walking_skeleton` — passed, including stale CAS and restart reconstruction.
- `cargo test -p ledger-api` — passed, including HTTP `409 HEAD_CHANGED`.
- `python3 /opt/codex/skills/.system/skill-creator/scripts/quick_validate.py <skill>` for all canonical skills — passed.
- `python3 -m json.tool .claude/settings.json` — passed. Official plugin and all seven skill validators passed with PyYAML installed into an ephemeral validation directory.
- Compose YAML was parsed structurally with PyYAML; semantic `docker compose config` was unavailable.
- `docker compose config` / Fluree differential scenario — not executed: Docker executable unavailable.

## Deferred work
Tracked in [tech debt](../tech-debt.md): PostgreSQL CAS, complete RDF grammar/skolemization, pinned Fluree digest and adapter, Fuseki validation integration, OpenAPI, supply-chain tooling, and benchmark baselines.

## Completion criteria
All locally applicable gates pass; the acceptance scenario persists and reconstructs two states and rejects stale HEAD; harness/navigation is coherent; Docker limitations and unimplemented work are explicit.
