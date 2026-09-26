# Architecture

The ledger is a narrow version-control service for RDF knowledge. The authoritative scope and invariants are in [`docs/product-specs/cognitive-ledger.md`](docs/product-specs/cognitive-ledger.md).

## Dependency direction

```text
ledger-server -> ledger-api -> ledger-store -> ledger-core
                                  |              ^
                                  +-> ledger-rdf-+
ledger-testkit -------------------------------> all test-facing crates
```

`ledger-core` owns IDs, commit protocol objects, errors, and storage traits. `ledger-rdf` owns the restricted canonical RDF change model. `ledger-store` provides two `ImmutableStore` backends — filesystem (`FileStore`, single host) and shared PostgreSQL (`PostgresImmutableStore` with a verified commit index and same-graph ancestry enforcement, ADR-0012) — plus reconstruction, the graph authority rows (`PgGraphs`, ADR-0010: globally unique `graph_id`, immutable tenant binding, `refs`/`commit_index` foreign keys), the administrative filesystem→PostgreSQL migration (`FsToPgMigration`), and a pluggable ref store (filesystem or PostgreSQL `PgRefStore`, ADR-0007). `Ledger` holds an `Arc<dyn ImmutableStore>` and an `Arc<dyn RefStore>`; commit operations are version-neutral over `AnyCommit` (ADR-0009 dual read). `PostgresLedgerStore` composes the immutable store, graph authority and `WorkflowRepository` (ADR-0013: prepare/accept/reject as single transactions with lineage, ref events, decisions, outbox and idempotency) over one pool. With a database URL the server uses the shared PostgreSQL backend for refs and content; PostgreSQL refs can only target indexed commits (migration 0006). `ledger-rdf` also owns the pure effective-delta primitive (ADR-0008). `ledger-api` is the authenticated, graph-scoped HTTP adapter: it verifies bearer tokens (`auth`: OIDC/JWKS or a development HS256 authenticator that cannot serve non-loopback), maps verified roles to capabilities, computes the canonical request identity `sculpin-ledger-request/v1` (`request_identity`, golden-pinned), enforces resource limits and the stable error envelope, and routes every public write through `WorkflowRepository` (no route reaches `Ledger::commit` or the raw ref primitive; the shared server runs `V1Binding::Reject`). Its contract is `docs/api/openapi.json`. `ledger-server` composes infrastructure and refuses unsafe configurations (non-loopback without production auth, unvalidated acceptance without the explicit development switch). Infrastructure types never enter core.

## Runtime boundary
Accepted changes are durable before refs move. Projection is downstream and cannot roll history back. The Sculpin semantic validation/reasoning layer (currently pySHACL and Python reasoning workers; Jena is a possible implementation detail, not an architectural dependency) validates and reasons; Fuseki queries projections. The virtual A-box stays transient. Fluree is an optional external differential-test oracle only.

## Evolution
Reserved boundaries (`ledger-dag`, `ledger-merge`, `ledger-validation`, `ledger-projection`) become crates only when a milestone needs them. Architectural or invariant changes require an ADR. See [storage boundaries](docs/design/storage-boundaries.md).
