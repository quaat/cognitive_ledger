# Technical debt and deferred work

## P1.5 production-qualification blockers (decisions or evidence an operator/deployment must supply before `production-qualified: YES`)

- The OIDC authenticator (`OidcAuthenticator`: JWKS fetch/rotation, issuer, audience, exp/nbf, unknown `kid` fails closed) is exercised against a real local JWKS endpoint from two replicas over PostgreSQL (`pg_api`), but has not been exercised against a live Entra ID tenant (no tenant credentials are available to the qualification runs); the live-issuer smoke test stays **pending** and must run before any non-development deployment. `Capability::Admin` is mapped from roles but no public route requires it (graph administration is `ledger-admin`, not HTTP).
- Admission control (Plan 0005 reviews): `accept` takes no expensive-operation slot (only `prepare` and state reads do), so an accept-heavy burst can occupy the whole 16-connection pool beside the 12 expensive slots; and when the edge timeout fires the handler future is dropped and its slot released while the PostgreSQL statement keeps running until `lock_timeout`/`statement_timeout`, so repeated edge timeouts can pin pool connections beyond `max_concurrent_expensive`. Both are bounded by the session limits (ADR-0016) and observed as `503 DEPENDENCY_*`, not as corruption; decide on a shared admission budget for accept and on cancelling the statement (`pg_cancel_backend` or a cancellation-aware driver call) when the request is abandoned.
- Restore semantics (Plan 0005 slice 8): a restore forks history — versions after the snapshot are reissued for different commits, acknowledged writes vanish, projections may be ahead. These questions (PITR/WAL archiving, writer fencing, publishing the restore point and rebuilding projections, forbidding new versions until reconciled) were open before ADR-0017. ADR-0017 decides: PITR/WAL archiving required in production, writer fencing, declared restore point, projection reconciliation, version reissue semantics; the deployment must provide the PITR configuration evidence.
- Migration 0009 aborts on a corrupt `immutable_objects` row with a raw `23514` naming no ids (the README convention is guards that name rows); the runbook says to run `verify` first. Add a pre-check guard that lists offending ids, and document the `ACCESS EXCLUSIVE` hashing window. Also: a graph moved from `importing` to `active` after raw ref moves has no `ref_events` for them and fails the verifier's version-equals-events check permanently — activation needs an audited path (Phase 4 admin flow).
- Fault injection (Plan 0005 slice 4): the lost-response-after-COMMIT case is proven deterministically in-process since Plan 0013 M1 (a `test-hooks` pause after `COMMIT` on every write path, `pg_lifecycle`, and over a live router with the edge timeout, `pg_api` `p7a_*`); at the HTTP level under `scripts/fault.sh` random SIGKILLs still hit the sub-millisecond COMMIT-to-response window only by chance (0–3 observations per run). A `fault-injection` cargo feature that aborts the *process* right after the workflow transaction commits — compiled only into a separate qualification image, never into the runtime image — would make the process-kill variant deterministic too.

