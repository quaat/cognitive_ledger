# Merge lineage: integration commits, not ref jumps

## Status
Proposed (2026-10-06, Plan 0009 / Phase 5). Decides how a merge moves a target ref under
migration 0009's movement invariant and how a merge fits the proposal → validation →
decision model. Implementation needs migration 0013 (additive). No commit, patch, state,
request or validation identity changes.

## Context
Migration 0009 (`refs_movement_is_audited`, a deferred constraint trigger) refuses every ref
movement unless a matching `ref_events` row exists **and** the new head's parent at position
0 is the old head:

```sql
EXISTS (SELECT 1 FROM commit_parents p
         WHERE p.commit_id = NEW.head AND p.position = 0 AND p.parent_id = OLD.head)
```

This is the ledger's strongest database-enforced history guarantee: no rewind, no jump,
every accepted movement lands on a direct first-parent child of the previous head. Phase 4
relies on it (reachability is monotone, so a checked branch point stays valid; ADR-0022).

The schema adds three more constraints that any merge design must respect:
- `proposals_candidate_unique (candidate_commit)` — a commit is proposed at most once,
  ledger-wide;
- `decisions_one_per_candidate (candidate_commit)` — a commit is decided at most once;
- `ledger-admin verify`: every ref event has exactly one accepted decision, and that
  decision decides the commit the event installed. The only exemption is the first event
  of a created branch (ADR-0022).

Commit v2 (ADR-0009) already permits two ordered, distinct parents. Parent 0 is the
reconstruction parent (ADR-0006). The accept path currently refuses candidates with two
parents ("before Phase 5").

A Git-style fast-forward moves the target directly to the source head:

```text
target C1        source C1 → C2 → C3 → C4        target := C4
```

0009 refuses this when the source is more than one commit ahead, because C4's first parent
is C3, not C1.

## Options

### A — step through the source's first-parent path in one transaction
Move the target C1 → C2 → C3 → C4, one audited movement per commit.
- C2, C3 and C4 were proposed and decided on the source branch.
  `decisions_one_per_candidate` forbids a second decision for them, but verify requires
  one decision per ref event. Either the uniqueness invariant or the verify invariant would
  have to be weakened.
- C2 and C3 become accepted target states (their own ref versions and outbox rows) without
  ever being validated under the **target's** policy and semantic environment. On `main`
  that contradicts the deployment floor (ADR-0019).
- One merge would consume N target versions and N outbox rows, so its idempotency result
  is no longer a single event.
- Rejected: the audit model becomes incoherent (who accepted C3 *on main*?).

### B — relax 0009 to "the new head is a first-parent descendant of the old head"
The ref jumps C1 → C4 directly, backed by a database-verifiable proof:
- a write-once `ref_event_path (event_id, step, commit_id)` table;
- a deferred trigger that checks, in one set-based (non-recursive) query bounded by the
  path length, that the path starts at the old head, ends at the new head and that each
  step's parent at position 0 is the previous step.

This keeps the guarantee database-enforced, but:
- the event installs C4, which already has its (source-branch) decision. A second decision
  is forbidden, so the merge needs a new decision kind that is not "the decision of the
  installed candidate", and verify's decision invariants must be rewritten around it;
- the target's new state is C4's state, but C4's provenance (author, activity, time) is
  the source author's. The act of integrating into the target has no commit of its own;
  only its decision row records it;
- a new table, trigger, verifier model and grant set must be added just to imitate Git's
  ref arithmetic.

Not chosen for v1. This option stays open for a later ADR if a concrete need for literal
ref equality appears (for example "target and source now point at the same commit").

### C — integration commit (chosen)
Every merge that changes the target produces a new commit:

```text
I.parents = [target_head, source_head]   (ordered, identity-bearing)
I.patch   = diff(state(target_head), merged_state)
```

