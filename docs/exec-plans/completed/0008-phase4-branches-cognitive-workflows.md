# Plan 0008: Phase 4 — branches and cognitive workflows

Status: **complete / merge-ready** (started 2026-09-28; first closed 2026-10-05 on final code
`46d3eb7`; **reopened by a post-closure GitHub Codex finding and re-closed 2026-10-06 on final
code `599ca07`** — see "Post-closure finding").

| | |
|---|---|
| Phase-4 implementation | complete |
| Phase-4 merge-ready | yes (gates re-run on `599ca07`; exact-head Codex no P0/P1 locally and on GitHub; hosted CI green; 0 unresolved review threads) |
| production-qualified | **no** — see "Still pending" below and the P1.5/Phase-2/Phase-3 blockers in [tech-debt](../tech-debt.md) |

Branch `claude/p4-branches-cognitive-workflows`
from `main` at `848ec28bfd48ffc7f0a1257058b21dc68a204ebc` (the PR #8 merge; Phase 3 complete,
see [Plan 0007](0007-phase3-accepted-state-projection.md)). No gate is reported as
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
| Hosted CI (PR #9) on `ab4b38a` | pass — ci-fast 36392899625, ci-security 36392899317 (supply chain, dependency review, container), ci-integration 36392899217, ci-fuzz 36392899384 (address + none) |

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

## Closure (2026-10-05, final code `46d3eb7`)
- **Exact-head Codex** (`codex exec -s read-only`, P0/P1-only brief covering creation
  concurrency, reachability, tombstone/restore, lifecycle serialization, policy, distinct
  parties, idempotency, event integrity, tenancy, `main` protection, 0012, verifier,
  projection, request identity): `e3fb2d7` — **no P0/P1**; after the closure changes,
  `46d3eb7` — **no P0/P1**.
- **Deterministic create-vs-source-advance races** (`pg_branches`, forced with row locks
  and the request's own idempotency advisory lock, no sleeps; 20/20 repeated runs on
  PostgreSQL 15 and 17):
  `create_racing_a_source_acceptance_branches_from_an_authoritative_head` — case 1 (create
  holds the source share lock first): branch at C1 v1, main C2; case 2 (acceptance holds
  main first): branch at C2; case 2b (create paused between its lock-free phase and its
  transaction while main commits C2): branch at C2; case 3 (explicit C1 while main moves
  C2 → C3, both orders): branch at C1; case 4 (point reachable only after the check):
  `BRANCH_POINT_UNREACHABLE`, retry succeeds — a safe false negative, documented in ADR-0022
  and tech-debt for Phase 5. Also `create_racing_the_deletion_of_its_source_has_one_valid_outcome`
  (both orders) and `a_checked_branch_point_survives_only_audited_source_movement`.
- **Focused storage/concurrency review** (Opus, read-only): no P0/P1; no create/accept,
  create/delete, create/restore or same-name deadlock; no never-committed source state;
  deleted sources refused. P2 D1 fixed in `46d3eb7`: a point checked against an earlier
  head was trusted across a raw import move during an owner's `importing` flip; it now
  carries over only across contiguous audited ref events, and the lock-free read is
  limited to an active graph of the caller's tenant (mutation-checked: the rewind test fails
  without the fix). Decisions recorded in tech-debt: graph status state machine; Phase 5
  keeping "old head = first parent".
- `Cargo.lock`: `yoke-derive` 0.8.3 was yanked upstream after the PR's CI; bumped to 0.8.4
  (the advisory gate failed on it, not on Phase-4 code).
- Gates on `46d3eb7` (2026-10-05): `check-fast` pass; `check-supply-chain` pass; PostgreSQL
  17 and 15 suites pass (`pg_branches` 16, `pg_workflow` 14, `pg_verify` 3,
  `pg_least_privilege` 19, `pg_api` 13, `pg_validation_api` 21, all other store suites);
  `test-integration.sh` `INTEGRATION OK` (Fuseki projection regression, branch e2e, VERIFY
  OK); `stress-branches.sh 100 3 4` PASS (`target/stress-branches/20261005T210333Z`: 1 493
  requests, races 3 landed / 17 refused, 90 pairs 0 disagreements, 0 deadlocks, verify
  clean; p99 create 98.6 ms, prepare 82.4, accept 59.8); `upgrade-p4.sh` `UPGRADE-P4 OK`
  (`target/upgrade-p4/20261005T210437Z`); `backup-restore.sh` `BACKUP RESTORE OK` (branch
  lifecycles restored from dump and base backup; five drift cases refused).
