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
- [ADR-0015: Canonical HTTP request identity](ADR-0015-canonical-request-identity.md)
- [ADR-0016: Database identities, schema ownership and runtime least privilege](ADR-0016-database-identities-and-schema-ownership.md)
- [ADR-0017: Backup, restore and recovery semantics](ADR-0017-backup-restore-and-recovery-semantics.md)

## Phase 2 semantic validation coordination (P2)
See [Plan 0006](../exec-plans/completed/0006-phase2-semantic-validation.md).

- [ADR-0018: Canonical identity of SemanticExecutionContext v1, ValidationRecord v1 and the candidate state digest](ADR-0018-semantic-context-and-validation-record-identity.md)
- [ADR-0019: Validation freshness and the binding of acceptance to a validation record](ADR-0019-validation-freshness-and-acceptance-binding.md)

## Phase 3 accepted-state projection (P3)
See [Plan 0007](../exec-plans/completed/0007-phase3-accepted-state-projection.md).

- [ADR-0020: Accepted-state projection protocol: target graph identity, marker and write semantics](ADR-0020-accepted-state-projection-protocol.md)
- [ADR-0021: Projection state, stream leases and the projector database identity](ADR-0021-projection-state-leases-and-projector-identity.md)

## Phase 4 branches and cognitive workflows (P4)
See [Plan 0008](../exec-plans/completed/0008-phase4-branches-cognitive-workflows.md).

- [ADR-0022: Named branches: identity, lifecycle, policy and authorization](ADR-0022-named-branches-lifecycle-and-policy.md)

## Phase 5 diff and merge (P5)
See [Plan 0009](../exec-plans/active/0009-phase5-diff-and-merge.md).

- [ADR-0023: Merge lineage: integration commits, not ref jumps](ADR-0023-merge-lineage-and-integration-commits.md)
- [ADR-0024: Diff, merge base, structural conflicts, merge preview and stale-safe apply](ADR-0024-diff-merge-base-conflicts-preview-and-apply.md)
