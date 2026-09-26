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
- [x] Verify current image versions/digests and SQLx release from primary sources.
- [x] Write and test additive ref-schema migrations.
- [x] Implement PostgreSQL `RefStore` behind the existing core trait.
- [x] Add simultaneous two-connection CAS integration test.
- [x] Add HTTP commit/state/restart Compose scenario.
- [x] Resolve Fluree pin/API or record a specific external blocker.
- [x] Run independent architecture, invariant, test, and security reviews.
- [x] Execute and record all applicable gates.

## Dependency/order constraints
Verify dependencies before adoption; migration precedes adapter; adapter precedes race test; Docker evidence precedes closure.

## Current status
Complete (2026-09-25). PostgreSQL is the tested runtime ref store, the two-connection CAS race is proven against a real database, and HTTP state survives a clean container restart. Ready to close.

## Decisions made
- ADR-0007 (accepted): storage traits made async via `async-trait`; `FileStore` I/O backed by `spawn_blocking`; `Ledger` holds `Arc<FileStore>` + `Arc<dyn RefStore>`; `RefStore` reduced to a pure atomic CAS primitive; the ref-target-existence invariant moved up to `Ledger::advance_ref` so it is enforced identically for every backend. This is internal architecture, not a protocol change — canonical bytes/hashes and golden vectors are unchanged, so ADR-0004's storage split stands.
- Fluree reference: pinned `fluree/server:4.2.1` by digest but the container is NOT run in CI pending BUSL-1.1 license sign-off (recorded in `test/reference-images.lock`, gated behind the `differential` compose profile).

## Discoveries
- Verified image digests from the registry: `postgres:17.2-bookworm@sha256:3267c505…`, and corrected a latent pull failure — `stain/jena-fuseki:5.2.0` does not exist; pinned `5.1.0@sha256:b1d0c96f…` and moved Fuseki behind a `projection` profile (it belongs to Plan 0003, not this milestone).
- `oxttl` 0.2.4 (MIT OR Apache-2.0, Rust 1.87) confirmed as the maintained standards parser at RDF ingress.
- During this milestone a disk-full crash truncated `scripts/test-integration.sh` to 0 bytes; because an empty bash script exits 0, an automated run would have reported the integration stage green while doing nothing. The independent test and security reviews caught this. The script was restored and, on the first genuine run, a real bug surfaced: the shell-built commit body did not escape the double quotes inside RDF literals, producing malformed JSON. Fixed by constructing the request body with Python. Both were real defects found and fixed, not evidence fabrication.

## Risks
Addressed: transaction isolation/SQL shape (single predicated statements, proven by the two-connection race); container health readiness (compose `--wait` on healthchecks); image pinning (all images digest-pinned). Residual/deferred: no explicit HTTP body/operation-count cap beyond Axum's default 2 MB extractor limit (see Deferred work).

## Quality gates
`./scripts/quality-gate.sh fast` (doc-links, architecture dependency check, `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -D warnings`, `cargo test --workspace`) and `./scripts/test-integration.sh` (`docker compose config`, compose `--wait`, real-PostgreSQL CAS race, HTTP commit/state/restart). `./scripts/test-differential.sh` validates the deterministic seam and explicitly defers the live Fluree comparison.

## Test evidence
- Fast gate: `./scripts/check-fast.sh` passed on 2026-09-25 (exit 0) after review fixes.
- Real-PostgreSQL two-connection CAS race (`crates/ledger-store/tests/pg_cas_race.rs`, `#[ignore]`d, driven with `LEDGER_TEST_DATABASE_URL`): passed against a standalone `postgres:17.2` 5/5 repeated runs, and again against the compose database in the end-to-end script. Asserts exactly one winner + exactly one `HeadChanged`, plus the genesis `ON CONFLICT DO NOTHING` rival-loses case.
- Container HTTP + restart durability (`./scripts/test-integration.sh`, exit 0, `INTEGRATION OK`, 2026-09-25): genesis commit over HTTP → head advanced; second commit CAS'd from the genesis head → C2; `docker compose restart ledger`; after restart `GET /v1/refs/main` returned C2 and `GET /v1/states/C2` reconstructed both quads. Direct `psql` inspection confirmed `refs(default,main).head == C2`, proving the head lives in PostgreSQL (only the ledger container was restarted; postgres was not), while state reconstructs from filesystem objects on the `/data` volume.
- Independent reviews (2026-09-25): architecture and invariant reviewers found no issues (core crates infrastructure-free, sqlx optional/gated, CAS lost-update-safe, target-existence enforced uniformly, protocol unchanged). Test and security reviewers found the truncated `test-integration.sh` (fixed) and flagged deferred hardening items below.

## Deferred work
Semantic validation/Jena integration, auth, HTTP request body/operation-count limits (currently only Axum's default 2 MB extractor bound), full RDF/skolemization, and benchmarks remain in `../tech-debt.md`. `compose.yaml` is a development/CI harness only (dev credentials, `sslmode=disable`); production deployment must inject `LEDGER_DATABASE_URL` with managed secrets and TLS.

## Completion criteria
Met: PostgreSQL is the tested runtime ref store; exactly one real concurrent writer wins; HTTP state survives clean Compose restart; configurations and documentation match executed evidence.
