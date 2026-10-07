# Plan 0012: Phase 6C — batched reconstruction and DAG/ancestry retrieval

Status: **in progress** (started 2026-10-07). Branch `claude/p6c-batched-retrieval` from the
Phase-6B head `d05113d` (PR #13 head `aabdd11` plus the two Codex P2 fixes of 2026-10-07),
which is `main` at `646b029` plus the reviewed Phase-6B changes. Continues
[Plan 0011](../completed/0011-phase6b-bear-reconstruction-characterization.md).

**Base reconciliation (recorded, not yet resolved).** The task required starting from the
`main` that results from merging PR #13. At the start of this plan PR #13 was still open
(CI green, mergeable, two Codex P2 comments outstanding; previous PRs were all merged by the
repository owner, never by the agent). The two comments were fixed on the PR branch, whose
head is now `d05113d`, and this branch was created on top of that head so that the
Phase-6B benchmark tooling (`ledger-bench recon`, `scripts/benchmark-recon.sh`) and the
documents this plan builds on are present. PR #13 touches no production crate
(`git diff --name-only 646b029..d05113d -- crates apps/ledger-server apps/ledger-projector
migrations` lists only three test files), so the production code this plan changes is
byte-identical on `main` and on the Phase-6B head. The M4 "before" measurement is taken at
`d05113d` (identical production code to `main`). **Before this plan's PR is merged:**
(1) PR #13 is merged by the owner; (2) this branch is rebased onto the resulting `main`
and the PR retargeted from `claude/p6b-bear-reconstruction` to `main`; (3) if `main` then
differs from `d05113d` in anything but the merge commit, the difference is recorded here;
(4) `check-fast`, the PostgreSQL suites and hosted CI are re-run on the rebased head.
Until then the PR is opened against `claude/p6b-bear-reconstruction`.

**Migration impact:** none (no schema, index or migration change; `git diff d05113d --
migrations` is empty). **Affected crates:** `ledger-dag`, `ledger-store` (plus their tests,
`scripts/test-integration.sh`, documentation). `ledger-api`, `ledger-core`, `ledger-rdf`,
protocol and golden files are untouched.

## Goal
Remove the dominant cost that Phase 6B measured, **≈ 2 PostgreSQL statements per
reconstructed ancestor and ≈ 2 per visited commit per ancestry side**, by retrieving
commits, patches and parent edges in **bounded windows** instead of one row at a time,
while leaving every ledger semantic, identity, limit, error and transaction boundary
exactly as it is on a sound store (the one intended exception, Decision 1: a
`commit_parents` position-0 row that contradicts the commit bytes is now `CorruptObject`
where the scalar walk silently followed the bytes). The deterministic gate is the
statement count; latency is observed.

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
  *contradicts* the bytes (a `commit_parents` position-0 row naming another parent, a
  malformed id, or a parent for a genesis) the scalar path silently followed the bytes and
  the windowed path fails with `CorruptObject`. This is the one intended, documented
  behaviour difference. Such rows cannot be written through any ledger path (publication
  verifies them against the decoded bytes before commit, and the rows are write-once);
  `PostgresImmutableStore::verify_commit_index` (the ADR-0012 re-derivation, reachable from
  the fs→pg migration and tests) reports them, while `ledger-admin verify`'s SQL checks
  catch only a row count disagreeing with `parent_count`, a foreign parent or an unindexed
  parent (tech-debt). A *missing* row is not a contradiction: the windowed path follows
  the bytes there too, so a missing commit behind a silent index is still `NotFound`, as
  before. **Decision 1** below.

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
| `prepare` | 2 per ancestor of the expected head | idempotency lock/lookup, graph status, branch `FOR SHARE`, ref read, object publication ×3, index rows, verification, proposal, result (26 observed in M4) |
| merge preview, contained/equal | 2 per visited commit per side | `readable_graph`, two `branch_head` reads |
| merge preview, divergent | 2 per visited commit per side + 3 × (2 per ancestor) | the same + nothing else |
| branch log (`first_parent_history`) | 2 per entry | branch read |
| historical branch point (`is_ancestor`) | 2 per visited commit until the hit | source ref read, index existence |
| `ledger-admin verify` merge rows | as preview + a fourth reconstruction | per row reads |
| fs→pg migration (admin-only, unchanged) | `Ledger::state_at` over the trait: 2 per ancestor; `get_content` once per object | per object |

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

Second diagnostic run (after the review fixes; 15,000-commit constant-state history, 30,000
objects, plus a 3,000-commit Fibonacci DAG in which every commit after the second has two
parents and is reachable at many depths):

| statement | rows out | execution | plan |
|---|---:|---:|---|
| chain window, K = 256 | 256 | 2.9 ms | recursive union 257 rows, index probes as above |
| object window, 256 patches, 30,000-object table | 256 | 1.24 ms | `unnest` → **nested loop with `immutable_objects_pkey` probes** (the sequential scan of the small-table run is gone; the planner risk the review raised does not materialize at this size) |
| ancestry window, linear, K = 256, cap 1,024 | 256 | 5.4 ms | recursion 256 rows; two index-only probes per step |
| ancestry window, linear first-parent, K = 256 | 256 | 6.1 ms | same, position-0 edges only |
| ancestry window, linear, K = 1,024, cap 4,096 | 1,024 | 38.8 ms | linear in K |
| ancestry window, Fibonacci DAG, K = 256, cap 1,024 | 87 commits (174 rows) | 13.8 ms | the recursion produced **exactly 1,024 `(id, depth)` pairs and stopped** (44 levels, 22 rows per level on average), yielding 87 distinct commits for the window |
| ancestry window, Fibonacci DAG, K = 64, cap 256 | 64 | 3.6 ms | 256 pairs, cap reached |
| scalar primary-key reads | 1 | 0.011–0.016 ms | index probes |

Reading: a window's execution is linear in K at ≈ 8 µs per ancestor for the chain
(Plan 0011 measured ≈ 30 µs of execution plus ≈ 170 µs of round trip per ancestor on the
scalar path), so even the PostgreSQL-side work falls; a K = 256 window costs 2 ms of
execution and one round trip. The ancestry window costs ≈ 29 µs per commit of execution
(the recursive union with `UNION` deduplication and the `DISTINCT ON`), still one round
trip per 256 commits instead of two per commit. No sequential scan appears on the
recursive paths; the one on `immutable_objects` in the first run was the small-table
planner choice and is gone at 30,000 objects (second run). On the one merge-heavy DAG
measured the pair cap bounded the statement at ≈ 14 ms at the price of fewer distinct
commits per window (87 of 256); that the cap bounds every shape is a construction
argument (at most 1,024 recursion rows per statement), not a measurement across shapes.

## Design (M1–M3)

### Windows and bounds
```text
RetrievalWindows::DEFAULT.objects  = 256   commits per chain window, patches per patch window
RetrievalWindows::DEFAULT.bytes    = 8 MiB object bytes returned per window, cut in SQL (the
                                     first object of a window is always served, so a window
                                     holds at most 8 MiB + one object; "one object" is the
                                     largest stored object, which the scalar path read whole
                                     too: an API patch is at most the 2 MiB body limit, a
                                     merge patch at most the 256 MiB state limit, an fs→pg
                                     import is unbounded by the ledger)
RetrievalWindows::DEFAULT.ancestry = 256   commits per DAG prefetch window; SQL recursion
                                     depth bound = window - 1; recursion rows capped at
                                     4 × window (id, depth) pairs (`REACH_PAIRS_PER_COMMIT`),
                                     so one statement is at most 1,024 index-probe steps on
                                     any DAG shape; also capped at max_visited - visited and
                                     at the entries a bounded history still wants
window ramp (`ledger_dag::WINDOW_RAMP`): a walk asks for 1, then 4, 16, 64, then full
                                     windows, so a search that stops after a few commits
                                     (a branch point near the head) costs a few small
                                     statements instead of one full window
maximum total traversal visits: unchanged (`TraversalLimits::max_visited`, counted as
                                distinct commits entered by the walk, not as rows fetched)
reconstruction depth limit:   unchanged (`ReconstructionLimits::max_depth`, checked before
                                the (max_depth + 1)-th commit is used, as today)
deadline:                     DAG walks keep `TraversalLimits::deadline`, checked around
                                every provider call and on every cache hit; a walk can
                                overshoot it by at most one window statement (bounded by
                                the pair cap above; `statement_timeout` remains the hard
                                bound); reconstruction has no store-level deadline today
                                and gains none (the HTTP request timeout and PostgreSQL
                                `statement_timeout` bound it, as before)
memory:                       reconstruction: O(depth) patch ids + the bounded state +
                                one window (≤ 256 envelopes, ≤ 8 MiB + 1 object of
                                patches); DAG walk: the colour map (≤ max_visited) + a
                                parent cache capped at max_visited entries
```
Why 256: Plan 0011 puts one round trip at ≈ 100 µs on the measured topology and the fold
at ≥ 10 µs per level; at K = 256 the amortized round trip is < 1 µs per level, under 10 %
of the fold, and a chain window of 256 executes in 2.0 ms (≈ 8 µs per ancestor, M0
table). Larger windows buy nothing measurable and cost memory. The values are fixed
public constants (`ledger_store::RetrievalWindows::DEFAULT`), changeable only through a
`test-hooks`-gated setter so tests exercise windows of 1, 2, 3 and larger (exact-window,
one-over-window and short-history cases). They are not operator configuration: Phase 6C
does not add a limit.

### Expected statement complexity (written before implementation)
```text
reconstruction, depth d:        2 × ceil(d / 256)          (was 2 × d)
                                + extra windows when the 8 MiB byte cut fires before 256
                                  objects (the count bound still holds: never more than
                                  2 × d)
ancestry walk, N commits in a   window_calls(N, 256) = 4 + ceil((N − 85) / 256) per side
  row (linear history):         for N > 85 (the ramp 1, 4, 16, 64, then full windows)
                                (was 2 × N per side)
ancestry walk, merge-heavy DAG: between window_calls(N, 256) and N statements per side
                                (a window holds fewer distinct commits when the recursion
                                reaches commits at several depths and stops at the pair
                                cap); each statement bounded as above
contained preview, depth d:     2 × window_calls(d, 256) + constants, linear (was 4 × d + constants)
divergent preview, depth d:     2 × window_calls(d, 256) + 3 × 2 × ceil(d / 256) + constants
                                (was 10 × d + constants); the constants observed in M4 are 4
                                for a preview and 26 for prepare
first-parent history, n:        window_calls(n, 256), never a window beyond the n wanted
                                (was 2 × n)
```
The gate: at depth 5,000 the reconstruction issues 40 statements, not 10,000; the contained
merge walk 2 × 24 = 48, not 20,000. The linear formulas are pinned exactly by the
`pg_retrieval` statement-count tests (depth 1,000 and 10,000 reconstructions, histories
of 1–1,000 entries, previews at depth 300); the merge-heavy bound by the Fibonacci-DAG
test; the official `recon` profile (M4) confirms them on the production stack.

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
statement anchored at `c`: a recursive CTE over `commit_parents` position 0 that serves
`min(256, max_depth − seen.len())` rows and discovers one id more, so every served row
has an exact `next_hint` (the hinted parent id as the raw index string, or `NULL` when the
index has no position-0 row), `LEFT JOIN immutable_objects` for the served rows, the byte
cut, rows ordered by depth. Locally, in depth order, each row is consumed as the commit it
must be (the anchor, then the previous row's decoded `parents[0]`; the index never chooses
it, and index strings are never parsed before the bytes that name them are decoded):
cycle check and depth check exactly as the scalar loop, `None` bytes → `NotFound(id)`,
hash check against the trusted id, `decode_commit_object`, then the hint rule below; push
the patch id; the window's last decoded `parents[0]` (from bytes) becomes the next anchor. Phase B (patches): the chain reversed,
in windows of ≤ 256 ids through `fetch_objects_window`; for every id in order: `None` →
`NotFound`, then `validate_patch_bytes`, then `apply_bounded`. Precedence, wording,
`Reconstructed{state, bytes, depth}` and the connection are unchanged.

Hint rule, exactly: `(decoded parents[0], next_hint)` = `(Some(p), Some(h))` with `p ≠ h`
or `(None, Some(_))` is `CorruptObject{"commit_parents position 0 disagrees with the commit
bytes"}`; `(Some(p), None)` (a silent index, no position-0 row) follows `p` from the bytes
like the scalar walk, and the next window is anchored there; `(None, None)` is genesis.

### M3 — `ParentProvider::ancestry_window`
```rust
async fn ancestry_window(&self, start: &CommitId, max: usize, kind: WindowKind)
    -> Result<Vec<(CommitId, Vec<CommitId>)>, Self::Error>  // default: parents(start) only
```
`Walk` keeps a `prefetched` map filled from windows; `load(id)` serves from it, otherwise
asks for a window anchored at `id` with `max = min(window, max_visited - visited, entries
the caller still wants)` (never 0), and reports `UnknownCommit(id)` when the anchor is not
in the reply. A provider reports damage only for the anchor; a damaged prefetched commit
is left out of the window and, if the walk needs it, fails when it is asked for as an
anchor — so a bounded history or an early-exit reachability check that never reaches the
damage answers exactly as the unwindowed walk did (review finding, M3). The visit limit counts
commits entered; the deadline is checked before/after each window call and on each cache
hit. `first_parent_history` asks for first-parent windows capped at the entries still wanted.
`GraphParents` implements the hook with a bounded recursive CTE (`UNION`, depth ≤ max − 1,
parents joined through `commit_index` of the same graph so a foreign graph is never
walked, the recursion's output capped at 4 × max `(id, depth)` pairs so a merge-heavy DAG
bounds the statement's work instead of multiplying it, `DISTINCT ON (id)` then `ORDER BY
depth, id LIMIT max`), returning for each commit its `parent_count` and `commit_parents`
rows; contiguity is checked locally with today's wording, and only the anchor's own
damage is reported (a damaged prefetched commit is left out and fails when asked for as
an anchor). The `parents` method stays as it is (window = 1 reproduces today's behaviour
exactly; the differential tests use it as the reference).


## M4 — characterization (before/after, same host, clean worktrees)

Two official `recon` runs (`scripts/benchmark-recon.sh`, default sweep, `official=yes`, no
tracked or untracked changes) from detached worktrees on the same workstation (16 cores,
31 GiB, Docker 29.8.1, PostgreSQL 17.2 with the benchmark-only instrumentation override,
same Dockerfile, compose files, PostgreSQL image digest and toolchain; the build-input
hash differs only by the two `ledger-store` dev-dependency edges in `Cargo.lock`):
**before** at `d05113d` (the Phase-6B head; production code identical to `main`) and
**after** at `7408a3e` (the Phase-6C code head), run one after the other. Both: `RECON
PASS`, 144 points, 186 exact oracle checks, 0 failures, `VERIFY OK`. Evidence:
[before](../../quality/evidence/benchmarks/2026-10-07-phase6c-recon-before/recon.md),
[after](../../quality/evidence/benchmarks/2026-10-07-phase6c-recon-after/recon.md) (the
JSON files are authoritative; `verify.log` and `run.log` are archived beside them). The
before run's history-build phase (not its measurement phase) overlapped a compile and
test job; the result files cannot show host load, and `fold_cpu` (identical code in both
runs) deviates only sporadically at single points in both runs, so no systematic
slowdown of either measurement phase is visible. This host differs from the Plan 0011
workstation (CPU, kernel, Docker); like-for-like on the API state read it is 1.8–2.3×
faster per level (97–105 µs here versus 189–225 µs there), so only same-host
before/after comparisons are made below. In this profile "depth d" is the history index
with genesis at 0: a reconstruction at depth d reads d + 1 commits, and the preview
branches sit at d + 1 and d + 2 commits.

Slopes are least-squares fits over depths 100–5,000; "calls" are `pg_stat_statements`
statements per operation.

| S quads | operation | p50 µs per level, before → after | PG statements per level | PG execution µs per level | depth 5,000 p50 ms | depth 5,000 statements | depth 1 p50 ms |
|---:|---|---|---|---|---|---|---|
| 1 | API state read | 105 → 24 | 2.00 → 0.008 | 15.4 → 13.7 | 523 → 121 | 10,004 → 42 | 0.46 → 0.53 |
| 1,000 | API state read | 97 → 26 | 2.00 → 0.008 | 13.7 → 15.0 | 492 → 132 | 10,004 → 42 | 2.32 → 2.37 |
| 10,000 | API state read | 98 → 27 | 2.00 → 0.008 | 14.3 → 15.7 | 506 → 148 | 10,004 → 42 | 18.2 → 17.6 |
| 1 | store reconstruction (`persisted`) | 131 → 25 | 2.00 → 0.008 | 14.2 → 14.3 | 657 → 124 | 10,002 → 40 | 0.28 → 0.47 |
| 1 | prepare | 91 → 26 | 2.00 → 0.008 | 13.2 → 14.1 | 457 → 131 | 10,028 → 66 | 2.52 → 2.36 |
| 1 | contained merge preview (ancestry walk only) | 173 → 40 | 4.00 → 0.008 | 28.9 → 34.8 | 867 → 202 | 20,010 → 52 | 0.90 → 1.52 |
| 1 | divergent merge preview (walk + 3 reconstructions) | 459 → 116 | 10.00 → 0.031 | 69.1 → 80.7 | 2,307 → 579 | 50,022 → 172 | 1.96 → 1.88 |
| 1 | API state read after a database restart | 94 → 26 | 2.00 → 0.008 | 14.7 → 15.3 | 478 → 135 | 10,004 → 42 | — |

**Statement-count gate (deterministic): passed.** Statements no longer grow at 2 per
ancestor. With n = d + 1 commits in a reconstruction, the observed counts at every
measured depth (1, 10, 100, 500, 1,000, 2,500, 5,000) are exactly these closed forms
(before → after):
- state read: `2n + 2` → `2 + 2 × ceil(n / 256)` (4 at depths 1–100, 10 at 1,000, 42 at
  5,000); the store path without the API's two constants: `2n` → `2 × ceil(n / 256)`;
- prepare: `2n + 26` → `26 + 2 × ceil(n / 256)` (28, 34, 66);
- contained preview (walks of d + 1 and d + 2 commits): `4d + 10` →
  `4 + window_calls(d + 1, 256) + window_calls(d + 2, 256)` (8 at depth 1, 20 at 1,000,
  52 at 5,000; the walks ramp 1, 4, 16, 64, then 256);
- divergent preview: `10d + 22` → `4 + 2 × window_calls(d + 2, 256) + 2 × ceil((d + 1) /
  256) + 4 × ceil((d + 2) / 256)` (14, 44, 172).
The counts are identical across the three state sizes. At depth 5,000 that is 238× fewer
statements for a state read, 385× for the contained walk and 291× for the divergent
preview. (The pre-implementation estimate of the constants in "Expected statement
complexity" and the census was low: prepare carries 26, a preview 4.)

**Latency (observed, not gated; one official run per revision, n = 20 per point, n = 10
for previews, n = 3 after a restart):** the per-level cost of a state read fell 3.6–4.3×
(4.3× at S = 1, 3.7× at 1,000, 3.6× at 10,000; 3.6× after a database restart); the store
path 5.2×, prepare 3.5×, the contained walk 4.3×, the divergent preview 4.0×. The
remaining per-level cost of a state read is ≈ 24–27 µs, of which ≈ 14–16 µs is
PostgreSQL execution (measured; about the same per ancestor as before — the windows do
the scalar statements' work with 12 buffer hits per level instead of 8); the other
≈ 10–11 µs per level is **unattributed** (the harness has no per-part latency breakdown;
the server container's CPU slope of 11–13 µs per level is consistent with it, and the
prefetched fold's 5.4–5.6 µs per level in the after run is a lower bound because
production hashes twice and checks limits per patch). PostgreSQL execution is now the
majority of a deep read (51–57 % at depth 5,000 across the state sizes, mean execution
over p50 latency) where it was 14–15 %: the round-trip share is gone. (The history-build
phase was faster too, but that figure mixes the code change with host load and is not
reported as a result.)

**Shallow histories (reported, not hidden; observed once per revision):** at depth 1
(two commits) a state read costs 0.53 ms instead of 0.46 ms p50 (p95 0.70 vs 0.62; 4
statements instead of 6), the store path 0.47 instead of 0.28 ms (p95 0.53 vs 0.37; 2
windows instead of 4 primary-key reads), and the contained preview 1.52 instead of
0.90 ms (means 1.7 vs 1.0 at all three state sizes; 4 window statements instead of 10
primary-key reads). PostgreSQL execution grows by only 0.03–0.06 ms per operation in
those cases, so most of the difference is unattributed by this profile (the windows are
heavier statements; planning time is not instrumented). From depth 10 on every path is
faster (state read 0.64 vs 1.40 ms, contained preview 1.75 vs 2.37 ms), and the 1,000- and
10,000-quad states show the same pattern within noise. The difference does not grow with
depth; under `METRICS.md`'s regression policy a single run cannot classify a +0.07 ms
depth-1 difference as a regression, so it is recorded as an observation.

**Not covered by this profile:** the `recon` histories are linear, so the merge-heavy
shape (where a window holds fewer distinct commits) and the 8 MiB byte cut (never
triggered by these small patches) are covered only by the M0 diagnostic `EXPLAIN` on one
3,000-commit Fibonacci DAG, the `pg_retrieval` Fibonacci statement-bound test and the
byte-cut equivalence tests, not by latency measurements on the production stack. The
per-statement bound on other DAG shapes is a construction argument from the pair cap,
measured on that one DAG.

**Residual cost and the checkpoint question.** After batching, a reconstruction costs
≈ 25 µs per ancestor plus the state-size cost at the head (≈ 17 ms at 10,000 quads for
JSON encoding and the final fold, unchanged). Extrapolated (not measured) to the
development `max_depth` of 10,000 that is ≈ 0.25–0.3 s per state read or prepare, ≈ 0.4 s
for a contained merge preview and ≈ 1.2 s for a divergent one (116–127 µs per level);
at a hypothetical 100,000 a read would be ≈ 2.5 s. These absolute figures belong to this
host and the instrumented configuration. No product target depth or latency/operational
budget has been declared, and the write ceiling at `max_depth` remains
(production-qualification matrix).

**CHECKPOINT-ADR-READY: NO.** The post-batching residual is measured (above), but the
second requirement — a declared target history depth and latency budget against which it
is too expensive — is still absent, and the plan does not invent one. If a target of the
order of 10,000 commits with a sub-second budget for state reads and prepares is
declared, the extrapolated residual meets it without checkpoints (a divergent merge
preview at that depth would not); a target of 100,000 or more, or a budget of a few tens
of milliseconds at depth, would reopen the question with these numbers as its input.

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
- [x] M4: before/after `recon` on this host from clean worktrees; residual estimate;
      checkpoint decision
- [x] Independent reviews (8), P0/P1 resolved
- [ ] Docs reconciled; completion report

## Decisions
1. **Index/bytes disagreement is corruption in the windowed path** (see Assumptions). The
   scalar path followed bytes and ignored the index. ADR-0012 defines the index as a
   derived view that must agree with the bytes, `verify_commit_index` reports a
   disagreement as corruption, and the task requires that a hint contradicted by the bytes
   is never followed silently. Reconstruction therefore fails closed on such a database,
   and it does so at the contradicted commit, in chain order: on such a database the
   error can precede a `NotFound`, `InvalidPatch`, patch-level `ResourceLimit` or even the
   depth limit that the scalar walk would have reported later (HTTP 500 where it was 404
   or 413). Every state read, prepare, validation, projection and merge-row verification of
   that head is affected. Operators upgrading a database that was never verified by
   `verify_commit_index` should run it first (tech-debt: make it part of `ledger-admin
   verify`). Recorded as the only intended behaviour difference; covered by tests on both
   sides, including the precedence cases.
2. **Two statements per reconstruction window**, not three: the chain hint and the commit
   bytes are one query (the recursive CTE joined to `immutable_objects`), the patches
   another. A separate "hint then objects" pair would be simpler to reuse but costs 50 %
   more round trips for nothing.
3. **The prefetch hook is a method with a default on `ParentProvider`**, not a second
   trait: every existing provider (including other crates' test providers) keeps compiling
   and behaving as before, and the DAG algorithms stay generic over the one boundary.
4. **Window constants are fixed public values** (`RetrievalWindows::DEFAULT`), changeable
   only through a `test-hooks` setter. No new operator limit, no `ReconstructionLimits`
   field.
5. **The walk ramps its window** (1, 4, 16, 64, then the provider's window) instead of
   asking for a full window on the first miss, so a branch point near the head or a short
   history page costs about what it did, while deep walks reach full windows after four
   statements. Review finding (shallow regression of `is_ancestor`).
6. **The ancestry recursion is capped at 4 (id, depth) pairs per requested commit.** On a
   merge-heavy DAG the recursion reaches one commit at many depths; without the cap a
   single statement's work was bounded only by the depth bound (review finding, four
   reviewers). With it a statement is at most 1,024 recursion rows by construction
   (measured on one Fibonacci DAG), and a window may hold fewer distinct commits (more
   statements, each bounded).
7. **Only the anchor's damage is reported by a window.** A damaged prefetched commit is
   left out, so bounded histories and early-exit reachability checks that never reach it
   answer as the unwindowed walk did (review finding).

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
- Recursive-CTE row growth on merge-heavy DAGs (`(id, depth)` pairs): bounded by the pair
  cap (Decision 6); the M0 diagnostic prints the plan on a 3,000-commit Fibonacci DAG and
  the `pg_retrieval` Fibonacci test pins the statement bound; the official `recon`
  profile is linear and does not cover this shape (reported as such in M4).
- Shallow-history regression (a recursive query costs more than one primary-key read):
  the ramp (Decision 5) removes it for walks; depth-1 and depth-10 reconstruction is
  reported from the M4 before/after runs, not hidden.

## Gates
`check-fast` (fmt, clippy, tests, architecture, doc links, doc consistency, goldens);
`ledger-store --features postgres` and `ledger-api` PostgreSQL suites on 17 and 15;
`test-integration.sh`; `upgrade-p5.sh`; `benchmark.sh ci` and `bear`; `ledger-admin
verify`; fuzz; `check-supply-chain.sh`; container/security; hosted CI on the PR merge
result; official `recon` from a clean worktree at the final revision, plus the same at the
base revision on the same host.

## Reviews (independent, bounded sub-agents; all P0/P1 findings resolved)
| Review | Result | Resolution |
|---|---|---|
| Storage / corruption invariants | no P0; P1: chain-window index strings were parsed before the bytes that name them (a malformed `parent_id` became `InvalidContentId` ahead of the scalar `NotFound`); P1 (shared with the DAG review): windows failed on damaged commits the walk never needed; P2: zero test window looped, an over-long reply could panic, matrix gaps | Rows are consumed as the commit the previous bytes named and index strings compared as text; anchor-only errors; `checked()` on entry and in the setter; `.get()` instead of indexing; matrix extended (unknown envelope version, malformed mid-chain, four precedence cases, malformed index id, contradicted hint at the limit, duplicate patch ids, v1 damage, byte cuts at ±1). Decision 1's precedence consequence and the upgrade note recorded. |
| DAG / merge semantic equivalence | no P0; P1: a damaged prefetched commit beyond a bounded history or an early hit failed the call | Only the anchor's damage is reported; first-parent windows capped at the entries still wanted; tests for both (PG and in-memory). Every listed semantic marked preserved afterwards. |
| PostgreSQL query and resource bounds | P1: the ancestry recursion's work was bounded by depth, not by the window (merge-heavy DAGs); P1: the "one object ≤ 2 MiB" bound was false (merge patches, imports); P1/P2: evidence and doc claims ahead of measurement; P2: planner risk on the object window, shallow `is_ancestor` regression, deadline overshoot | Recursion capped at 4 pairs per requested commit (measured on a Fibonacci DAG: exactly 1,024 rows); the walk ramps 1/4/16/64/256; the object bound reworded; planner re-checked at 30,000 objects (primary key used); merge-heavy and shallow cases labelled as diagnostic/observed; deadline overshoot documented. |
| Concurrency / transactions | no P0/P1; P2: zero window loop (test-hooks only), one-connection test did not walk the DAG, Decision 1 upgrade consequence | Fixed; the one-connection test now walks, previews, proposes and applies; `deployment.md` carries the upgrade note. Verdict per path: prepare, reads, branch creation, preview, propose, apply, concurrent ref movement unchanged. |
| Security / tenant isolation | no P0; P1: the same unbounded recursion work (client-triggerable through preview and branch creation) | Same fix. SQL construction, graph scoping, entry points, error hygiene, test-only exclusion and privileges all pass. |
| Test completeness | P1: `pg_retrieval` was not in `test-integration.sh`; P1: the statement-count arithmetic no longer matched after the history cap; P1: precedence untested, previews never compared with the scalar reconstruction; P2: merge-heavy branch missing, genesis-cycle case not a cycle, carve-outs, throwaway databases never dropped | Wired into the script; counts recomputed along the ramp on a dedicated connection with statement summaries; four precedence cases; every preview input through the scalar reference; 10-round merge-heavy branch with pinned classes; cycle reasons pinned; databases dropped with `FORCE`. |
| Architecture / documentation consistency | P1: "`ledger-admin verify` reports the disagreement" was false; P1: "no error changed" contradicted Decision 1; P1: "the index is never an authority" overstated for DAG walks; P2: stale formulas, constant names, "crate-internal", evidence revisions, unmeasured claims | All corrected (`verify_commit_index` is the re-derivation; tech-debt asks for it in `ledger-admin verify`); boundaries, dependency direction and the no-migration/no-API claims confirmed. |
| Benchmark methodology (M4) | gate and conclusion supported; P1: preview formulas off by the harness's depth index, "4× everywhere" overstated (3.5–5×), build-time comparison contaminated, "≈ 10 µs server" and the depth-1 bound not measured; P2: labels (inputs hash, 0.031, 51–57 %, fold caveat, cross-host factor, "any shape", byte cut, linear-only) | All corrected in the plan, baselines, qualification matrix, roadmap note, METRICS, BENCHMARK_ARCHITECTURE; `verify.log`/`run.log` archived; before/after protocol added to RUNNING_BENCHMARKS. |

## Evidence
Revisions: `e9a8681` is the first implementation checkpoint; the review-driven fixes
(anchor-only window errors, first-parent window cap, lazy chain-row checks, recursion
pair cap, window ramp, zero-window guards) and the reworked tests are the next commit
(recorded below as "review fixes"); hashes are updated as the branch advances.

| Gate | Revision | Result |
|---|---|---|
| `ledger-dag` unit tests: 32 (the window property test over 60 seeded DAGs × 5 window sizes; generated back edges and visit limits under every window; deep-linear window-call counts along the ramp; limits/cycles/unknowns across windows; damage beyond a bounded history; evasive and overfilling providers; `window_calls`) | review fixes | 32 passed |
| `pg_retrieval` (11 tests) on PostgreSQL 17.2 and 15.19, default parallelism: scalar-vs-windowed equivalence at depths 1–20 × 10 window configurations, merge commit, limits exact/+1, quads/bytes limits, absent head, patch-as-head; v1 imported + v2 history; ≈ 200 KiB patches under 100 KiB / 1-byte budgets; duplicate patch ids and byte cuts at ±1 of every prefix; corruption matrix (24 cases incl. v1 damage, unknown envelope version, a malformed envelope at a window boundary, four precedence cases, a malformed index id, a contradicted hint at the depth limit; × 10 windows, blamed ids pinned); DAG equivalence (15 branch pairs with pinned classes incl. criss-cross and a 10-round merge-heavy branch × 4 strategies × 8 windows; every preview input reconstructed through the scalar reference; first-parent histories; tight limits; 4 × 14 historical branch points); DAG corruption matrix (7 cases × 8 windows, reasons pinned, damage beyond bounded reads invisible); one-connection prepare / preview / propose / apply; statement counts on a dedicated one-connection pool (`2 × ceil(1000 / w)` for w ∈ {256, 100, 1, byte-cut, 10,000}; 80 statements at the exact 10,000 depth limit and the same refusal one beyond it; histories of 1–1,000 entries × 6 windows along the ramp; contained and divergent previews at depth 300); Fibonacci DAG of 600 (bounded, ≥ 4× fewer statements than scalar) | review fixes | 11 passed on 17.2; 11 passed on 15.19 |
| Existing PostgreSQL store suites with the windowed code on 17.2 (`pg_immutable_store` 9, `pg_workflow` 14, `pg_branches` 18, `pg_merge` 27, `pg_verify` 4, `pg_validation` 9, `pg_projection` 7, `pg_cas_race` 1, `pg_fs_migration` 8, `pg_graphs_migration` 8, `pg_least_privilege` 19 with `--test-threads=1`) and API suites (`pg_api` 13, `pg_validation_api` 23) | `e9a8681` + fixes | all passed |
| The same store and API suites on PostgreSQL 15.19 (`--test-threads=1`) | review fixes | all passed (9, 14, 18, 27, 4, 9, 7, 1, 8, 8, 19; 13, 23) |
| `check-fast` (fmt, clippy `-D warnings` incl. test targets, workspace tests, architecture, doc links, doc consistency, goldens) | `e9a8681` | pass (re-run on the final head recorded below) |
| `check-supply-chain.sh` (cargo audit with the one documented exception re-proven, cargo deny advisories/licenses/bans/sources, CycloneDX SBOMs) | review fixes (lockfile: two dev-dependency edges) | pass |
| M0 query plans | `e9a8681` | see the M0 table |
| Official `recon`, before (clean worktree `d05113d`, `official=yes`) | `d05113d` | `RECON PASS`, 144 points, 186 exact checks, 0 failures; `VERIFY OK` (wall 38 min including the history build, which overlapped a compile job; not a result) |
| Official `recon`, after (clean worktree `7408a3e`, `official=yes`) | `7408a3e` | `RECON PASS`, 144 points, 186 exact checks, 0 failures; `VERIFY OK` (wall 12 min); statement counts per the closed forms in M4 at every depth |

## Deferred work
- `Ledger::state_at_bounded` over the trait (filesystem backend) stays scalar.
- The checkpoint ADR inputs are updated from the M4 residual, not implemented.

## Completion criteria
Every box above checked with evidence; statement counts in the official `recon` scale with
windows (formula above), not with depth; every corruption and differential test passes;
no P0/P1 review finding open; `CHECKPOINT-ADR-READY` restated with the post-batching
residual.