- Repository governance (observed 2026-10-06): the GitHub `main` branch has no branch protection or ruleset. Before a production release, enable a ruleset requiring a pull request, the required CI checks (benchmark-ci, container, dependency-review, docker, fast, fuzz, supply-chain), a review, and no direct pushes to `main`. `benchmark-ci` (workflow `ci-benchmark`) is the Phase-6A correctness gate (the dataset manifests, the oracle assertions and `ledger-admin verify`); its timings never gate. `scripts/check-doc-consistency.py` keeps this list equal to the jobs of the PR-gating workflows (rulesets match check-run names, i.e. job names, not workflow file names). A matrix job reports one check per combination, for example `fuzz-sanitizer (none)` and `fuzz-sanitizer (address)`, so the stable `fuzz` aggregate job (`if: always()`, succeeds only if every sanitizer job succeeded) is the one to require. Not changed automatically (repository policy is the owner's).

## Phase 2 (Plan 0006) external prerequisites and residuals

- **Sculpin validation endpoint — EXTERNAL PREREQUISITE.** The ledger-side contract, client and
  every ADR-0014 scenario are implemented and tested against a deterministic fake validator;
  no live Sculpin service exists yet. Sculpin must provide the endpoint of
  `docs/design/sculpin-validation-service.md`, an aggregate stable `base_kb.revision`,
  content-identifying ontology/shape versions, Virtual A-Box identification, and a way to
  publish its current semantic environment. A live end-to-end test is external evidence,
  never part of the workspace gate.
- Environment freshness for external data rests on Sculpin's `sources_revision` changing
  whenever the source versions it would hydrate change; the ledger cannot verify that
  discipline. A Sculpin "current environment" endpoint (publishing the environment id) would
  make orchestration uniform.
- The runtime remains the trusted writer of new validation records (ADR-0016 residual): a
  compromised runtime could fabricate a conforming record for its own tenants. The report
  digest/reference allows cross-checking against Sculpin's report store; signed validator
  responses (a validator key verified by the ledger) would close it and need an ADR.
- The 0009 → 0010 upgrade from the P1.5 release with populated data is qualified by
  `scripts/upgrade-p2.sh` (Plan 0006 evidence); re-run it on the final release candidate.
- Sculpin must honour the validation invocation identity (`invocation_id` /
  `Idempotency-Key`, ADR-0019 amendment): repeated or concurrent calls with one id resolve to
  one logical validation. The ledger sends it on every delivery and its tests prove its own
  side against a fake that implements the contract; the ledger cannot verify Sculpin's
  deduplication. Until Sculpin implements it, concurrent same-key requests (or a retry after
  a crash between answer and record) may record a result from whichever environment was
  current when the winning delivery ran.
- Detail rows under sealed parents (security review, P2, accepted for this release): the
  write-once triggers on `decision_validations`, `validation_violations` and
  `semantic_virtual_contexts` block UPDATE/DELETE but the runtime keeps INSERT, so a
  compromised runtime could add a citation to an already-decided decision, a summary row to a
  recorded validation, or a virtual-context row to a context. This is the same class as the
  accepted ADR-0016 residual (runtime is the trusted writer of new rows); acceptance never
  reads these rows (it decides on the hashed bytes) and `ledger-admin verify` detects every
  such row (count/array agreement plus element-by-element comparison with the decoded
  bytes). Closing it needs insert-time guards bound to the parent's transaction (a
  `decision_validations` INSERT trigger requiring `validation_id = ANY(decisions.validation_ids)`,
  deferred count triggers for the detail tables) plus verifier coverage and an ADR — or the
  `SECURITY DEFINER` write-function model below.
- Database-level size bounds (security review, P3): `semantic_virtual_contexts.object_refs`
  elements and the `canonical_bytes` columns have no `octet_length` CHECK (the protocol
  bounds them; only a compromised runtime could exceed them), and `ledger-admin verify` loads
  all records and contexts into memory. Add CHECKs in a later migration and stream in verify.
- The trusted validator service id is an operator assertion (no response signature or key
  binds it to the endpoint), and the invocation id is an unkeyed hash (guess-confirmable,
  equal across deployments sharing a validator). Signed validator responses and a keyed
  invocation id each need an ADR (and, for the id, new vectors) — security review, P2/P3.
- `AppState::with_validation*` panic on a mismatching or malformed configuration
  (construction time only; `main` validates first); return `Result` if embedders appear.
- Offline-verifier hardening against owner-level FK removal (Codex on `e264950`, P2,
  accepted for this release): `ledger-admin verify` does not yet flag orphaned
  `decision_validations` / `validation_violations` / `semantic_virtual_contexts` rows or a
  validate idempotency row whose `result_commit`/graph disagree with its record *after the
  corresponding FK was dropped* (records with a missing context are flagged since
  `e264950`). The start-up FK-shape match ignores `ON DELETE`/`ON UPDATE` actions (all
  0001–0010 FKs are `NO ACTION`); a cascade cannot fire because DELETE is refused by the
  structurally verified write-once triggers and by runtime privileges. Add parent-existence
  and binding checks per detail table and include referential actions in the FK shape.
