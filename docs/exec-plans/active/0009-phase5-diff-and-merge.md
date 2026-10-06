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
- Implementation reviews (DAG, RDF diff/conflicts, storage/concurrency, semantic
  validation, security/idempotency, tests, projection; 2026-10-06): no P0. P1s fixed in
  `ad75e17`: a UNIQUE preview token made a rejected merge un-re-proposable (500); a silent
  `NO_CHANGE` lost a take-target/convergent resolution, so a later merge could reapply what
  was set aside (rule amended: empty integration commit unless the source has no net
  change from the base); four-eyes could be laundered through a merge (the target's
  distinct-reviewer rule now also excludes the proposers of the source-only commits,
  `merge_proposals.source_parties`); forced merge races were missing; the
  `ledger-store -> ledger-merge` `Cargo.lock` entry was uncommitted (fixed in `4ab6251`).
  P2s fixed: decided-before-stale reporting, propose lost-response replay, preview patch
  reuse under locks, independent verify recomputation, weighted admission (3 slots),
  pre-permit replay. Remaining P2s are in tech-debt (inline compute memory and per-tenant
  fairness, conflict-report byte budget, per-strategy token).
- Codex on the final candidate `60c918f` (read-only, 2026-10-06): **no P0/P1**. Two P2s,
  both fixed:
  - propose classified the recomputation before its stored-result replay check, so a
    lost-response retry racing its own apply could get `MERGE_NOTHING_TO_DO`;
  - `verify` did not recompute `source_parties`.

  The verify fix has a tamper test (erase, then restore). At `beece09` the replay
  reordering had no forced-race test, because failpoints only inject errors and no
  deterministic pause existed.
- Closure work after `beece09` (2026-10-06), summarized in ADR-0024 "Conflict report byte
  budget and replay before refusal":
  - **Conflict-report byte budget.** `LEDGER_LIMIT_MERGE_CONFLICT_REPORT_BYTES` defaults to
    2 MiB; values outside 1 KiB..=64 MiB are refused. The budget is enforced while the
    report is collected in `ledger-merge` (`ReportLimits`, `three_way_reported`), before
    any JSON exists. The response gains `conflicts_truncated`. The merged state, the count,
    the token and the candidate are unchanged under any budget.
  - **Deterministic propose-replay race.** The non-default `ledger-store` feature
    `test-hooks` adds a pause between the first stored-result lookup and the
    recomputation. Only that crate's test targets enable it, through a self
    dev-dependency; `cargo tree` shows that no binary's normal graph has it. The forced
    test found a residual defect in `beece09`: a retry carrying an explicit `base` got
    `INVALID_MERGE_BASE` after the original had been applied, because recomputation
    errors were returned before the second replay lookup. Propose now replays before
    **any** refusal. Two mutations (the `beece09` ordering, and no second lookup) each
    fail the test.

## Evidence
Code under test: `ad75e17` (implementation and review fixes). Every gate below ran on 2026-10-06
on this workstation; nothing is reported that did not run.
- `check-fast` (fmt, clippy, unit and property tests, the 6 merge-preview token goldens
  plus the Python reference, 26 request goldens plus the Python reference): exit 0.
- `check-supply-chain`: advisories, bans, licenses and sources ok; SBOMs generated.
- PostgreSQL 17 and 15 (`--ignored` suites): `ledger-store` 120 passed per version (including
  `pg_merge` 24: strategies, recorded resolutions, delete vs modify, distinct parties,
  reject then re-propose, refusals write nothing, crash atomicity at every merge failpoint,
  raw-SQL refusals by the 0013 triggers, and forced races — opposite applies, apply vs
  source accept in both orders, two applies of one proposal, apply vs target deletion in
  both orders, a three-branch ring, propose paused while the target moves; and `pg_verify`
  per-check merge tampering); `ledger-api` 35 passed per version (validated merge with
  Virtual A-Box environment change, `VALIDATION_REQUIRED`, `LINEAGE_MISMATCH`, 403, 404).
  0 failed.
- Property DAG suite: `ledger-dag` merge base against the cubic reference oracle (100
  seeds plus fixtures: linear, fork, nested fork, diamond, repeated merge, criss-cross,
  deep, missing parent, cycle); `ledger-merge` three-way properties (600 seeds); both run
  in `check-fast`.
- `test-integration.sh`: `INTEGRATION OK`; merge of `agent/it-task` into `main` as `[C2, B1]`
  (preview read-only, ordinary accept refused, apply), projected by the unchanged Phase-3
  projector (marker v3, verify CONSISTENT), repeat contained; `VERIFY OK`.
- `upgrade-p5.sh`: `UPGRADE-P5 OK` (previous release `5216bce`, schema 12 → 13; run
  `target/upgrade-p5/20261006T074003Z`): pre-upgrade rows byte-identical, 0001–0012
  untouched, every recorded key replays identically, a branch created by the previous
  release merged into `main` and projected, version skew refused, clean and upgraded 0013
  identical.
- `backup-restore.sh`: `BACKUP RESTORE OK` (`target/backup/20261006T074045Z`); dump and
  basebackup both restore the merge proposal and merge event; the drift refusals hold.
- `stress-branches.sh`: `BRANCH STRESS GATE OK` (`target/stress-branches/20261006T074208Z`),
  including the new verify check "every merge proposal recomputes from the DAG and
  immutable states (0 violations)".
- Not run: live Fluree differential (BUSL-1.1 approval pending; not counted), live Sculpin
  semantic service (validation uses the Phase-2 contract with a fake validator), hosted CI
  (recorded after push).
