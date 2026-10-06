# Plan 0009: Phase 5 — diff and merge

Status: **in progress** (started 2026-10-06). Branch `claude/p5-diff-and-merge` from `main` at
`5216bcea0faf530faace98e92bc33adf6222194b` (the PR #9 merge; Phase 4 complete, see
[Plan 0008](../completed/0008-phase4-branches-cognitive-workflows.md)). No gate is reported as
passed until it is executable and has run.

## Goal
Deterministic comparison and merging of divergent branches while preserving immutable DAG
history, migration 0009's direct-first-parent movement invariant, the target branch's
policy, Phase-2 semantic validation and stale-safe acceptance. Decision records:
[ADR-0023](../../decisions/ADR-0023-merge-lineage-and-integration-commits.md) (integration
commits; what "fast-forward" means here) and
[ADR-0024](../../decisions/ADR-0024-diff-merge-base-conflicts-preview-and-apply.md) (merge
base, diff, conflicts, strategies, preview token, apply).

## Sequence (product plan §§14–18, Phase 5)
1. ADR-0023 / ADR-0024, independently reviewed before implementation.
2. `ledger-dag`: `merge_base` (unique / ambiguous / unrelated), `ahead_behind`,
   classification inputs; property tests against a slow reference model (linear, fork,
   nested fork, diamond, repeated merge, criss-cross, deep DAG, missing parent, cycle).
3. `ledger-rdf`: deterministic `diff`, structural keys, summaries.
4. `ledger-merge` (new, infrastructure-free): classification, three-way structural merge,
   strategies `abort | take-target | take-source | union`, conflict report; property tests.
5. Migration 0013 (ADR-0024 list: `merge_proposals`, `ref_events` operation + shape,
   merge/advance and decision triggers, idempotency, grants), verifier; repository
   `merge_preview` (read-only), `merge_propose` (token-checked, persists candidate +
   proposal + merge row) and `merge_apply` (sorted two-ref lock protocol, stored-value
   staleness, target policy, validation binding, `merge` ref event); ordinary accept keeps
   refusing merge candidates.
6. Preview token v1 and merge request identity: Rust goldens + Python reference.
7. HTTP: `POST …/merges/preview` (read), `…/merges/propose` (propose), `…/merges/apply`
   (review); OpenAPI.
8. Forced-interleaving merge races; repeated merge; `main` merge projected by the Phase-3
   projector; Virtual A-Box-dependent validation (environment change → refusal).
9. Upgrade 0012 → 0013, backup/restore, integration, stress regression; reviews; Codex.

## Non-goals
Checkpoints and reconstruction optimization (Phase 6), GC, branch projection, mutable
policy, per-branch ACLs, S3, AI/heuristic conflict resolution, rebase/history rewriting,
virtual merge bases for criss-cross histories, ref jumps (ADR-0023 option B). Live Fluree
differential: deferred (BUSL-1.1 approval pending) — not run, not counted; internal
reference-model scenarios replace it.

## Invariants
All Phase 0–4 invariants and identities unchanged; migration 0009 unchanged. Added: (a) a
merge never moves a target except to an integration commit whose parent 0 is the target
head; (b) `ALREADY_EQUAL`/`ALREADY_CONTAINED` create nothing; (c) merge base is unique or
the merge is refused; (d) diff and merge are deterministic functions of their inputs;
(e) preview has no accepted-state effect; (f) apply refuses any movement since preview
(`MERGE_STALE`) and never recomputes; (g) the target branch's policy and the deployment
floor govern every merge; (h) `reconstruct(I) = merged state` of the preview; (i) a
completed merge request replays before any recomputation.

## Persistent data changes
Migration 0013 (ADR-0024): `merge_proposals`, `ref_events.operation += merge`, idempotency
operations/results, verifier, runtime grant.

## Quality gates
`check-fast`, `check-supply-chain`, PostgreSQL 15 + 17 suites, property DAG suite, merge
concurrency suite, integration (Fuseki regression and `main` merge projection), upgrade
0012 → 0013, backup/restore, hosted CI; independent reviews (DAG, lineage invariant, RDF
diff/conflicts, storage/concurrency, semantic validation, security, idempotency,
projection, tests); Codex on the final candidate.

## Roadmap reconciliation (recorded 2026-10-06)
See `product_development_plan.md` "Roadmap reconciliation": Phase-2 ledger infrastructure is
complete; the live Sculpin semantic service is an external product-integration prerequisite
carried into Phase 8 / production qualification.

## Discoveries
- Pre-implementation reviews (architecture, invariant, storage/concurrency; 2026-10-06)
  found no fault with the integration-commit choice and the merge-base/merge-rule
  definitions, and required: a sorted two-ref lock protocol at apply (opposite applies
  otherwise both commit and leave a permanent criss-cross — rated P0 by the storage
  reviewer), database enforcement that ordinary accept cannot install merge candidates,
  replacing `ref_events_genesis_shape`, stored digest/heads for lock-time comparisons, a
  `NO_CHANGE` class (two-way sync otherwise loops forever on empty integrations, and empty
  commits contradict ADR-0008), a read-only preview split from a persisting propose
  (product spec: preview MUST be side-effect free), a fixed integration-commit envelope, an
  explicit `base` for criss-cross, the `structural-slot/v1` algorithm id, prepare limits on
  merged states, and admission control. All folded into ADR-0023/0024 before code
  (ADR-0024 "Review resolutions"); the product spec's merge paragraph is amended.
- Option A (stepping the target through the source's commits) is incompatible with
  `decisions_one_per_candidate` / `proposals_candidate_unique` and verify's
  one-decision-per-event invariant, and would accept intermediate states never validated
  under the target's policy (ADR-0023).

## Evidence
(filled as gates run)