- The released P1.5 server refuses a database restored from a logical dump (strict CHECK
  deparse vs PostgreSQL's re-parse flattening; fixed in Phase 2, ADR-0017 amendment). A P1.5
  rollback must use a physical base backup.
- A P1.5 `ledger-admin verify` does not check the schema level and prints `VERIFY OK` against
  a 0010 database; always run the verifier from the same build as the servers.
- Validation calls are synchronous inside the request (bounded by the validator timeout and a
  dedicated budget). A queued/async validation flow is a later orchestration layer above
  these primitives, not a replacement for them.

## Phase 3 (Plan 0007) residuals and accepted risk

- Full-state projection: every projection sends the whole accepted state in one SPARQL Update (bounded by `LEDGER_PROJECTOR_MAX_STATE_BYTES` / `MAX_UPDATE_BYTES`; beyond them the stream blocks with `STATE_TOO_LARGE`). Incremental patch application behind the same marker needs benchmarks first (ADR-0020 alternatives).
- Out-of-band replacement of target triples that preserves the graph's triple count is not detected by the normal path or by reconciliation (both compare marker count with the target's count); only `ledger-projector verify` (full content comparison by term equality) finds it. The projector's credential must be the target's only writer (deployment runbook).
- Metrics scrape runs the status query over every stream of the target (one aggregate over `projection_state` × `refs` × pending outbox). Fine for hundreds of streams; cache or paginate before thousands.
- Migration 0011 adds `ref_events_version_head UNIQUE (graph_id, branch, new_version, new_head)`, which overlaps `ref_events_version_unique` (an extra index maintained on every acceptance). `ALTER TABLE … ADD CONSTRAINT … UNIQUE` takes an **ACCESS EXCLUSIVE** lock on `ref_events` for the migration transaction, blocking reads (history, status) and acceptance for the index build; it cannot fail on existing data (the narrower key is already unique). Needed as the FK target of recorded progress; revisit if a composite FK onto the existing key becomes possible.
- `ledger_grant_projector` refuses superusers, CREATE holders and non-owners but, like `ledger_grant_runtime`, does not refuse a role that is also the runtime role; `ledger-admin migrate --projector-role` refuses equal names, and start-up identity verification refuses the resulting mixed identity. Keep operators on `ledger-admin`.
- Development compose runs the projector with the Fuseki `admin` account and a checked-in development password (`deploy/fuseki/development-admin-password`); production must use a dedicated update-only account over https (runbook). The anonymous `query` endpoint is intentional for development reads.
- TDB2 commit latency on the qualification host is ~0.5–1 s per update (single writer); the Fuseki suite therefore runs one test at a time. Throughput per target is bounded by that latency (one projection write per stream step); measure on production storage before sizing.
- Projection of ledger named graphs, of refs other than `main`, and switching a KB's feed graph without an operator rebuild are v1 non-goals (ADR-0020).
- The dataset binding is keyed by the operator-chosen `target_id` only and checked at projector start-up only: two deployments configured with the same id (a restored staging copy) both pass and would rebuild each other's projections (`MARKER_COMMIT_MISMATCH`), and a dataset wiped while a projector runs could be bound by a second deployment. Runbook: a copy always gets its own target id and dataset. Consider binding to an installation id recorded in the ledger plus re-checking the binding with the periodic probe.
- Target TLS uses the built-in web roots (`reqwest` `rustls-tls`): no private-CA bundle setting yet, which pushes internal deployments towards a TLS sidecar. `LEDGER_PROJECTOR_DEVELOPMENT` couples plain loopback http with optional credentials (now logged as a warning at start-up); split it if a deployment needs only one. The projector's database URL (with password) comes from an environment variable and `ledger-admin … --database-url` from arguments; add `_FILE` variants.
- Adapter resource use: one response cap (`LEDGER_PROJECTOR_MAX_RESPONSE_BYTES`, 64 MiB) covers the tiny observe/ASK/bind answers too; the write path copies the state several times while building the request (≈ 5 × state per in-flight write); the containment `ASK` sends the whole state as one query. Tighten caps per query kind, build the request in one buffer, and chunk containment before raising the state limit.
- `/metrics` and `/ready` run database queries on every request without authentication; they bind loopback by default, but compose binds `0.0.0.0` inside Fuseki's network namespace (development). Cache or rate-limit before exposing them.
- `projection_outbox.delivered_at`/`attempts` are shared by all targets: with two targets, the second target's backlog is under-reported by `unconfigured_pending` and the outbox columns (per-target progress in `projection_state` is authoritative and correct).
- A retry that finds its own earlier write already in the target (`AlreadyProjected` after a lost response) acknowledges without re-running the rebuild-only containment `ASK`; the count comparison still runs. The 75 % lease budget is measured from the start of the step, not from the lease grant (a claim that took long leaves less margin than computed).
- Backup/restore is qualified for the ledger (`scripts/backup-restore.sh`) but not while a projector is actively projecting; a restored ledger's projections are recovered by `MARKER_AHEAD` → operator rebuild (Fuseki suite, runbook). Add a backup-under-projection scenario to the restore qualification.
- No fuzz target covers the SPARQL JSON result parsers (`ledger-projection-fuseki::sparql::parse_*`, input from the configured target only; unit-tested). Add one before the target is treated as untrusted.
- A future protocol (v2) marker would most likely live under another graph or namespace, so a v1 projector would see "no marker" and rebuild over it (`UNMARKED_CONTENT` / `TARGET_LOST`); only v1-shaped markers ahead of the head are held for an operator. Record the protocol version in the dataset binding and refuse (`TARGET_CONFLICT`) a dataset bound by a newer protocol before shipping v2.
- Re-pointing a KB's cognitive graph to another tenant's ledger graph is allowed for the owner (disable + enable + rebuild) with no cross-tenant confirmation; decide whether an explicit flag is wanted once multi-tenant deployments share targets.
- Test isolation: the Rust Fuseki suite shares the compose dataset (bound to target id `fuseki` by the compose projector) under per-test target ids and never calls `bind_target` (the second-id refusal is exercised by the integration script); it serializes itself with a process-wide lock. `dead_target()` binds and releases a port (small reuse race). Test roles `it_projector_<pid>` and Fuseki test graphs are not cleaned up (development targets only).
- The integration script's Fuseki restart proves TDB2 durability and a projector restart onto the restarted target; a running projector riding out a target outage is covered by `fuseki_projection::a_target_outage_never_blocks_acceptance_and_the_projector_catches_up`, not by the compose scenario.