- 0009 holds **unchanged**: `I.parents[0] = target_head = OLD.head`.
- I is a new candidate. It gets exactly one proposal (the merge preview) and at most one
  decision (the merge apply). One ref event installs it, with one outbox row, one
  idempotency result and one validation, all under the target's policy. Every existing
  invariant and verify check applies as-is.
- I carries its own provenance: who integrated, when, why, and with which strategy and
  validation. The source commits keep theirs.
- The two-parent shape records the ancestry. After the merge, the source head is an
  ancestor of the target head, so repeating the merge is classified *already contained*
  and creates nothing.

## Decision
Option C.

**Definition of "fast-forward" in the Cognitive Ledger.** A merge is classified
`FAST_FORWARD` when the target head is an ancestor of the source head (the target has
nothing the source lacks). It is applied as a **conflict-free integration commit** whose
resulting state is exactly the source head's state:
- the merge base is the target head;
- the target delta is empty;
- no conflict is possible;
- `I.patch = diff(state(T), state(S))`.

The target ref does **not** come to point at the source commit. Clients compare states
(state digest) or ancestry, not head ids, to learn that the target "has" the source. This
differs deliberately from Git: integrating into a branch is an audited, validated decision
of the target branch, and it is recorded as such.

Classification (ADR-0024 defines merge base and ancestry):

| class | condition | action |
|---|---|---|
| `ALREADY_EQUAL` | target head = source head | nothing; no commit, no event |
| `ALREADY_CONTAINED` | source head is an ancestor of target head | nothing; no commit, no event |
| `FAST_FORWARD` | target head is an ancestor of source head | integration commit, state = source state |
| `DIVERGENT` | neither | three-way merge from the unique merge base; integration commit, state = merged state |

**Merge as a proposal.** A merge preview is a merge *proposal* on the target branch:
- it is persisted like an ordinary prepare: an immutable candidate commit, a `proposals`
  row with `expected_head = target_head`, and a write-once merge row (migration 0013)
  holding the source branch and head, merge base, classification, strategy and preview
  token;
- the preview moves nothing, decides nothing and writes no outbox row;
- validation uses the existing Phase-2 path on the candidate;
- apply is an acceptance of that proposal with the merge-specific staleness checks
  (ADR-0024), under the **target** branch's policy.

A merge candidate may have an **empty patch**: its state equals the target state (for
example, all source changes are already present, or `take-target` resolved every
difference). It still records ancestry. The ordinary prepare's `NO_EFFECTIVE_CHANGE`
refusal does not apply to merges.

**Ref event kind.** Migration 0013 extends `ref_events.operation` with `merge` (additive
CHECK superset), so operators can distinguish integrations from ordinary advances. Every
`merge` event installs a two-parent candidate that has a merge row. Every `advance` event
installs a candidate with at most one parent. `verify` checks both directions.

**Not changed:** migration 0009 and every other migration up to 0012, commit-v2 encoding
and vectors, reconstruction (first-parent), projection (the projector sees an ordinary
accepted head on `main`), and the request domains of Phases 1–4.

## Consequences
- Every integration adds exactly one commit to the target's history, including
  fast-forward-class integrations. Target history therefore never shares head ids with the
  source. Tooling that wants "is the source fully integrated?" asks for ancestry
  (`ALREADY_CONTAINED`), not head equality.
- Merging the target back into the source after an integration is a `FAST_FORWARD`-class
  integration on the source branch (the integration commit is a descendant of the source
  head). It produces its own integration commit there.
- First-parent history of the target lists integration commits, not the source's
  individual commits. The source commits stay reachable through parent 1 (`ancestors`,
  `is_ancestor`), and their provenance is intact.
- The audit question "who put this state on `main`?" always has exactly one answer: the
  decision on the integration commit. That decision cites the validation of the merged
  state in `main`'s semantic environment.
- Option B can be added later by ADR (a new migration and a new verify model) without
  invalidating any history written under Option C.
