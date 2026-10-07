# Plan 0012: Phase 6C — batched reconstruction and DAG/ancestry retrieval

Status: **in progress** (started 2026-10-07). Branch `claude/p6c-batched-retrieval` from the
Phase-6B head `d05113d` (PR #13 head `aabdd11` plus the two Codex P2 fixes of 2026-10-07),
which is `main` at `646b029` plus the reviewed Phase-6B changes. Continues
[Plan 0011](../completed/0011-phase6b-bear-reconstruction-characterization.md).

**Base reconciliation (recorded, not yet resolved).** The task required starting from the
`main` that results from merging PR #13. At the start of this plan PR #13 was still open
(CI green, mergeable, two Codex P2 comments outstanding; previous PRs were all merged by the
repository owner, never by the agent). The two comments were fixed on the PR branch
(`d05113d`), and this branch was created on top of that head so that the Phase-6B
benchmark tooling (`ledger-bench recon`, `scripts/benchmark-recon.sh`) and documents this
plan builds on are present. PR #13 touches no production crate (`git diff --name-only
646b029..d05113d -- crates apps/ledger-server apps/ledger-projector migrations` lists only
three test files), so the production code this plan changes is byte-identical on `main`
and on the Phase-6B head. **Before this plan's PR is merged, this branch must be rebased
onto (or merged with) the post-#13 `main`, and the result re-checked**; if `main` then
differs from `d05113d` in anything but the merge commit, the difference is recorded here.
Until then the PR for this plan is opened against `claude/p6b-bear-reconstruction`.

## Goal
Remove the dominant cost that Phase 6B measured, **≈ 2 PostgreSQL statements per
reconstructed ancestor and ≈ 2 per visited commit per ancestry side**, by retrieving
commits, patches and parent edges in **bounded windows** instead of one row at a time,
while leaving every ledger semantic, identity, limit, error and transaction boundary
exactly as it is. The deterministic gate is the statement count; latency is observed.

## Evidence driving the work (Plan 0011, two official runs)
- `state_at_on` issues 2 statements per ancestor (commit object, patch object): statements
  grow exactly linearly, 2.00 per level.
- API state-read latency grows by 189–225 µs per ancestry level; PostgreSQL execution is
  14–15 % of it, the prefetched fold ≥ 10–11 µs per level (a lower bound). The rest, about
  85 %, is per-statement round-trip overhead.
- A database restart changes nothing material (block reads ≈ 1,200, 13–15 ms).
- The contained merge preview (ancestry walk only) costs ≈ 330 µs and 4 statements per
  level; the divergent preview ≈ 960 µs and 10 statements per level (walk + three
  reconstructions).
- `CHECKPOINT-ADR-READY: NO` (Plan 0011) stays in force; this plan builds no checkpoint.

## Scope
1. **M0 — design and reference behaviour.** Document the scalar algorithms and their error
   precedence (below); census of per-ancestor and constant statements; the indexes used;
   `EXPLAIN (ANALYZE, BUFFERS)` of the candidate window queries on a diagnostic database;
   a test-only scalar reference kept for differential tests; the expected statement
   complexity written before implementation.
2. **M1 — bounded batch object retrieval**: one crate-internal primitive in
   `postgres_immutable.rs` that retrieves several immutable objects on the caller's
   connection with bound parameters, accounts for every requested id, verifies every
   returned object's SHA-256 against its requested id, caps the count and the returned
   bytes in SQL, and is order-deterministic.
3. **M2 — windowed first-parent reconstruction** in `WorkflowRepository::state_at_on`
   (hence `reconstruct`, `prepare`, merge preview/propose/verify, validation and
   projection reads): a bounded recursive query over `commit_parents` position 0 as a
   *hint* for the next window, every commit decoded from verified bytes, the decoded
   `parents[0]` cross-checked against the hint, patches fetched set-wise and folded
   oldest-first, cycle state and the depth limit carried across windows.
4. **M3 — windowed DAG retrieval**: `ledger-dag` gains an infrastructure-neutral
   prefetch hook on `ParentProvider` (`ancestry_window`) with a default that keeps every
   existing provider unchanged; `GraphParents` implements it with a bounded recursive,
   graph-scoped query; the DAG algorithms keep their semantics and run against the
   abstract provider.
5. **Tests**: the corruption/adversarial matrix, differential tests (scalar reference vs
   windowed, window sizes around the boundaries), DAG window-equivalence property tests,
   transaction-ownership checks.
6. **M4 — characterization**: the Phase-6B `recon` profile before and after on the same
   host and clean worktrees, statement counts as the gate, latency observed, residual
   per-level cost estimated, the checkpoint decision restated.
7. **Documentation** in the same PR: this plan, ARCHITECTURE, benchmark methodology,
   performance baselines, production-qualification matrix, deployment/limits, tech-debt,
   the Phase-6 roadmap note.

## Non-goals
No persistent checkpoints, reconstruction cache, persistent change index, new public
history API, new commit/patch protocol or canonical encoding, new merge semantics, new
storage backend, background compaction, garbage collection, state snapshots, **no new
database schema or migration**, no raised API/resource limit, no benchmark special case.
If a schema or index change ever appears necessary, that path stops and is written up for
an ADR/plan decision instead.

## Invariants (all must hold byte-for-byte or category-for-category)
Canonical commit and patch bytes; every `CommitId`/`ContentId`; RDF state and state
digests; v1 and v2 read compatibility; graph and tenant isolation; `ReconstructionLimits`
(depth, quads, bytes) and the patch-operation limits; cycle handling; missing-object
behaviour; corruption detection; the stable error taxonomy (and the wording of the
published reasons); branch semantics; merge-base, ahead/behind, merge classification,
structural conflicts, preview tokens, stale-preview protection, source-party provenance;
API representations; transaction and locking semantics (reconstruction inside `prepare`
stays on the workflow transaction's own connection; no helper acquires a second pool
connection while the caller owns one).

**Immutable object bytes remain authoritative.** `commit_index` and `commit_parents` are
hints for *which rows to fetch next*; nothing decoded from them enters a reconstruction or
an ancestry answer unverified:
- commit: stored bytes → SHA-256 = requested id → production decoder → decoded id =
  requested id (`decode_commit_object`) → decoded `parents[0]` = the hinted next id;
- patch: stored bytes → SHA-256 = the patch id taken from the *decoded commit* →
  `validate_patch_bytes`;
- parents for DAG walks: `commit_parents` rows → contiguity against `parent_count`
  (exactly as `GraphParents::parents` does today).

A disagreement between a hint and the bytes is `CorruptObject`. A row missing from an
outer join is a typed missing-object error for that id, never a shorter history.

## Assumptions
- Every commit that a reachable commit names as `parents[0]` is itself indexed
  (`publish_commit_in` refuses unindexed parents, migration 0006 lets refs target only
  indexed commits, the fs→pg migration indexes everything). A first-parent chain that the
  index cannot continue while the bytes do is therefore corruption, which `ledger-admin
  verify` already reports (`check_parent_rows`).
- The scalar algorithm never consulted the index; it followed bytes. Where the index
  *contradicts* the bytes (a `commit_parents` position-0 row naming another parent, or a
  parent for a genesis) the scalar path silently followed the bytes and the windowed path
  fails with `CorruptObject`. This is the one intended, documented behaviour difference,
  and it only applies to databases that `ledger-admin verify` already classifies as
  corrupt. A *missing* row is not a contradiction: the windowed path follows the bytes
  there too, so a missing commit behind a silent index is still `NotFound`, as before.
  **Decision 1** below.

## M0 — the scalar algorithms as implemented at `d05113d`

### `WorkflowRepository::state_at_on(conn, head, limits)` (`postgres_workflow.rs`)
```text
chain := []            # patch ids, head first
seen  := {}            # cycle detection
cursor := Some(head)
while cursor = Some(c):
    if !seen.insert(c):            -> CorruptObject{id: c, "commit cycle"}
    if seen.len() > max_depth:     -> ResourceLimit("reconstruction depth exceeds {max_depth} commits")
    bytes := SELECT bytes FROM immutable_objects WHERE id = $1        # statement 1 of 2
        None                       -> NotFound(c)
        sha256(bytes) != c         -> CorruptObject{id: c, "stored bytes do not hash to id"}
    commit := decode_commit_object(c, bytes)
        Ok(None) (not a commit)    -> NotFound(c)
        Err(e)                     -> e   (CorruptObject "commit ID mismatch" / "commit envelope with a
                                           known header does not decode: …", UnknownCommitVersion, …)
    cursor := commit.parents()[0]  # bytes are authoritative; the index is never read
    chain.push(commit.patch())
state := {}; total_bytes := 0
for patch_id in chain.reverse():   # genesis first
    bytes := SELECT bytes FROM immutable_objects WHERE id = $1        # statement 2 of 2
        None                       -> NotFound(patch_id)
        sha256(bytes) != patch_id  -> CorruptObject{id, "stored bytes do not hash to id"}
    patch := validate_patch_bytes(patch_id, bytes)                    # InvalidPatch{…} if not canonical
    apply_bounded(state, total_bytes, patch, limits)                  # ResourceLimit (quads / bytes), checked per patch
return Reconstructed{state, bytes: total_bytes, depth: chain.len()}
```
Properties the windowed version must keep:
- **Error precedence.** All commit-chain errors (cycle, depth, missing, corrupt, undecodable)
  are raised before any patch is read; the depth limit is checked *before* the fetch of
  the (max_depth + 1)-th commit, so a missing commit beyond the limit is `ResourceLimit`,
  not `NotFound`. Patch errors are raised oldest-first, and a patch-related error is
  raised before a later patch's size-limit error.
- The cycle error names the first repeated id; the depth error fires when the
  (max_depth + 1)-th distinct commit is reached.
- Memory is `O(depth)` patch ids plus the state; no envelope is retained.
- The patch-hash reason string is the object reader's ("stored bytes do not hash to id"),
  not `validate_patch_bytes`'s, because the reader checks first.
- Statements: exactly `2 × depth`, no constant statements inside the function. Callers add
  constants: `reconstruct` acquires a pool connection (0 statements); the API state read
  adds the auth/graph-membership reads (Plan 0011: API calls = store calls + 2).

### `Ledger::state_at_bounded` (`lib.rs`)
The same algorithm over the `ImmutableStore` trait (`get_commit`, `get_content`). Used by
the filesystem backend and the fs→pg migration. Not a PostgreSQL hot path; unchanged by
this plan. (Through `PostgresImmutableStore` it would also be 2 statements per ancestor.)

### `GraphParents::parents(commit)` (`postgres_branches.rs`)
```text
parent_count := SELECT parent_count FROM commit_index WHERE id = $1 AND graph_id = $2   # statement 1
    None -> Ok(None)   (unknown to this graph: foreign or absent commits never resolve)
rows := SELECT position, parent_id FROM commit_parents WHERE commit_id = $1 ORDER BY position  # statement 2
    rows.len() != parent_count or positions != 0..n  -> CorruptObject{id, "commit_parents rows disagree with parent_count {n}"}
Ok(Some(parent ids in position order))
```
Statements: 2 per *distinct* commit visited (the `ledger-dag` walk fetches each commit at
most once per call). `analyze_with_ancestries` walks both sides: 4 per level for the
contained preview that Plan 0011 measured; `first_parent_history` 2 per entry;
`is_ancestor` 2 per visited commit until the hit.

### `ledger-dag` traversal (`Walk`)
Iterative DFS with white/grey/black colouring, parents in position order, `max_visited`
counted as distinct commits fetched, deadline checked before and after every provider
call, `UnknownCommit` for a missing parent, `Cycle` on a grey commit. `first_parent_history`
follows position 0 only and reports a repeated id as `Cycle`. Merge base = maximal
elements of the common ancestry; ahead/behind = set differences; `analyze` classifies
equal / source-contained / fast-forward / divergent from the two recorded ancestries.

### Statement census (constant parts around the per-ancestor parts)
| path | per-ancestor / per-visited-commit statements | constant statements around them |
|---|---|---|
| `GET …/commits/{c}/state` | 2 per ancestor | auth/tenant, `commit_graph` membership (≈ 2) |
| `prepare` | 2 per ancestor of the expected head | idempotency lock/lookup, graph status, branch `FOR SHARE`, ref read, object publication ×3, index rows, verification, proposal, result (≈ 15) |
| merge preview, contained/equal | 2 per visited commit per side | `readable_graph`, two `branch_head` reads |
| merge preview, divergent | 2 per visited commit per side + 3 × (2 per ancestor) | the same + nothing else |
| branch log (`first_parent_history`) | 2 per entry | branch read |
| historical branch point (`is_ancestor`) | 2 per visited commit until the hit | source ref read, index existence |
| `ledger-admin verify` merge rows | as preview + a fourth reconstruction | per row reads |

### Indexes available (migrations 0002/0003; no change in this plan)
- `immutable_objects(id)` primary key (`id = $1` and `id = ANY($1)` are index lookups);
- `commit_index(id)` primary key, `UNIQUE (graph_id, id)`;
- `commit_parents(commit_id, position)` primary key (a position-0 probe is one index
  lookup), `commit_parents_by_parent (parent_id)`, `UNIQUE (commit_id, parent_id)`.
No index is added. The recursive queries below probe `commit_parents` by its primary key
and `immutable_objects`/`commit_index` by primary key, K times per window.

### Query plans (M0, diagnostic run 2026-10-07)
`pg_retrieval::explain_window_queries` (ignored; `--nocapture`) builds a 3,000-deep
constant-state history (1,000 quads, 6,000 objects) in a throwaway database on the
workstation's compose PostgreSQL 17.2 and prints `EXPLAIN (ANALYZE, BUFFERS)` of the three
window statements and the three scalar statements. Warm cache, all buffers shared hits.

| statement | rows | execution | buffers | plan |
|---|---:|---:|---:|---|
| chain window, K = 1 | 1 | 0.083 ms | 6 | recursive union: WorkTable scan → `commit_parents_distinct` index probe per step; `immutable_objects_pkey` probe per served row |
| chain window, K = 64 | 64 | 0.64 ms | 387 | same, 3 buffers per step and 3 per object |
| chain window, K = 256 | 256 | 2.0 ms | 1,536 | same (≈ 8 µs per ancestor) |
| chain window, K = 1,024 | 1,024 | 7.5 ms | 6,144 | same (≈ 7 µs per ancestor); the sort of the CTE is 145 kB |
| object window, 256 patches | 256 | 2.0 ms | 245 | `unnest` function scan hash-joined with a **sequential scan** of the 6,000-row `immutable_objects` (the planner's choice on a table this small; on a large table the primary key is used) |
| ancestry window, K = 256, all parents | 256 | 7.5 ms | 1,878 | recursive union with two index-only probes per step (`commit_parents_distinct`, `commit_index_graph_commit`); `DISTINCT ON` + sort + `LIMIT`; final join by `commit_parents_pkey` |
| scalar `SELECT bytes … WHERE id = $1` | 1 | ≈ 0.02–0.08 ms | 3 | primary-key probe |

Reading: a window's execution is linear in K at ≈ 8 µs per ancestor for the chain
(Plan 0011 measured ≈ 30 µs of execution plus ≈ 170 µs of round trip per ancestor on the
scalar path), so even the PostgreSQL-side work falls; a K = 256 window costs 2 ms of
execution and one round trip. The ancestry window costs ≈ 29 µs per commit of execution
(the recursive union with `UNION` deduplication and the `DISTINCT ON`), still one round
trip per 256 commits instead of two per commit. No sequential scan appears on the
recursive paths; the one on `immutable_objects` is the small-table planner choice and is
re-checked in M4 on the benchmark database (≈ 30,000 objects).

## Design (M1–M3)

### Windows and bounds
```text
RECONSTRUCTION_WINDOW_OBJECTS = 256   commits per chain window, patches per patch window
RETRIEVAL_WINDOW_BYTES        = 8 MiB object bytes returned per window, cut in SQL (the
                                first object of a window is always served, so a window
                                holds at most 8 MiB + one object; one object is at most
                                the ingest body limit, 2 MiB by default)
ANCESTRY_WINDOW_COMMITS       = 256   commits per DAG prefetch window; SQL recursion depth
                                bound = window - 1; also capped at max_visited - visited
maximum total traversal visits: unchanged (`TraversalLimits::max_visited`, counted as
                                distinct commits entered by the walk, not as rows fetched)
reconstruction depth limit:   unchanged (`ReconstructionLimits::max_depth`, checked before
                                the (max_depth + 1)-th commit is used, as today)
deadline:                     DAG walks keep `TraversalLimits::deadline`, checked around
                                every provider call and on every cache hit; reconstruction
                                has no store-level deadline today and gains none (the
                                HTTP request timeout and PostgreSQL `statement_timeout`
                                bound it, as before)
memory:                       reconstruction: O(depth) patch ids + the bounded state +
                                one window (≤ 256 envelopes, ≤ 8 MiB + 1 object of
                                patches); DAG walk: the colour map (≤ max_visited) + a
                                parent cache of at most max_visited + window entries
```
Why 256: Plan 0011 puts one round trip at ≈ 100 µs on the measured topology and the fold
at ≥ 10 µs per level; at K = 256 the amortized round trip is < 1 µs per level, under 10 %
of the fold, and a window's recursive query is 256 primary-key probes (sub-millisecond,
see the plans). Larger windows buy nothing measurable and cost memory. The constants are
crate-internal; tests exercise windows of 1, 2, 3 and larger through a
`test-hooks`-gated setter so the exact-window, one-over-window and short-history cases
are cheap. They are not operator configuration: Phase 6C does not add a limit.

### Expected statement complexity (written before implementation)
```text
reconstruction, depth d:        2 × ceil(d / 256)          (was 2 × d)
                                + ceil(patch_bytes / 8 MiB) extra patch windows when the
                                  byte cut fires before 256 patches (the count bound still
                                  holds: never more than 2 × d)
ancestry walk, N visited:       ceil(N / 256) per side      (was 2 × N per side)
contained preview, depth d:     2 × ceil(d / 256) + 3 constants (was 4 × d + 3)
divergent preview, depth d:     2 × ceil(d / 256) + 3 × 2 × ceil(d / 256) + 3  (was 10 × d + 3)
first-parent history, n:        ceil(n / 256)               (was 2 × n)
```
The gate: at depth 5,000 the reconstruction issues 40 statements, not 10,000; the contained
merge walk 40, not 20,000.

### M1 — `fetch_objects_window(conn, ids, max_bytes)`
One statement, bound array parameter, request order preserved through `unnest … WITH
ORDINALITY`, a running byte total computed in SQL (`octet_length` reads the varlena size
without detoasting) so the served rows are the longest prefix whose preceding bytes stay
under the budget (the first row always), `LEFT JOIN` so a missing id is an explicit
`None`, rows re-sorted locally by ordinal and checked to be a contiguous prefix, each
returned object re-hashed against its requested id (`CorruptObject "stored bytes do not
hash to id"`, the scalar reader's wording). Returns the served prefix; the caller continues
from the first unserved id. Count is capped by the caller (≤ 256 ids) and asserted.

### M2 — windowed `state_at_on`
Phase A (chain): while `cursor = Some(c)`: if `seen.len() >= max_depth` the next commit
would exceed the limit → the scalar `ResourceLimit` wording without a fetch. Otherwise one
statement anchored at `c`: a recursive CTE over `commit_parents` position 0 of at most
`min(256, max_depth + 1 - seen.len()) + 1` ids (one more than served, so every served row
has an exact `next_hint`: the hinted parent id, or `NULL` when the index has no position-0
row), `LEFT JOIN immutable_objects` for the served rows, the byte cut, rows ordered by
depth. Locally, in depth order: cycle check and depth check exactly as the scalar loop,
`None` bytes → `NotFound(id)`, hash check, `decode_commit_object`, then `decoded
parents[0] == next_hint` (both absent or equal) else `CorruptObject{id, "commit_parents
position 0 disagrees with the commit bytes"}`; push the patch id; the window's last decoded
`parents[0]` (from bytes) becomes the next anchor. Phase B (patches): the chain reversed,
in windows of ≤ 256 ids through `fetch_objects_window`; for every id in order: `None` →
`NotFound`, then `validate_patch_bytes`, then `apply_bounded`. Precedence, wording,
`Reconstructed{state, bytes, depth}` and the connection are unchanged.

Hint rule, exactly: `(decoded parents[0], next_hint)` = `(Some(p), Some(h))` with `p ≠ h`
or `(None, Some(_))` is `CorruptObject{"commit_parents position 0 disagrees with the commit
bytes"}`; `(Some(p), None)` (a silent index, no position-0 row) follows `p` from the bytes
like the scalar walk, and the next window is anchored there; `(None, None)` is genesis.

### M3 — `ParentProvider::ancestry_window`
```rust
async fn ancestry_window(&self, start: &CommitId, max: usize)
    -> Result<Vec<(CommitId, Vec<CommitId>)>, Self::Error>  // default: parents(start) only
```
`Walk` keeps a `known` map filled from windows; `load(id)` serves from it, otherwise asks
for a window anchored at `id` with `max = min(window, max_visited - visited)` (never 0),
and reports `UnknownCommit(id)` when the anchor is not in the reply. The visit limit counts
commits entered; the deadline is checked before/after each window call and on each cache
hit. `first_parent_history` asks for first-parent windows. `GraphParents` implements the
hook with a bounded recursive CTE (`UNION`, depth ≤ max − 1, parents joined through
`commit_index` of the same graph so a foreign graph is never walked, `DISTINCT ON (id)`
then `ORDER BY depth, id LIMIT max`), returning for each commit its `parent_count` and
`commit_parents` rows; contiguity is checked locally with today's wording. The
`parents` method stays as it is (window = 1 reproduces today's behaviour exactly; the
differential tests use it as the reference).

## Work
- [x] M0: plan, algorithms, census, bounds, complexity (this document); query plans
- [x] M1: `fetch_objects_window` (+ `fetch_first_parent_window`), covered by the PG
      differential tests (byte cut, missing rows, order, hash) — `pg_retrieval`
- [x] M2: windowed `state_at_on`; scalar reference kept under `test-hooks`; windows setter
- [x] M3: `ancestry_window` in `ledger-dag` (+ `MemoryDag` windows, property tests);
      `GraphParents` window query; `first_parent_history` first-parent windows
- [x] Corruption/adversarial matrix (PG), differential tests (PG + in-memory), transaction
      ownership tests, criss-cross/two-parent tests, statement-count tests
- [ ] Gates: check-fast, PG 17 + PG 15 suites, integration, upgrade, benchmark ci/bear,
      verify, fuzz, supply chain, container, hosted CI
- [ ] M4: before/after `recon` on this host from clean worktrees; residual estimate;
      checkpoint decision
- [ ] Independent reviews (8), P0/P1 resolved
- [ ] Docs reconciled; completion report

## Decisions
1. **Index/bytes disagreement is corruption in the windowed path** (see Assumptions). The
   scalar path followed bytes and ignored the index. ADR-0012 defines the index as a
   derived view that must agree with the bytes, `ledger-admin verify` reports a
   disagreement as corruption, and the task requires that a hint contradicted by the bytes
   is never followed silently. Reconstruction therefore fails closed on such a database.
   Recorded as the only intended behaviour difference; covered by tests on both sides.
2. **Two statements per reconstruction window**, not three: the chain hint and the commit
   bytes are one query (the recursive CTE joined to `immutable_objects`), the patches
   another. A separate "hint then objects" pair would be simpler to reuse but costs 50 %
   more round trips for nothing.
3. **The prefetch hook is a method with a default on `ParentProvider`**, not a second
   trait: every existing provider (including other crates' test providers) keeps compiling
   and behaving as before, and the DAG algorithms stay generic over the one boundary.
4. **Window constants are crate-internal**, exercised through a `test-hooks` setter. No new
   operator limit, no `ReconstructionLimits` field.

## Discoveries
- A recursive CTE's anchor `SELECT 0` is `INT4`; sqlx refuses to decode it as `i64`
  ("mismatched types"), which failed every workflow suite on the first run. Anchors are
  cast to `bigint` explicitly.
- The planner answers the position-0 probe through `commit_parents_distinct`
  `(commit_id, parent_id)` with a filter on `position`, not through the primary key; both
  are one index probe per step.
- `pg_least_privilege` fails on this workstation when its tests run in parallel against the
  shared database (five tests) and passes with `--test-threads=1`; observed before the
  change was made (tech-debt entry).
- Hint rule refinement from the corruption matrix: "missing commit object and its index
  rows" must stay `NotFound` (the scalar answer); treating a *silent* position-0 hint as a
  contradiction would have turned it into `CorruptObject`. The rule now distinguishes a
  contradicted hint (corruption) from a silent one (followed from the bytes).
- A commit of another graph cannot be made by rewriting `commit_index.graph_id` on a
  workflow-built history (`commit_index_graph_fk`, `proposals_candidate_fk`); the foreign
  cases point a `commit_parents` row at a commit published under a second graph instead.

## Risks
- Error precedence drift between the scalar and windowed paths: mitigated by the
  differential tests on corrupted databases and by verifying windows in chain order,
  stopping at the first error.
- Recursive-CTE row growth on merge-heavy DAGs (`(id, depth)` pairs): bounded by the
  depth bound and `LIMIT`; measured on the merge-heavy synthetic history in M0/M4.
- Shallow-history regression (a recursive query costs more than one primary-key read):
  measured and reported at depth 1 and 10, not hidden.

## Gates
`check-fast` (fmt, clippy, tests, architecture, doc links, doc consistency, goldens);
`ledger-store --features postgres` and `ledger-api` PostgreSQL suites on 17 and 15;
`test-integration.sh`; `upgrade-p5.sh`; `benchmark.sh ci` and `bear`; `ledger-admin
verify`; fuzz; `check-supply-chain.sh`; container/security; hosted CI on the PR merge
result; official `recon` from a clean worktree at the final revision, plus the same at the
base revision on the same host.

## Evidence
| Gate | Revision | Result |
|---|---|---|
| `ledger-dag` unit tests (28, incl. the window property test over 60 seeded DAGs × 5 window sizes, deep-linear window-call counts, limits/cycles/unknowns across windows, evasive-provider test) | working tree, 2026-10-07 | 28 passed |
| `pg_retrieval` (PostgreSQL 17.2): scalar-vs-windowed equivalence at depths 1–20 with 10 window configurations, merge commit, limits exact/+1, quads/bytes limits, absent head, patch-as-head; v1 imported + v2 history; ≈ 200 KiB patches under 100 KiB / 1-byte budgets; corruption matrix (13 cases × 10 windows); DAG equivalence (12 branch pairs × 4 strategies × 8 windows, first-parent histories, tight limits, 42 historical branch points); DAG corruption matrix (6 cases × 8 windows); one-connection prepare/preview; statement counts (`2 × ceil(1000 / w)` for w ∈ {256, 100, 1, byte-cut, 10,000}; history `ceil(1000 / w)`; contained and divergent previews at depth 300) | working tree, 2026-10-07 | 9 passed |
| Existing PostgreSQL store suites with the windowed code (`pg_immutable_store` 9, `pg_workflow` 14, `pg_branches` 18, `pg_merge` 27, `pg_verify` 4, `pg_validation` 9, `pg_projection` 7, `pg_cas_race` 1, `pg_fs_migration` 8, `pg_graphs_migration` 8, `pg_least_privilege` 19 with `--test-threads=1`) and API suites (`pg_api` 13, `pg_validation_api` 23) | working tree, 2026-10-07 | all passed |

## Deferred work
- `Ledger::state_at_bounded` over the trait (filesystem backend) stays scalar.
- The checkpoint ADR inputs are updated from the M4 residual, not implemented.

## Completion criteria
Every box above checked with evidence; statement counts in the official `recon` scale with
windows (formula above), not with depth; every corruption and differential test passes;
no P0/P1 review finding open; `CHECKPOINT-ADR-READY` restated with the post-batching
residual.