## Phase 4 (Plan 0008) residuals and accepted risk

- Branch policy is immutable after creation (policy v1); changing it needs a new branch. Mutable, audited policy changes (and per-branch ACLs) are deferred to a later ADR.
- The runtime can fabricate a consistent lifecycle event (delete/restore) within its tenants, like any other audit row (ADR-0016 trusted-writer class); `SECURITY DEFINER` write functions would close this with the rest.
- Branch outbox rows are written and never delivered (projection v1 is `main` only); they accumulate with branch traffic until a later protocol projects branches or GC exists. `unconfigured_pending` ignores them by design.
- Branch-point reachability is a bounded DFS per request (100 000 commits / 5 s) with two point queries per visited commit; the 5 s deadline, not the visit limit, is the effective bound on deep histories, which are then refused `413 RESOURCE_LIMIT` rather than found (timing-dependent). A batched frontier fetch or generation numbers would make it deterministic; not needed at current depths. The first-parent log has the same per-commit cost (≤ 1 000).
- Read pagination: the branch list, movement history and first-parent log return at most 1 000 entries without a cursor (older entries are unreachable through the API); branch history returns the latest `limit` lifecycle events and movements. Add cursors before branch counts or depths exceed that.
- `projection_unconfigured_pending` scans the outbox (`delivered_at IS NULL AND branch = 'main'`, no covering index) on every metrics scrape, and branch rows are never delivered. Add a partial index `ON projection_outbox (graph_id) WHERE delivered_at IS NULL` in a later migration before branch traffic is large.
- Branch names keep the ref grammar: `..`, empty segments, a trailing `/` and case variants of `main` (`Main`) are legal names. Harmless as long as names are never paths or case-folded; decide before names feed file or Fuseki mappings.
- The deadlock-free lock order and every lifecycle race, including create racing an acceptance on its source (both orders, explicit historical points, the false-negative case), are proven by forced interleavings on PostgreSQL 15 and 17 (`pg_branches`); the 100-branch stress adds randomized races across two replicas.
- Branch-point reachability is checked against the source head read before the creating transaction (no lock held while walking). A commit that becomes reachable only through a later source movement is refused `BRANCH_POINT_UNREACHABLE` (a safe, retryable false negative; nothing unchecked is accepted). Phase 5 merge commits make second-parent history reachable this way; the same rule applies (re-walking under the lock is deliberately not done). A pre-checked point carries over only across audited fast-forwards of the source (contiguous ref events); a raw import move refuses it.
- No database-enforced graph status state machine: the owner can move an `active` graph back to `importing`/`bootstrap`, re-enabling unaudited raw head moves on refs that already have branches (migration 0004 allows any known status). Branch creation is safe against it (above); decide in a later ADR/migration whether to forbid leaving `active`/`archived` or to document quiescing as the owner procedure.
- Phase 5 must keep the 0009 rule "the old head is the new head's first parent" for every ref movement (a divergent merge commit has the target head at `parents[0]`) or relax it by ADR; a multi-commit fast-forward merge is a direct move to a descendant that 0009 currently refuses unless it is one commit.
- Live Fluree branch differential: deferred with the rest of the Fluree comparison (BUSL-1.1 sign-off pending); not run, not counted.
- Merge-base, merge, conflicts, checkpoints and GC of deleted branches are Phase 5+ (product plan).
- Activating a raw-imported graph (`bootstrap`/`importing` → `active`) adopts its refs as branches, but the imported heads still have no ref events (Phase-1 import semantics), so `ledger-admin verify` reports the ref-version and lifecycle-position checks for that graph. The audited import/activation command (P1.5 blocker above) must write the import's ref events before activation.

## Phase 5 (Plan 0009) residuals and accepted risk

- Merge ancestry walks compute full ancestor sets of both heads in memory, bounded by the
  visit limit (100 000) and a 10 s deadline. Very long-lived branches eventually hit the
  limit (`RESOURCE_LIMIT`, never a wrong answer). Generation numbers and checkpoints are
  Phase 6.
- Each preview reconstructs three full states (base, target, source), bounded by the
  reconstruction limits and the expensive-operation slot. Incremental diff (`change_index`)
  is deferred until measurements require it (product plan §15).
- The structural slot key `(graph, subject, predicate)` conservatively flags multi-valued
  predicates (for example, two different `rdf:type` additions) as conflicts. A per-quad
  strategy would be a new algorithm id (ADR-0024).
