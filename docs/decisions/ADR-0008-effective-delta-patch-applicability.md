# Effective-delta patch applicability semantics

## Status
Accepted

## Context
Two accepted descriptions currently disagree. The product specification treats patch
application as pure set semantics: deleting an absent quad or adding a present quad is a
harmless idempotent no-op. The architecture's Level-1 validation, by contrast, checks
whether a deletion is applicable to the base state. Before cognitive workflows are built
we must decide, for production, what a write request means and what gets persisted, so
that history, diff, and blame never claim a change that did not modify the graph.

## Decision
Adopt a base-relative *effective-delta* rule at request/acceptance time:

1. resolve the expected base state for the target ref;
2. compute the effective delta (drop adds already present, drop deletes of absent quads);
3. validate the request against the base according to branch policy;
4. persist the **effective** canonical patch, so `PatchId`/`CommitId` reflect only what
   actually changed.

For protected/accepted refs (e.g. `main`, ADR-0010) the default policy is strict:

- deleting a quad that is not in the base state yields a typed base-mismatch error
  (a stale/precondition failure), not a silent no-op;
- adding a quad already present collapses to no change;
- a request whose effective delta is empty MUST NOT produce an accepted commit unless the
  caller explicitly asks for an auditable workflow event;
- the caller's requested intent is retained in the proposal/decision record (ADR-0014),
  never as an empty or misleading RDF commit.

Reconstruction and patch *application* remain set-based and idempotent (invariant 8 is
unchanged); this rule governs request validation and which patch is persisted, not how a
stored patch replays. Non-protected/import workflows may select a permissive policy that
tolerates no-ops, but still persists the effective delta.

## Alternatives considered
- **Pure set semantics everywhere.** Simplest, but lets commits assert changes that did
  nothing, corrupting diff/blame and inflating history.
- **Strict apply everywhere (reject every no-op).** Too rigid for idempotent bulk
  imports and agent retries; forces callers to pre-diff.

## Consequences
- Acceptance needs the resolved base state (reconstruction or a checkpoint), so this
  interacts with reconstruction bounds and checkpoints.
- Because the persisted patch is the reduced effective set, the recorded `PatchId` can
  differ from a naive hash of the raw request; the persisted effective patch is
  authoritative. This is why the rule is an ADR: it affects persistent identity.
- Property tests (P1) MUST cover: effective-delta idempotence, no-op collapse, and empty
  effective delta on a protected ref producing a typed error with no commit.
- Because identity depends on the resolved base, a property test MUST assert that the
  effective delta computed via a checkpoint equals the one computed via full
  reconstruction for the same base commit, so `PatchId`/`CommitId` cannot drift by
  acceleration path (upholds invariant 8).

## Gate
On a protected ref, an empty effective delta produces a typed precondition error and no
commit; a request containing only no-ops does not create RDF history.
