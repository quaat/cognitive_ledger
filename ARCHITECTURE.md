# Architecture

The ledger is a narrow version-control service for RDF knowledge. The authoritative scope and invariants are in [`docs/product-specs/cognitive-ledger.md`](docs/product-specs/cognitive-ledger.md).

## Dependency direction

```text
ledger-server -> ledger-api -> ledger-store -> ledger-core
                                  |              ^
                                  +-> ledger-rdf-+
ledger-testkit -------------------------------> all test-facing crates
```

`ledger-core` owns IDs, commit protocol objects, errors, and storage traits. `ledger-rdf` owns the restricted canonical RDF change model. `ledger-store` provides two `ImmutableStore` backends — filesystem (`FileStore`, single host) and shared PostgreSQL (`PostgresImmutableStore` with a verified commit index, ADR-0012) — plus reconstruction and a pluggable ref store (filesystem or PostgreSQL `PgRefStore`, ADR-0007). `Ledger` holds an `Arc<dyn ImmutableStore>` and an `Arc<dyn RefStore>`; commit operations are version-neutral over `AnyCommit` (ADR-0009 dual read). `ledger-api` adapts HTTP to the application service; `ledger-server` composes infrastructure. Infrastructure types never enter core.

## Runtime boundary
Accepted changes are durable before refs move. Projection is downstream and cannot roll history back. The Sculpin semantic validation/reasoning layer (currently pySHACL and Python reasoning workers; Jena is a possible implementation detail, not an architectural dependency) validates and reasons; Fuseki queries projections. The virtual A-box stays transient. Fluree is an optional external differential-test oracle only.

## Evolution
Reserved boundaries (`ledger-dag`, `ledger-merge`, `ledger-validation`, `ledger-projection`) become crates only when a milestone needs them. Architectural or invariant changes require an ADR. See [storage boundaries](docs/design/storage-boundaries.md).
