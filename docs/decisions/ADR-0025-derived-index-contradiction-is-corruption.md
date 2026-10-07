# ADR-0025: A derived-index claim that contradicts verified immutable bytes is storage corruption

## Status
Accepted (2026-10-07, Plan 0012 / Phase 6C). Records the one intended behaviour change of
windowed retrieval, raised as a P1 review finding on PR #14 (an invariant change recorded
only in the plan). It builds on ADR-0012 (immutable bytes authoritative; `commit_index` and
`commit_parents` a derived, verified index) and ADR-0013 (reconstruction inside the
acceptance transaction). No canonical identity, encoding, limit, schema or API representation
changes.

## Context
- **Immutable commit bytes are authoritative** (ADR-0002, ADR-0012). A commit is the
  canonical envelope whose SHA-256 is its id; `get_commit` decodes bytes and verifies the
  digest and never trusts the index for content. The first-parent chain that reconstruction
  folds is the chain the envelopes name.
- **`commit_index` and `commit_parents` are derived acceleration structures.** One code
  path writes them, `PostgresImmutableStore::publish_commit_in` (reached from `put_commit`,
  from `prepare` and from merge apply), in the same transaction as the object bytes, from
  the commit being published and checked against its canonical bytes before the transaction
  commits (`check_parent_rows`); the rows are write-once (migration 0005 triggers forbid
  `UPDATE` and `DELETE`; the runtime identity holds `INSERT`, which publication needs).
  ADR-0012 calls the index *verified*: it can be re-derived from the bytes at any time, and
  `verify_commit_index` is that re-derivation. The same rows already carry authority for
  other public answers: the ref-advance rule `new_head.parents[0] == expected_head` is
  evaluated from the position-0 row (the accept-time query in `postgres_workflow.rs` and the
  migration 0009 ref-movement trigger), and every DAG walk (branch history, historical
  branch points, merge base, merge classification) reads parents from `commit_parents`
  through `GraphParents`.
- **The scalar reconstruction of Phases 1–6B never consulted `commit_parents`.** It read one
  commit object, decoded it, followed `parents[0]` from the bytes, and repeated: two
  statements per ancestor. On a database whose index rows disagreed with the bytes it
  silently served the state the bytes describe, while the DAG layer and the advance
  predicate answered from the rows.
- **Phase 6C needs the index to locate bounded windows.** Fetching 256 ancestors in one
  statement means the database, not the client, must decide which rows to return; the only
  structure that can continue a first-parent chain inside a statement is `commit_parents`
  position 0. The windowed `state_at_on` therefore anchors a recursive statement at a known
  commit, follows position-0 rows as a *hint* for which rows to join, and then verifies
  every returned object exactly as before (SHA-256 against the id the previous bytes named,
  production decoder, decoded id). The decoded `parents[0]` is compared with the hinted
  next id.
- **Consequently an index/bytes disagreement can now be observed during reconstruction**,
  where the scalar path could not observe it. Something has to be decided for that case,
  and the plan's "Decision 1" decided it; this ADR records the decision where AGENTS.md
  requires it.

## Decision
```text
immutable bytes remain authoritative for ledger state and identity;

derived index rows may be used only to locate candidate retrieval rows;

after immutable bytes are hash-verified and decoded, any explicit derived-index
claim that contradicts those bytes is storage corruption;

the operation fails closed with CorruptObject rather than silently trusting
either representation.
```

