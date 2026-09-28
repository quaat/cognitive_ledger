# Plan 0008: Phase 4 — branches and cognitive workflows

Status: **complete — pending hosted CI and PR review** (started 2026-09-28). Branch `claude/p4-branches-cognitive-workflows`
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
- Phase 1–3 created any ref implicitly by a genesis acceptance. Keeping that would give a
  branch no creation event, source or policy, so 0012 limits genesis to `main` and adopts
  every existing ref (`origin = adopted`, one `adopted` event). This is a client-visible
  behaviour change (runbook); `pg_api`'s genesis test moved to `main`.
- A created branch's first ref event is a `genesis`-kind row without a decision (no
  proposal moved it). `verify`'s decision check exempts exactly that case: the version-1
  event of a branch whose `created` lifecycle event names the same head.
- Holding a second pool connection for the reachability walk while the creating
  transaction holds the source ref lock could exhaust the pool under load. The walk runs
  on the transaction's own connection (`GraphParents`), bounded by `TraversalLimits`.
- `projection_unconfigured_pending` counted every undelivered outbox row, so branch
  traffic would look like a projection backlog. It now counts `main` only (`PROJECTED_REF`).
  The metric's HELP text says so.
- The runtime needs `INSERT (protected)` on `refs` to record branch protection. The
  `refs_main_protected` CHECK plus the guard triggers keep that grant from un-protecting
  `main` or changing protection later.
- Trigger-raised violations surface as SQLSTATE 23000 (integrity), not P0001; the tests
  assert the class, not the text.

## Evidence
All on the qualification host (Linux 5.10, Docker; scratch PostgreSQL 15-bookworm and
17-bookworm), 2026-09-28. Candidate `83e5f3f` (+ `5779839`: backup harness graph choice).

| gate | result |
|---|---|
| `check-fast` (fmt, clippy `-D warnings`, unit tests, architecture, doc links) | pass on `83e5f3f` |
| `check-supply-chain` (advisories, bans, licenses, sources, SBOM) | pass |
| PostgreSQL 17 suites (`ledger-store --features postgres`, `ledger-api`, `--ignored`) | pass — incl. `pg_branches` 13, `pg_verify` 3, `pg_least_privilege` 19, `pg_validation_api` 21, `pg_api` 13 |
| PostgreSQL 15 suites (same) | pass (same counts) |
| `test-integration.sh` (compose, owner migrate → runtime/projector, Fuseki projection regression, branch e2e, verify) | `INTEGRATION OK` |
| `stress-branches.sh 100 3 4` (2 replicas) | `BRANCH STRESS GATE OK` — see below |
| `upgrade-p4.sh` (from `848ec28`, schema 11 → 12) | `UPGRADE-P4 OK`, run `20260928T072740Z` |
| `backup-restore.sh` (50 writers, 10 graphs, branch workload) | `BACKUP RESTORE OK` |
| Golden vectors | 21 request vectors (6 v1, 6 v2, 9 branch); Rust and the Python reference agree; no v1/v2 vector changed |
| Live Fluree branch differential | **deferred** (BUSL-1.1 sign-off pending) — not run, not counted |
| Hosted CI | see the PR |

**100-branch stress** (`scripts/stress-branches.sh 100 3 4`, final run `6aba194cb`,
`target/stress-branches/20260928T073518Z`): 100 branches × 3 commits (every fourth from
`main`'s root, the rest from the moving head; a head-created branch must start at or after
the `main` version its agent read before creating) while 4 writers land 20 commits on
`main`; every tenth agent duplicates each request to the second replica. 1 519 requests in
0.9 s. Delete-vs-accept (acceptance only, prepared beforehand): 0 landed before the
tombstone / 20 refused `BRANCH_DELETED` in this run (the previous run: 2 / 18; both
orders are proven deterministically by the forced-interleaving store tests); 20 prepares on
tombstones refused; 10 restores (duplicated across replicas) and 10 refused second restores
(`BRANCH_STATE_CONFLICT`); the list returned 101 branches. 88 duplicated pairs compared, 0
disagreements; 0 deadlocks; no unexpected error class (191 admission refusals `503
RESOURCE_LIMIT`, retried under the same key). Owner-side invariants hold for every branch
(version = events = 1 + landings, landings are ref events, branch point, lifecycle count and
status, tombstone head, outbox = accepted) and for `main` (Plan-0005 graph invariants);
unconfigured projection backlog 22 = `main` undelivered 22; `verify` clean.
Successful-operation latency p50 / p95 / p99 (ms): create 69.1 / 108.7 / 120.9, prepare
28.8 / 72.9 / 104.8, accept 23.6 / 45.9 / 58.8, delete 10.4 / 13.7 / 16.2, restore 4.9 /
6.9 / 7.7, branch reads 14.9 / 23.0 / 24.1. The first run (`6aba0feab`) failed only on its
own list check (default page 100 of 101 branches); the harness now pages with
`limit=1000` and races the acceptance only (the first run raced prepare, 0 / 20).