- Criss-cross histories need an explicit `base`. Virtual merge-base synthesis needs a
  later ADR.
- Every `propose` persists an immutable candidate, proposal and merge row, with no GC in
  v1. Re-proposing after staleness adds rows; superseded merge proposals are retired by
  `reject`.
- Fast-forward-class merges create an integration commit rather than moving the target to
  the source commit (ADR-0023). Ref equality between target and source after a merge is not
  provided; option B (a database-verified descendant jump) would need its own ADR and
  migration.
- The Virtual A-Box-dependent merge validation is qualified against the protocol-conformant
  fake validator. The live Sculpin service remains an external prerequisite (Phase 8).
- Merge computation runs inline on an async worker and holds the base, target, source and
  merged states plus the summary diffs at once (roughly 6–8 times one state's budget at
  the limits). It is bounded by the 3-slot admission weight and the reconstruction limits.
  Computing keys only for the changed quads, starting from T, and running under
  `spawn_blocking` are measured-later optimizations. The admission semaphore is global,
  not per tenant.
- Resolved (Plan 0009 closure): the conflict report now also has a byte budget
  (`LEDGER_LIMIT_MERGE_CONFLICT_REPORT_BYTES`, default 2 MiB) and a report-level
  `conflicts_truncated` flag (ADR-0024 "Conflict report byte budget"). The budget bounds the
  report only; the merge itself still holds the full states (item above).
- `verify` recomputes merges under `ReconstructionLimits::DEVELOPMENT` and
  `TraversalLimits::DEFAULT`, not the deployment's configured limits. A deployment that
  raises them sees valid large merges reported as violations. This fails closed (a false
  alarm, never a missed fault); pass the configured limits to `ledger-admin verify` when
  limits are raised (review, 2026-10-06).
- A merge walks each side's whole ancestry (`analyze_with_ancestries`, 100 000-visit cap),
  so very long histories cannot merge even when the base is recent (`RESOURCE_LIMIT`, fails
  closed). Generation numbers and checkpoints are Phase-6 measurement items.
- Merge preview builds the integration patch and the source-only set even for API
  previews, which discard them. This is a small allocation saving for Phase 6.
- `scripts/upgrade-p5.sh` merges one fast-forward-class branch on upgraded data (now with
  exact row-count assertions). Divergent and explicit-base merges on upgraded data rely on
  the DDL identity of clean and upgraded 0013 and on the PostgreSQL suites.
- Forced-interleaving pauses exist only under the non-default `ledger-store` feature
  `test-hooks` (two points today, both in propose: after the first replay lookup, and just
  before `COMMIT`; enforced absent from every app build by `check-architecture.py`). Further races that are now forced by holding database locks could move to
  such pauses if those tests become slow or brittle.
- The preview token binds the chosen strategy even when no slot conflicts, so previewing
  with `abort` and proposing with `union` is `MERGE_STALE`. This is intended and documented;
  normalizing it would be a token v2.
- Live Fluree merge differential: deferred (BUSL-1.1 approval pending). It has not been run
  and is not counted; the ledger-dag and ledger-merge property suites against independent
  reference models replace it internally.

## Phase 6C (Plan 0012) residuals and accepted risk
- `Ledger::state_at_bounded` (the reconstruction over the `ImmutableStore` trait, used by
  the filesystem backend and the fs→pg migration) stays scalar: one `get_commit` and one
  `get_content` per ancestor. It is not a PostgreSQL hot path; through
  `PostgresImmutableStore` it would still cost two statements per ancestor.
- The index/bytes rule of windowed reconstruction: a `commit_parents` position-0 row that
  contradicts the decoded commit is `CorruptObject`, where the scalar walk silently followed
  the bytes (ADR-0025, Plan 0012 Decision 1); a silent index (no row) is followed from the
  bytes as before. Such a row is detected by `PostgresImmutableStore::verify_commit_index` (the
  ADR-0012 re-derivation from bytes), which `ledger-admin verify` does **not** run: its SQL
  checks catch a parent-row count disagreeing with `parent_count`, a foreign parent and an
  unindexed parent, and its merge-row checks reconstruct through the windowed path (so a
  contradicted row behind a merge candidate surfaces there); no check re-derives every row.
  Add the re-derivation (or a bounded sample of it) to `ledger-admin verify` so operators
  can diagnose the new failure mode before an upgrade.
