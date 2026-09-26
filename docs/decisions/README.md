# Architecture decision records

ADRs record accepted, expensive-to-reverse choices. Use `ADR-NNNN-title.md` with Status, Context, Decision, Alternatives considered, and Consequences. Changing a hard invariant or protocol requires a new superseding ADR; never rewrite an accepted decision silently.

- [ADR-0001: Rust and service boundaries](ADR-0001-rust-and-service-boundaries.md)
- [ADR-0002: Content-addressed immutable commits](ADR-0002-content-addressed-immutable-commits.md)
- [ADR-0003: RDF patch canonicalization](ADR-0003-rdf-patch-canonicalization.md)
- [ADR-0004: Immutable objects and mutable ref metadata](ADR-0004-storage-separation.md)
- [ADR-0005: Fluree differential-reference policy](ADR-0005-fluree-reference-policy.md)

- [ADR-0006: Finalize unreleased v1 parent and recording-time semantics](ADR-0006-finalize-unreleased-v1-parent-and-time-semantics.md)
- [ADR-0007: Async storage traits and pluggable ref store](ADR-0007-async-storage-traits-and-pluggable-ref-store.md)

## Phase 0 architectural consolidation (P0)
These records freeze the persistent-identity and atomicity decisions the later phases
depend on; see [Plan 0003](../exec-plans/completed/0003-phase0-architectural-consolidation.md)
and `product_development_plan.md`.

- [ADR-0008: Effective-delta patch applicability semantics](ADR-0008-effective-delta-patch-applicability.md)
- [ADR-0009: sculpin-cognitive-commit/v2 provenance envelope](ADR-0009-commit-v2-provenance-envelope.md)
- [ADR-0010: Graph, tenant, and ref identity model](ADR-0010-graph-tenant-ref-identity.md)
- [ADR-0011: Authenticated principal and temporal-field validation boundary](ADR-0011-principal-and-temporal-validation.md)
- [ADR-0012: Production immutable-store abstraction and default](ADR-0012-production-immutable-storage.md)
- [ADR-0013: Atomic acceptance transaction](ADR-0013-atomic-acceptance-transaction.md)
- [ADR-0014: Two-phase semantic-validation protocol and contracts](ADR-0014-semantic-validation-protocol.md)