Precisely, for the first-parent chain (`WorkflowRepository::state_at_on`, hence every
state read, `prepare`, merge preview/propose/apply reconstructions, validation and
projection reads and `ledger-admin verify`'s merge-row checks):

```text
decoded parents[0]   position-0 row        outcome
Some(p)              Some(h), h == p       follow p (the bytes); the hint located the row
Some(p)              Some(h), h != p       CorruptObject{id: this commit,
Some(p)              Some(malformed)         reason: "commit_parents position 0 disagrees
None (genesis)       Some(_)                 with the commit bytes"}
Some(p)              None (no row)         follow p from the bytes; the next window is
                                           anchored at p (the scalar behaviour)
None (genesis)       None                  end of chain
```

- **Contradicted hint → `CorruptObject`.** The error blames the commit whose bytes the row
  contradicts, in chain order, at the point where the chain is being established; nothing
  is folded first. The hinted id is compared as text against the decoded parent, so a
  malformed row is a contradiction and is never parsed as an id before the bytes that
  would name it are decoded.
- **Missing or silent hint → follow the immutable bytes; prior missing-object behaviour
  preserved.** A missing row is not a claim about the bytes and is not evidence that they
  are wrong. A commit whose object and index rows are all missing stays `NotFound`, as the
  scalar path answered; a commit whose object exists behind a silent index is folded from
  the bytes. (`verify_commit_index` still reports the missing row as an index defect.)
- **The index never chooses the chain.** Rows are consumed as the commit the previous bytes
  named; a row the index returned for another id is a mismatch and fails the window. Patch
  ids come from the decoded commits only; `commit_index.patch_id` is not consulted.
- **DAG walks are unchanged in authority and behaviour.** `GraphParents` read
  `commit_parents` before Phase 6C and still does; the windowed prefetch adds no new trust
  (contiguity against `parent_count` is checked with the same wording) and reports damage
  only for the anchor a walk actually asks for.
- **Scope of the comparison.** Reconstruction reads exactly one derived claim per commit,
  the position-0 row, and that is the claim it checks. `parent_count`, position-1 rows and
  `commit_index.patch_id` are not read by reconstruction and are therefore neither trusted
  nor checked there (the `pg_retrieval` matrix pins that a wrong `parent_count` or a
  position-1 row without a position-0 row still reconstructs from the bytes); the DAG layer
  checks the rows' contiguity against `parent_count` but has no bytes to compare them with,
  and `verify_commit_index` checks every column. Extending the fail-closed rule to claims
  reconstruction does not read would be a further decision, not an application of this one.
- **Sound databases are unaffected.** Rows written by `publish_commit_in` agree with the
  bytes by construction, so on a database that only ledger code paths have written the new
  rule never fires. Every identity, limit, error wording for the other error classes,
  transaction boundary and API representation is unchanged (Plan 0012 invariants; the
  `pg_retrieval` differential and corruption matrices pin this on PostgreSQL 17 and 15).

## Compatibility analysis
This is an intentional behavioural change on **pre-existing corrupt databases only**: a
database in which a `commit_parents` position-0 row names something other than the parent
its child's bytes name. No ledger code path writes such a row (publication checks it against
the bytes in the same transaction, and `UPDATE`/`DELETE` are trigger-blocked). It can arise
from: a direct `INSERT` of a position-0 row for a commit that has none — a genesis, or a
commit whose row was never written — issued with the **runtime** database credential, which
must hold `INSERT` on `commit_parents` to publish commits (the runtime identity's residual
write authority is ADR-0016's accepted residual risk; the `pg_retrieval` matrix builds its
genesis case with exactly such an `INSERT`); an owner-privileged `UPDATE` with the write-once
trigger disabled; a restore that mixed states; or storage-level corruption. For the holder
of the runtime credential this ADR changes the effect of such an `INSERT` from a silent
inconsistency (DAG answers and the advance rule from the row, state from the bytes) into a
visible refusal of that history; the same credential could already corrupt the DAG answers
and ref advances, so no new authority is created, and the defect becomes observable.

On such a database the error a reader receives can change in class and in position
(examples pinned by the `pg_retrieval` corruption matrix, where each row is the scalar
reference versus the windowed path):

```text
situation                                        old (scalar, bytes only)     new (windowed)
contradicted row at commit C, sound history       Ok(state)                    CorruptObject(C)
contradicted row at C, an older commit missing    later NotFound               earlier CorruptObject(C)
contradicted row at C, an older patch corrupt     later CorruptObject(patch)   earlier CorruptObject(C)
contradicted row at C, state over the quad limit  later ResourceLimit          earlier CorruptObject(C)
contradicted row at C = the last commit the       ResourceLimit (depth)        CorruptObject(C)
  depth limit allows
position-0 row missing at C                       Ok(state) / NotFound         unchanged
```

Over HTTP a `CorruptObject` is `500 INTERNAL` (the reason is logged with the correlation id,
not returned), where the scalar path returned `200`, `404 NOT_FOUND` or `413 RESOURCE_LIMIT`
for the same history. Every read of that history is affected: state reads, `prepare` on a
branch whose head is behind the contradicted commit, merge preview/propose/apply that
reconstructs through it, validation and projection reads, and the merge rows of
`ledger-admin verify`. Writes to other branches and graphs, and reads of histories that do
not pass through the contradicted commit, are unaffected.

Nothing about identity changes: no commit, patch or state digest, no canonical byte, no
ref, and no accepted history is altered or reinterpreted. The change is strictly from
"serve a state while the DAG layer tells a different story" to "refuse and name the
commit".

## Alternatives considered
1. **Silently follow the immutable bytes on disagreement** (keep the scalar outcome). Keeps
   old error outcomes, but serves state through known structural corruption while the ref
   advance predicate, branch history and merge base — all computed from the same rows —
   answer from the other parent. Two public answers about one history would disagree, and
   the only component that noticed would say nothing. It also contradicts ADR-0012's
   definition of the index as *verified*: a verified derived structure that is found wrong
   is a defect, not a preference. Rejected.
2. **Abandon batched index-assisted retrieval** (stay at two statements per ancestor).
   Avoids the question by never observing the index, at the measured cost Phase 6B
   identified (Plan 0011: per-statement round trips are ≈ 85 % of a deep read). The
   product specification requires reconstruction to be bounded and the roadmap requires
   measurement-driven optimisation; the measured dominant cost is exactly this. Rejected.