- Closure-review residuals (Plan 0012, 2026-10-07; all P3, none a defect): (a) the
  ancestry recursion's pair cap (`REACH_PAIRS_PER_COMMIT`) is tested only through statement
  counts and the hand-copied `EXPLAIN` diagnostic, not by asserting the recursion's actual
  row count on the production statement; the bound also assumes the planner keeps the
  index-probe plan for the recursive step (measured at 30,000 objects). (b) Which commits
  of a merge-heavy DAG fill a window when the pair cap cuts a recursion level is
  plan-dependent (`capped` has no `ORDER BY`); answers never change (a window is a
  prefetch), statement counts on such DAGs are bounded, not exact. (c) `ledger-dag`'s
  deadline checks after a window call and before serving a prefetched commit are exercised
  only with window 1; tight `max_visited` is compared windowed-vs-unwindowed only for
  `ancestors`. (d) A window's object bytes are held about twice over while `sqlx` rows are
  copied into owned buffers (≈ 2 × (8 MiB + one object); the scalar reads copied the same
  way). (e) The `ledger-admin` pools set no `statement_timeout`, so the window statements
  `ledger-admin verify` runs are bounded by their SQL limits only (pre-existing).
- The retrieval windows (256 objects / 8 MiB / 256 commits, recursion cap 4 rows per
  commit, ramp 1/4/16/64) are fixed public constants (`RetrievalWindows::DEFAULT`) with a
  `test-hooks` setter, not operator configuration. Revisit only with a measured reason
  (Plan 0012 M4 records the per-window cost); a configuration surface would need the
  limits-pairing discussion of the production-qualification matrix.
- Ancestry windows on merge-heavy DAGs hold fewer distinct commits than on linear history
  (the recursion stops at the pair cap), so the statement count lies between the linear
  formula and one per commit; each statement stays bounded. A walk can overshoot its
  `TraversalLimits::deadline` by one such statement. Validation, projection and
  `ledger-admin verify` reconstruct through `state_at_on` with the default windows (not
  the `test-hooks` setter), so their window-boundary coverage comes from the shared
  implementation, not from their own suites.
- The statement-count tests (`pg_retrieval`) count sqlx's `sqlx::query` tracing events on
  the test thread. They pin `2 × ceil(n / window)` for reconstructions and `window_calls`
  along the ramp for histories and previews exactly and will need
  adjusting if sqlx changes its per-statement logging, or if a path gains a constant
  statement (the tests subtract measured constants where they exist).
- `pg_least_privilege` fails when its tests run with cargo's default parallelism (one
  thread per core, 16 here) against one database: PostgreSQL answers "sorry, too many
  clients already" and pools time out, because the suite's tests each open several pools.
  All 19 pass with `--test-threads=1`; a full `scripts/test-integration.sh` run with
  `RUST_TEST_THREADS=4` has not completed locally yet (host port 8080 was busy). Observed 2026-10-07 before and after the Plan 0012 change; not
  caused by it; hosted runners have fewer cores. Bound the suite's parallelism in the
  script, or its pools, rather than relying on the runner. Related (Plan 0012 closure): with
  Docker's default 64 MiB `/dev/shm`, `pg_validation` and `pg_merge` fail under 16-way test
  parallelism with `could not resize shared memory segment … No space left on device`
  (parallel-query dynamic shared memory); every suite passes on a container started with
  `--shm-size=1g`. `compose.yaml` sets no `shm_size`; hosted runners pass because they run
  fewer tests at once. Set `shm_size` on the compose PostgreSQL (or document the host
  requirement) before relying on local full-parallel runs.

## Phase 7A (Plan 0013) M0 findings awaiting their milestone
Recorded 2026-10-07 from the read-only inventory in
[Plan 0013](active/0013-phase7a-resource-governance.md) (findings F1–F10 there carry the
file:line evidence and the milestone that fixes each):
- ~~`FailPoint` is compiled into release builds~~ **Fixed in M1**: `FailPoint`, every fail
  point, the pause hooks and the projector's crash windows exist only under the `test-hooks`
  feature; `scripts/check-architecture.py` proves the apps' build graphs never enable it and
  that the symbols are absent without it (compile probe).
- ~~The fault gate accepts `DEPENDENCY_TIMEOUT`~~ **Fixed in M1**: `fault_unexpected` fails the
  fault run on it (mode-specific; the pair-verdict helper is unchanged); the merge crash tests
  assert the injected error; the post-COMMIT lost-response proof is deterministic in
  `pg_lifecycle` / `pg_api` (see the Plan 0005 bullet above).
- Abandoned statements run to completion after the edge timeout (sqlx 0.8.6 pins the
  connection until PostgreSQL finishes; `ROLLBACK` is queued behind it), `statement_timeout`
  equals `request_timeout`, `idle_in_transaction_session_timeout` exceeds it, and no
  transaction-level bound exists — measured in M1 ([evidence](../quality/evidence/plan-0013-m1-lifecycle-2026-10-07.md)).
  M2 per ADR-0026 (proposed): timeout-only cancellation; **active cancellation stays open**
  because `pg_cancel_backend(pid)` can hit the next borrower of the pooled session
  (demonstrated); it needs connection fencing first, and the backend-reuse race test is its gate.
