# Two-phase semantic-validation protocol and contracts

## Status
Accepted (design); the `ledger-validation-protocol` crate and Sculpin adapter are
implemented in Phase 2.

## Context
The ledger must coordinate semantic acceptance of cognitive changes but must not implement
or embed semantics (invariants 13–14). It also must not develop a hard runtime dependency
`ledger → Sculpin → ledger` at write time, and it must let the same immutable candidate be
revalidated later against a different semantic context (so validation results cannot live
in the commit). Sculpin today validates in-process (pySHACL + Python reasoning) via
KB-scoped tools, exposes deterministic `urn:exodus:kb:<id>` graphs, versioned SHACL shape
sets with `shapes_hash`, and reasoning `source_graph_hash`; it has no standalone
validation endpoint, no aggregate KB-revision identity, and no Virtual A-Box contract yet.

## Decision
Adopt the two-phase protocol and coordination contracts specified in
[validation-protocol](../design/validation-protocol.md):

- **Prepare** creates an immutable candidate commit (ADR-0009) with an effective patch
  (ADR-0008) against a resolved base, moving no accepted ref. Prepare is idempotent under
  `Idempotency-Key` (ADR-0013) because `recorded_at` is server-assigned and
  identity-bearing.
- **Accept** requires an immutable `ValidationRecord` and runs the atomic acceptance
  transaction (ADR-0013).
- `SemanticExecutionContext`, `ValidationRecord`, `ProposalRecord`, and `DecisionRecord`
  are versioned contracts; validation and semantic context are referenced by candidates
  and decisions, never embedded in hashed commit bytes.
- The ledger stays validator-agnostic: it calls a validation service (to be exposed by
  Sculpin) and stores its immutable record. Whether Sculpin uses Jena or pySHACL is
  opaque.

## Alternatives considered
- **Synchronous embedded validation.** Would require the ledger to run or hard-depend on a
  semantic engine at write time; violates the boundary and couples availability.
- **Single-phase accept (validate then move ref in one call).** Prevents revalidating an
  unchanged candidate against a new context and re-introduces the runtime dependency.
- **Store validation/semantic context inside the commit.** Rejected in ADR-0009; breaks
  revalidation and bloats identity.

## Consequences
- Phase 2 adds a `ledger-validation-protocol` crate (protocol models and coordination
  only, no Jena) and a Sculpin validation adapter; Sculpin must expose a synchronous
  validation endpoint, a stable KB-revision/base-graph digest, and a Virtual A-Box
  identification contract (all defined-by-contract here because they do not exist yet).
- Depends on ADR-0009/0010 (candidate and graph identity) and ADR-0013 (acceptance).
- The required test scenarios (valid/invalid SHACL, reasoning-derived, A-box-dependent,
  stale external/ontology, validator-unavailable) become Phase 2 gates.