**Upgrade 0011 → 0012** (`scripts/upgrade-p4.sh`): previous release `848ec28` built from git
and deployed as owner/runtime/projector with a real Fuseki; populated through its API (two
tenants, `dev` ref, rejected/pending proposals, validations, validated acceptances,
projection to lag 0, then a backlog). After `migrate`: 0001–0011 checksums untouched, 0012
checksum = sha384 of the file, all 17 pre-upgrade tables byte-identical, 6 refs adopted
(one `adopted` event each at their head/version), 85 recorded idempotency keys replay
identically without calling the validator (51 Phase-2 workload keys: 27 prepare, 21 accept,
3 reject; 34 Phase-3 keys), VERIFY OK; branch create (head and historical),
three validated cycles, delete/restore with head kept and lifecycle replays on an upgraded
graph with `main` unchanged; the new projector consumed the old backlog (lag 0, target =
accepted state); unconfigured backlog 20 = `main`-only (7 non-`main` rows excluded, metric
agrees); previous server refuses 0012 (ahead), new server and projector refuse 0011
(behind); clean 0012 install and upgraded 0012 identical (schema dump and ownership).

**Backup/restore**: branch workload (created, historical, deleted, deleted+restored) before
the backups, under a live write load; both the logical dump and the base backup restore
the branch and lifecycle rows identically (subset of live) with the expected lifecycle
states, VERIFY OK, identical states served; a restore missing `branches_guard` is refused
at start-up (fifth drift case).

**Reviews** (independent Opus, read-only, on `cc4219b`): lifecycle/invariants, DAG,
storage/concurrency, security, API/idempotency, tests, projection — no P0. P1s fixed in
`83e5f3f`: reachability walk held the source-ref share lock (walk now precedes the
transaction; unknown/foreign refused without walking); deleted-branch database guards read
the branch row without a lock (raw writes racing an uncommitted delete were admitted; now
`FOR SHARE`, proven by a forced race test); refs of graphs activated from
`bootstrap`/`importing` had no branch row (`graphs_adopt_refs`); `require_distinct_reviewer`
was bypassable by varying delegation or principal type (now accountable parties); two
lifecycle races were not concurrent in the tests (now forced in both orders under held row
locks); the deleted-head guard test never reached the guard; policy-flag order not pinned
by a vector. Also fixed: `branch_events.reason` bound (1 024) below the API's (4 096), a
latent database error; verifier gaps (numbering, event positions, tombstone position,
restore position, created event head) with a bypass test each; corruption no longer
reported as "unreachable". Documented instead of changed (tech-debt): pagination cursors,
N+1 walk cost, outbox scan index, confusable names, runtime trusted-writer residuals.
Codex (`codex exec -s read-only`) on `5779839`: **no P0/P1**; three P2 — lifecycle history
ignored `limit` (fixed: latest `limit` events), the stress accepted any historical `main` head
as a head-created branch point (fixed: must be ≥ the version read before creating), and
activating a raw import leaves verifier findings (documented: the pre-existing import gap,
tech-debt).

## Sub-agent decomposition (§42)
Main session owns the ADR, migration, verifier, repository, request identity and API.
`ledger-dag` implemented by a bounded agent (crate only). Read-only reviewers at the end:
lifecycle/invariants, DAG, storage/concurrency, security/authorization, API/idempotency,
tests, projection interaction; then Codex on the final candidate.