- `accept`, `reject`, `merge_apply` and branch writes take no admission permit; 12 + 4 slots
  equal the 16-connection pool; prepare holds a slot while waiting for a connection — measured
  in M1 (reads and `/ready` starve 10 s behind three blocked accepts). M3 per ADR-0026 §5
  (the `db_work` permit before the first pooled query, held by the detached operation).
- M1 review residuals (P2/P3): the validation record transaction has no `BeforeCommit`/
  `AfterCommit` hook and no drop test (same `begin_scoped`/`record_result` shape as the eight
  hooked paths; needs the validation fixtures) — before M2 acceptance; `mark_superseded` has
  none either (F8 scope); an identical in-flight `immutable_objects` insert from another tenant
  waits on the unique index and could surface as `DEPENDENCY_TIMEOUT` once `lock_timeout` is
  5 s — M2 classifies that wait.
- `mark_superseded` has no idempotency key (a retry after a lost response gets
  `LineageMismatch`); the projector's `number()` accepts 0, its DB session limits are not
  configurable and its worker count is not checked against its 8-connection pool;
  `ledger-admin` pools set no session limits. Deferred or M3 as the plan states.

## Later-phase work and accepted residual risk (does not block Phase 2 or the P1.5 gate)

- Design a stable skolemization/import protocol and hostile-input limits around the standards N-Quads parser.
- The Phase 6 streaming export / history-lookup API now has a named consumer — replaying history for
  predictive models (`docs/design/neural-prediction-assessment.md`, roadmap Phase 9 candidate) — but
  stays gated by Phase 6's measured-need rule; a read-only export identity would extend ADR-0016 and
  needs an ADR.
