# Plan 0008: Phase 4 — branches and cognitive workflows

Status: **in progress** (started 2026-09-28). Branch `claude/p4-branches-cognitive-workflows`
from `main` at `848ec28bfd48ffc7f0a1257058b21dc68a204ebc` (the PR #8 merge; Phase 3 complete,
see [Plan 0007](../completed/0007-phase3-accepted-state-projection.md)). No gate is reported as
passed until it is executable and has run.

## Goal
Durable named branches as lightweight cognitive workspaces, with audited lifecycle,
authorization and policy, preserving the proposal → validation → acceptance model, without
merge. Decision record: [ADR-0022](../../decisions/ADR-0022-named-branches-lifecycle-and-policy.md).

## Scope
1. ADR-0022 (identity, lifecycle, historical branching, tombstone deletion, restore, policy
   v1, authorization, audit, idempotency, proposals, projection).
2. `crates/ledger-dag` (infrastructure-free): `is_ancestor`, `first_parent_history`,
   `ancestors`, bounded (visit limit, deadline), cycle and missing-parent detection, a
   `ParentProvider` trait; generated tests against a brute-force oracle.
3. Migration 0012 (additive): `branches`, `branch_events`, lifecycle guard and audit
   triggers, deleted-branch write refusal triggers, `main`-protected CHECK, idempotency
   operations/results for branch operations, runtime grant re-issued; verifier coverage;
   PostgreSQL 15 + 17.
4. Repository: branch create (from head / reachable history), delete, restore, get, list,
   lifecycle and movement history; prepare/accept/reject honour status and policy
   transactionally; genesis only for `main`; `verify` updated.
5. API: `POST/GET /v1/graphs/{graph}/branches`, `GET …/branches/status?name=`,
   `GET …/branches/history?name=`, `POST …/branches/delete`, `POST …/branches/restore`;
   request identity `sculpin-ledger-branch-request/v1` (goldens + Python reference);
   OpenAPI.
6. Projection: observability counts only projection-eligible refs (`main`, protocol v1).
7. Tests and qualification: store and API suites, the C100 → C101..C103 cognitive workflow,
   historical branching (reachable / unreachable / foreign / unknown), lifecycle races
   (accept vs delete, restore vs accept, delete vs prepare), 100-branch stress on two
   replicas, upgrade 0011 → 0012 from the Phase-3 release, backup/restore regression,
   Phase-3 Fuseki regression.

## Non-goals
Merge-base, three-way merge, conflicts, merge preview, merge commits, checkpoints,
incremental projection, S3, GC, mutable branch policy, per-branch ACLs, projection of
non-`main` refs. Live Fluree branch differential: deferred (BUSL-1.1 sign-off pending,
`docs/exec-plans/tech-debt.md`); not run, not counted.

## Invariants
All Phase 0–3 invariants and canonical identities unchanged. Added: (a) a deleted branch's
head never moves and it receives no new proposal; (b) branch history (ref events, lifecycle
events, proposals, decisions) is never removed; (c) a branch point is the source head or
reachable from it within the same graph; (d) `main` is always protected, never deleted, never
created by branch creation; (e) branch policy only tightens the deployment floor; (f) every
lifecycle change is exactly one audited event and idempotent under its key.

## Persistent data changes
Migration 0012 only (ADR-0022). Existing refs are adopted as active branches.

## Migration impact
Owner `ledger-admin migrate --runtime-role … --projector-role …`; a 0011 build refuses 0012
(`ahead`), a 0012 build refuses 0011 (`behind`); the projector needs no new grants.
Behaviour change: genesis acceptance on a new non-`main` ref is refused (create first).

## Quality gates
`check-fast`, `check-supply-chain`, PostgreSQL 15 + 17 suites, Docker integration, Phase-3
Fuseki regression, 100-branch stress, historical branching, lifecycle races, upgrade
0011 → 0012, backup/restore regression, hosted CI.

## Discoveries
(recorded as work lands)

## Evidence
(filled as gates run)

## Sub-agent decomposition (§42)
Main session owns the ADR, migration, verifier, repository, request identity and API.
`ledger-dag` implemented by a bounded agent (crate only). Read-only reviewers at the end:
lifecycle/invariants, DAG, storage/concurrency, security/authorization, API/idempotency,
tests, projection interaction; then Codex on the final candidate.