- Hosted CI on the PR head: recorded in the PR (ci-fast, ci-integration, ci-security,
  ci-fuzz).

## Post-closure finding (2026-10-06, final code `599ca07`)
The plan above was closed and moved to `completed/` on 2026-10-05. GitHub Codex then
reviewed PR head `022d31d` and opened one thread: **P2 — durable branch-create replay was
preceded by the historical reachability check.** Durable idempotency is a core invariant
here, so it was treated as blocking.
- **Verified on the unchanged code**: replaying a completed historical create on an active
  graph whose source had moved ran the lock-free walk first. With `commit_index` /
  `commit_parents` locked, the replay blocked (test timed out after 10 s). The
  archived-graph `BRANCH_NOT_FOUND` described in the finding did not reproduce: the
  pre-walk error was held, not returned, and the transaction's replay check ran first.
  Replay still depended on DAG and traversal state, which breaks the stated rule.
- **Fix**: `create_branch` with `from_commit` looks up a completed result on a short-lived
  connection (no lock, no transaction) before walking, and replays it or returns
  `IDEMPOTENCY_CONFLICT`. The definitive second check, in the scoped transaction under the
  idempotency advisory lock, is retained as the serialization point. ADR-0022 now states:
  a completed branch lifecycle request is replayed before any mutable graph/source/
  reachability check; preflight reachability applies only to requests with no durable
  result. No request identity, golden vector or migration changed.
- **Regression tests** (`pg_branches`, PostgreSQL 15 and 17, 18/18):
  - `a_completed_historical_create_replays_without_any_source_or_dag_check`: replay with
    `max_visited = 0`, an expired deadline and the DAG tables locked `ACCESS EXCLUSIVE`,
    first on the active graph after the source moved, then after the graph is archived.
    Result: original event, `replayed = true`. A changed request under the same key after
    archive returns `IDEMPOTENCY_CONFLICT`.
  - `concurrent_identical_historical_creates_create_once_and_replay_once`: both requests
    past their walk and queued on the idempotency lock; one creates, one replays, same
    event, one durable result.
- **Security hardening**: the `.trivyignore` exception for CVE-2026-84782 (`libssl3` in
  the distroless base) now has a mechanical premise check. `scripts/check-runtime-linkage.sh`
  runs in `ci-security`'s container job: it prints `NEEDED` for ledger-server, ledger-admin
  and ledger-projector (libgcc_s, libm, libc, ld-linux) and fails on libssl/libcrypto.
  Verified to fail against an image whose binary links libcrypto.
- **Gates on `599ca07`**:
  - `check-fast` pass; `check-supply-chain` pass.
  - PostgreSQL 17 and 15 suites pass: `pg_branches` 18, `pg_workflow` 14, `pg_verify` 3,
    `pg_least_privilege` 19, `pg_api` 13, `pg_validation_api` 21.
  - `INTEGRATION OK`, including the Fuseki regression.
  - `UPGRADE-P4 OK` (`target/upgrade-p4/20261005T222102Z`); `BACKUP RESTORE OK`.
  - 100-branch stress PASS, run twice:
    - `20261005T222013Z`, under host load (load average 3.8): every operation sat on a
      uniform ~500 ms plateau, create p99 581 ms;
    - `20261005T222253Z`, on an idle host: create p50 / p95 / p99 = 58.9 / 180.1 / 198.7
      ms, accept 32.9 / 78.4 / 97.1, prepare 39.0 / 88.5 / 105.2.
    - Both runs: 0 deadlocks, 0 pair disagreements, verify clean, unconfigured backlog
      equal to `main` only.
- **Exact-head Codex on `599ca07`**: local `codex exec -s read-only` reported **no
  P0/P1**; GitHub Codex said "Didn't find any major issues". The original thread was
  resolved after the regression test passed.
- **Hosted CI on `599ca07`**: ci-fast, ci-integration, ci-security (with the new linkage
  step) and ci-fuzz all pass.

### Still pending (accepted, not merge blockers)
Live Fluree branch differential (BUSL-1.1 approval pending; not run, not counted);
pagination cursors; deep-history traversal optimization; branch outbox accumulation;
projection-metrics partial index; confusable/odd-but-legal branch names; audited raw-import
activation; trusted-writer database residual; graph status state machine.

## Sub-agent decomposition (§42)
Main session owns the ADR, migration, verifier, repository, request identity and API.
`ledger-dag` implemented by a bounded agent (crate only). Read-only reviewers at the end:
lifecycle/invariants, DAG, storage/concurrency, security/authorization, API/idempotency,
tests, projection interaction; then Codex on the final candidate.