- Run the live Fluree differential adapter; the reference image is already digest-pinned (see test/reference-images.lock), so only running the semantic-state adapter remains, blocked pending BUSL-1.1 license sign-off.
- Graph import operator path (ADR-0010): register `status='importing'`, import, activate. Until it exists, migration 0004 fails closed on unowned graphs and `ledger-admin migrate-fs-to-pg` can only target `bootstrap`/`importing` graphs.
- `WorkflowRepository::state_at_on` (transaction-connection, bounded reconstruction) and `Ledger::state_at_bounded` are two implementations of the same fold over `ReconstructionLimits`; unify when `Ledger` composes over `PostgresLedgerStore`.
- `mark_superseded` is an explicit operator action without an idempotency key; a retry after a lost response reports `LINEAGE_MISMATCH` (already decided) rather than replaying. Give it a scope/key if it becomes an API operation.
- `ledger-admin migrate-fs-to-pg` loads the whole source store into memory (bootstrap scale only) and cannot catch up with a destination ref that has moved past the source HEAD (it is a cutover tool; live writes must stop first).
- `WorkflowRepository` trusts `RequestScope.request_digest`; since P1.4 the only producer is `ledger-api::request_identity` (server-computed from the parsed request, golden-pinned). If a second producer appears, move the canonical encoding into `ledger-store` so the repository can recompute it.
- Correlation ids generated by the server are a hash of pid/counter/time (unique, not secret); if they are ever used as capability-bearing tokens they must become cryptographically random. Client-supplied correlation ids are stored verbatim (bounded, printable); a client can reuse another request's id, so investigations must key on the server's own ids (`proposal_id`, `decision_id`, `event_id`) first.
- Without checkpoints, `ReconstructionLimits::max_depth` is a hard ceiling on branch length (prepare refuses beyond it, by design). Snapshot/materialised base state needs an ADR only if §24's condition is met — a declared depth/latency target the measured residual exceeds (Plan 0012 M4: `CHECKPOINT-ADR-READY: NO`; conditional design in `docs/quality/performance-baselines.md`).
- `WorkflowRepository::accept` enforces `ValidationPolicy::Required`, but the policy still travels with each request; a future in-process caller could pass `NoValidation`. Consider making the policy a repository construction parameter once Phase 2 defines real policies.
- The Python request-identity reference hashes fixture quads verbatim (they are already canonical N-Quads) and does not implement N-Quads canonicalization itself; its independence covers the envelope layout, not RDF canonicalization (covered by the ledger-rdf goldens).
- `tenant_id` on audit rows now means both the actor's tenant and the graph's tenant (composite FKs); a cross-tenant platform operator acting on a graph cannot be recorded. Decide before Phase 4/5 admin flows.
- Any `read`-capable principal of a tenant can reconstruct the state of prepared, rejected or superseded candidates (they are indexed commits of the graph); intended for review, documented in security.md.
- The expensive-operation semaphore is now tested under a blocked database (`pg_api::expensive_operations_are_admission_controlled_under_a_slow_database`); a live Entra ID issuer test remains pending (above). Under 1,000 concurrent clients the admission control rejects most prepare attempts immediately (`503 RESOURCE_LIMIT`, see the stress evidence); a queue with a bounded wait instead of immediate rejection is a possible later refinement, not a defect.
- Test hygiene: `pg_api` creates the cluster-wide test role `ledger_rt_api` and never drops it; `pg_least_privilege` drops its role only on success. Development clusters only; add teardown when the suites get a shared fixture. Qualification scripts pass development DSNs and the dev HS256 secret as process arguments (visible in `ps`/`docker inspect`); acceptable only because every value is labelled development-only.
- OIDC rotation latency (Plan 0005 slice 5): with the production refresh policy (60 s minimum interval) a token under a `kid` published after the last JWKS fetch is refused with `401 UNAUTHENTICATED` until the interval elapses (pinned by `pg_api::two_replicas_share_one_key_source_and_replay_identically_across_rotation`, step 6). Acceptable because issuers publish keys ahead of use; if an issuer ever rotates keys and uses them immediately, return a retryable 503 for unknown `kid` during the throttle window instead.
- Residual write authority of the runtime identity (ADR-0016): it can fabricate a consistent forward ref move with its audit rows or pre-seed idempotency results within its tenants. Closing it needs `SECURITY DEFINER` write functions (with pinned `search_path`) as the only write path, and ideally a cargo feature gate so `ledger-server` cannot link the migrating constructors (`connect_and_migrate`, `from_pool`, `with_ref`).
- `mark_superseded` has no idempotency record; a retry after a lost response reports `LINEAGE_MISMATCH`. Give it a scope/key if it becomes an API operation. PostgreSQL 17's `transaction_timeout` would bound a workflow transaction that keeps issuing statements; consider it once PG17 is the floor.
- Identical prepares whose content, actor and microsecond `recorded_at` coincide under two different keys collide on `proposals_candidate_unique`; reported as `LINEAGE_MISMATCH` (not a 500) — acceptable, extremely unlikely.
- **Intermittent hang: cause unknown, defensive fix (Plan 0011).** Observed 2026-10-06 in
  hosted `ci-integration` run 37524260391, first attempt:
  `pg_graphs_migration::upgrade_refuses_graphs_without_a_derivable_owner` stalled for 36
  minutes; the rerun passed.
  - The cause is **unknown**. The suspected mechanism is a failed sqlx run keeping its
    advisory lock on a pooled connection. `a_failed_migration_keeps_its_advisory_lock_on_a_pooled_connection_only`
    shows that this mechanism exists. However, the stalled test already closed its pool after
    the failure, so the evidence does not establish the link.
  - Defensive changes:
    - every expected-failure migration in `pg_graphs_migration`, `pg_fs_migration` and
      `pg_least_privilege` runs on a dedicated connection that is closed afterwards;
    - `migrate_expecting_failure` is bounded to 120 s and asserts that no advisory lock
      remains;
    - every `pg_graphs_migration` test has a 300 s whole-test deadline, so a recurrence fails
      by name. The deadline is a tokio timer, so it fires only if the test awaits; a hang
      that blocks the runtime thread is bounded only by the job timeout;
    - the expected-failure migrations in `pg_fs_migration` and `pg_least_privilege` are
      bounded to 120 s each;
    - `ci-integration` has `timeout-minutes: 30`.
  - The hang did not reproduce in local repeated runs (counts in Plan 0011 Evidence). Keep
    this entry open until hosted runs have stayed clean over a longer period.
- Failed sqlx migration runs keep their advisory lock on the pooled connection; library constructors that migrate on a caller's pool inherit this. Run migrations on a dedicated connection or through the explicit `schema` entry points from a fresh process.
- Evaluate `cargo-deny`, SBOM, and container scanning with classified findings (`cargo audit` is now a blocking gate via `scripts/check-supply-chain.sh`; its single exception, RUSTSEC-2023-0071 for the lockfile-only `rsa` under `sqlx-mysql`, is re-proven on every run and must be deleted when sqlx/rsa move).
- Third-party GitHub Actions are pinned by commit SHA (Plan 0005 slice 2); bumping them is a deliberate change with the release name in the comment. The distroless runtime base is pinned by digest and must be refreshed when the classified container findings gain fixes (`docs/quality/security.md`).
- Establish benchmark baselines and checkpoint policy before performance gates.
- Dedicated Rust >=1.94 / SQLx 0.9 migration and full requalification (Dependabot's
  `sqlx 0.8.6 → 0.9.0` needs Rust 1.94 and source changes; the workspace targets Rust 1.89).