3. **Fall back to scalar reconstruction on disagreement.** Would double the work on the
   affected history and still serve through the corruption (alternative 1's defect with
   extra cost), while making the error path depend on a second code path that production
   never otherwise runs. Rejected.
4. **Fail closed on disagreement** (selected). Preserves the derived-index invariant
   (ADR-0012: the index must agree with the bytes), refuses to serve through known
   structural corruption, blames the exact commit in chain order, and keeps a missing row
   from being mistaken for a wrong one. The product specification's rule for derived
   checkpoints ("a corrupt checkpoint MUST be detected and ignored or quarantined; it
   cannot redefine commit state") does not transfer unchanged, because a checkpoint has its
   own digest and its corruption is self-evident and local, whereas a `commit_parents` row
   has no digest and the same rows are already trusted elsewhere; what does transfer is
   "detected, never redefines state", which both the refusal and the bytes-only fold satisfy.
   Fail closed is the one option under which the failure is visible.

## Operational consequence
**Upgrades.** A `commit_parents` position-0 row that contradicted its child's bytes was
latent before Phase 6C: the scalar reconstruction served the bytes' state and nothing
reported the row. After the upgrade every read through that commit fails with
`CorruptObject`. There is no data loss and no identity change; the history is refused, not
rewritten.

**Detection and investigation.**
- The server logs each mapped `500 INTERNAL` at `warn` with the correlation id, the error
  class and the reason `commit_parents position 0 disagrees with the commit bytes`, and the
  blamed commit id; the client receives the correlation id. The blamed commit is the one
  whose row is wrong, so the operator can inspect it directly:
  `SELECT position, parent_id FROM commit_parents WHERE commit_id = $1 ORDER BY position`,
  against the parents the canonical envelope names (the envelope is the canonical v2 line
  format of ADR-0009; the fs→pg migration and `verify_commit_index` decode it).
- `ledger-admin verify` (its SQL checks) catches a parent-row count that disagrees with
  `parent_count`, a parent in a foreign graph and an unindexed parent, and it reconstructs
  every merge candidate's base, target, source and integration states through the windowed
  path, so a contradicted row behind any merge is reported there too. It does **not**
  re-derive every row from the bytes.
- The whole-database re-derivation exists as code — `PostgresImmutableStore::
  verify_commit_index` (every indexed commit) and `verify_commits` (a scoped set) — and is
  run by the `pg_immutable_store`/`pg_graphs_migration` tests and, scoped to the imported
  commits, by `ledger-admin migrate-fs-to-pg`. **No shipped command runs it over a whole
  database; its integration into `ledger-admin verify` remains deferred** and is tracked as
  tech-debt (`docs/exec-plans/tech-debt.md`, "Phase 6C residuals"). This ADR does not add
  it: the condition has no ledger-path origin, the failure is fail-closed and names the
  commit, and a verifier feature would be unrelated work in an identity-relevant change.
- **Remediation** is an operator decision outside the ledger's write paths: the rows are
  write-once and the ledger ships no repair command. Restoring from a backup taken before
  the alteration (ADR-0017) or an owner-run correction of the row to the value the bytes
  name are the available paths; in both the bytes, not the row, are the reference.

**Why the absence of a pre-upgrade whole-database check is acceptable.** The change
affects only databases that are already corrupt in a way no ledger code path causes (the
routes above need the database credential, an owner session or storage damage), the
effect is a refused read of exactly the misdescribed history (not a wrong answer, not a
lost write), the refusal names the commit, and the pre-upgrade reference behaviour was
*worse* in the one respect that matters (serving through the inconsistency). The upgrade
note in `docs/operations/deployment.md` (Runtime limits) states the change; the
production-qualification matrix carries it as a behaviour to note before an upgrade.

## Consequences
- The hint rule above is a persistent invariant of the PostgreSQL retrieval path. Changing
  it (for example, treating a *silent* hint as a contradiction, or trusting a row to pick
  the chain) requires a superseding ADR.
- Tests: the `pg_retrieval` corruption matrix pins every row of the table above on
  PostgreSQL 17 and 15 across ten window configurations (a contradicted row on a sound
  history, a malformed row, a parent for a genesis, a contradicted row at the depth limit,
  a contradicted row ahead of a missing older commit, a contradicted row ahead of a corrupt
  older patch, and the silent-row case), each against the scalar reference kept under
  `test-hooks`.
- Documentation carrying the rule: `ARCHITECTURE.md` (Phase 6C sentence), Plan 0012
  (Decision 1), `docs/operations/deployment.md` (Runtime limits),
  `docs/quality/production-qualification.md`, `docs/exec-plans/tech-debt.md` (the deferred
  verifier integration).
