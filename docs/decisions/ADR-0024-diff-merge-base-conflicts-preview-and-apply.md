# Diff, merge base, structural conflicts, merge preview and stale-safe apply

## Status
Proposed (2026-10-06, Plan 0009 / Phase 5). Builds on ADR-0023 (integration commits). Adds:
- a new, separately versioned preview-token identity (`sculpin-ledger-merge-preview/v1`);
- a request-identity domain (`sculpin-ledger-merge-request/v1`);
- migration 0013 (additive).

No existing canonical identity changes.

## Context
Phase 5 must compare and merge divergent branches deterministically, without weakening
existing guarantees. Those guarantees are: immutable DAG history, the target branch's
policy, Phase-2 semantic validation, and stale-safe acceptance.

Constraints:
- RDF state is a set of canonical quads (`BTreeSet<Quad>`, `sculpin-rdf-state/v1`). There
  is no ordering and there are no blank nodes.
- The commit graph is a DAG (two-parent commits from ADR-0023), not a tree.
- Semantic validity belongs to Sculpin. The ledger only verifies the integrity and
  applicability of a validation record (ADR-0014/0018/0019).

## Decision

### Ancestry and merge base (`ledger-dag`, infrastructure-free)
- `ancestors(c)` includes `c`. Traversal follows **all** parents, and is bounded
  (`TraversalLimits`: visit limit and deadline). It fails closed on cycles and on missing
  parents. The provider enforces same-graph lookup (ADR-0022 `GraphParents`).
- **Common ancestors** of T and S: `A(T) ∩ A(S)`. **Best** common ancestors: the common
  ancestors that are not a proper ancestor of another common ancestor (the maximal
  elements).
- **Merge base**:
  - one best common ancestor → that commit;
  - none → `UNRELATED_HISTORIES`. Merge is refused in v1, because there is no implicit
    empty base;
  - more than one (criss-cross) → `AMBIGUOUS_MERGE_BASE`. Merge is refused in v1, and the
    set is reported. Choosing by iteration order, time or id is forbidden. Synthesizing a
    virtual base needs a later ADR.
- `ahead_behind(T, S) = (|A(S) \ A(T)|, |A(T) \ A(S)|)`: commits the source has that the
  target lacks, and the reverse.
- **Classification** is exactly ADR-0023's table. Tests are evaluated in order: equal,
  then source ∈ A(T) (contained), then target ∈ A(S) (fast-forward), otherwise divergent.
  The merge base is computed only for `DIVERGENT`. For `FAST_FORWARD` the base is T by
  definition.
- v1 computes both ancestor sets in memory, each bounded by the traversal limits. Deep
  histories that exceed the limits are refused with `RESOURCE_LIMIT`, never answered
  wrongly. Generation numbers and checkpoints are Phase 6.

### State diff (`ledger-rdf`, infrastructure-free)
- `diff(A, B) = { deletes: A − B, adds: B − A }`. Both are `BTreeSet<Quad>`, i.e. the
  canonical byte order of `sculpin-rdf-state/v1`, independent of hash maps, row order or
  scheduling.
- **Structural key** of a quad: `(graph, subject, predicate)`, using canonical N-Triples
  terms; the graph is the default graph or a named-graph IRI. The object is deliberately
  not part of the key.
- Diff output also exposes the affected structural keys (`BTreeSet`) and summary counts.
  It is usable independently of merge (compare, history explanation, Sculpin tooling).

### Three-way structural merge (`ledger-merge`, infrastructure-free)
Inputs: base B, target T, source S as states. Partition all quads of `B ∪ T ∪ S` by
structural key k. Write `X|k` for the quads of X with key k.
- **Unchanged-or-one-sided key**: if `T|k = B|k`, the result is `S|k`; if `S|k = B|k`, the
  result is `T|k`.
- **Convergent key**: if `T|k = S|k`, the result is `T|k` (both sides made the same change;
  this is not a conflict).
- **Conflicting key**: `T|k ≠ B|k`, `S|k ≠ B|k` and `T|k ≠ S|k`. Examples:
  - target X→Y, source X→Z;
  - target deletes the slot, source changes it;
  - both add different values to an empty slot.

  These are resolved only by the strategy:
  - `abort` — no candidate. The preview reports the conflicts. This is the default.
  - `take-target` — `T|k`.
  - `take-source` — `S|k`.
  - `union` — `T|k ∪ S|k` (RDF set union of both sides' results for that slot. This can
    keep a statement one side deleted if the other side still has it. "Union" means set
    union, never "semantically acceptable").
- **Merged state** `M = ⋃_k result(k)`. The candidate patch is `diff(T, M)`, which is exact
  because every add is absent from T and every delete is present in T.
- **Conflict report**: per conflicting key, the key and `B|k`, `T|k`, `S|k`, in key order.
  It is capped at 1 000 keys plus the total count. There is no time-, confidence-, AI- or
  ontology-based resolution; the ledger stays deterministic, and semantic adequacy is
  Sculpin's job.
- `FAST_FORWARD` is the special case B = T, so M = S and there are no conflicts.

### Merge preview (side-effect-free with respect to accepted state)
`POST /v1/graphs/{graph}/merges/preview {source, target, strategy}` (`propose` capability on
the target):
1. Read both heads, classify, and compute the merge base and states. This runs before any
   transaction and holds no lock, like Phase-4 reachability.
2. `ALREADY_EQUAL` and `ALREADY_CONTAINED` return the classification only. Nothing is
   persisted.
3. `DIVERGENT` with conflicts under `abort` returns the conflicts only. Nothing is
   persisted.
4. Otherwise one transaction runs. It re-checks both heads unlocked; if they changed, it
   returns `MERGE_STALE` and the client re-previews. It then persists the candidate commit
   `I` (parents `[T, S]`, patch `diff(T, M)`), the proposal on the target with
   `expected_head = T`, and the write-once `merge_proposals` row (source branch and head,
   base, classification, strategy, conflict count, preview token).
5. The response contains:
   - classification, heads and base;
   - ahead/behind;
   - target-delta and source-delta summaries;
   - conflicts;
   - the candidate id and its state digest;
   - the preview token.

Preview never moves a ref, never writes a decision or an outbox row, and never marks
anything accepted.

### Preview token v1
Canonical bytes, using the same field primitives as the other request domains
(`field` = u32 BE length plus UTF-8; `u8` enums):

```text
"sculpin-ledger-merge-preview/v1\0"
field graph_id
field source_branch · field source_head
field target_branch · field target_head
field merge_base
u8    classification   (1 fast_forward, 2 divergent)
u8    strategy         (0 abort, 1 take_target, 2 take_source, 3 union)
field candidate_commit · field candidate_state_digest
```

`token = "sha256:" || hex(sha256(bytes))`.

Correlation ids, wall-clock times and replica identity are never bound. The token is
recomputable from the persisted merge row and the candidate, so it works across replicas
without server session state. Rust golden vectors are matched by an independent Python
reference encoder.

The semantic environment is not inside the token. It is bound at apply time by the
existing Phase-2 rule (ADR-0019): the apply must cite a conforming, unsuperseded validation
of **this candidate** in the **current** environment. A preview validated in E1 cannot be
applied when E2 is required. The validation's environment identity and its Virtual A-Box
source versions are what change. Virtual A-Box triples are never persisted in the commit.

### Stale-safe apply
`POST /v1/graphs/{graph}/merges/apply {proposal, preview_token, validation_id?,
semantic_environment_id?, reason?}` (`review` capability, plus the target's policy). It runs
in the ordinary acceptance transaction:
1. Lock the target ref `FOR UPDATE`, then the target branch `FOR SHARE` (the Phase-4 lock
   order). Check that the target is active, and that its head equals the preview's target
   head; otherwise `MERGE_STALE`.
2. Recompute the token from the persisted merge row; it must equal the request's token
   (`MERGE_STALE`/`IDEMPOTENCY_CONFLICT`-style refusal on mismatch).
3. Read the source ref and branch **without locks**. The source head must equal the
   preview's source head and the source must be active; otherwise `MERGE_STALE`. A
   committed source movement after this read is ordered after the merge. The merge
   integrated the authoritative head of its read. Not locking the source avoids a lock
   cycle between two opposite merges.
4. The **target** branch's policy is authoritative:
   - the deployment floor (production requires validation);
   - `require_validation`;
   - `require_distinct_reviewer` (merge proposer vs applier, as accountable parties);
   - protection.

   The source branch's weaker policy never applies. Validation binding is exactly the
   ordinary accept's (ADR-0019) on the merge candidate.
5. Install `I` with a `merge` ref event (ADR-0023), the decision, the outbox row (projected
   if the target is `main`, ignored otherwise) and the idempotency result, all in one
   transaction. Migration 0009 holds because `I.parents[0] = T`.

Any movement since the preview is `MERGE_STALE`. Apply never recomputes a different merge
under an old token. The client asks for a new preview.

### Idempotency and request identity
- Preview and apply require `Idempotency-Key`. They are new idempotency operations,
  `merge_preview` and `merge_apply`, with results bound to the merge proposal and the
  decision respectively.
- The request digest is `sculpin-ledger-merge-request/v1`: operation, graph, then the
  operation's fields (source, target, strategy; or proposal, token, validation,
  environment, reason). It has golden vectors and the Python reference.
- **Completed requests replay first.** As learned in Phase 4, a stored result is looked up
  before any reconstruction, DAG walk or head check, and is replayed or refused as a
  conflict. The scoped transaction checks again under the advisory lock, which remains the
  serialization point.

### Concurrency (forced-interleaving tests required)
- apply vs ordinary acceptance on the target: target ref lock; the loser gets `MERGE_STALE`
  or `HEAD_CHANGED`;
- apply vs source acceptance: both orders are valid; either the merge integrated the old
  source head, or it is stale;
- two applies to the same target: one wins;
- apply vs target delete or restore, and preview then delete;
- the same preview applied on two replicas;
- a lost response replayed.

Exactly one valid target movement commits in every case. No merge lands on a deleted
target.

### Migration 0013 (additive)
- `merge_proposals`: write-once, keyed by `proposal_id`, with composite FKs to `proposals`
  and `commit_index`. It holds source branch and head, base, classification, strategy,
  conflict count and preview token.
- `ref_events.operation` gains `merge`.
- Idempotency operations and results for the merge operations.
- Verifier model and `verify` checks:
  - a `merge` event's candidate has two parents and a merge row whose source head is
    parent 1;
  - an `advance` event's candidate has fewer than two parents.
- Runtime grant re-issued; projector unchanged.

## Alternatives considered
- **Object-level conflict key** `(graph, subject, predicate, object)`: this finds
  conflicts only when the identical quad is both added and deleted, and silently unions
  competing values of a slot. Rejected in favour of the conservative slot key.
- **Choosing one criss-cross base** (first found, newest, smallest id): non-deterministic
  or arbitrary. Rejected; `AMBIGUOUS_MERGE_BASE`.
- **Server-side preview sessions**: these do not work across replicas and are not
  auditable. Rejected in favour of a persisted proposal plus a recomputable token.
- **Recomputing on apply when heads moved**: this applies something nobody previewed or
  validated. Rejected.

## Consequences
- Merges reuse the proposal, validation, decision, projection and idempotency machinery,
  with no merge-specific projector logic.
- A conflicting divergent merge under `abort` leaves no trace. A merge resolved by strategy
  records the strategy in the merge row and in the token.
- Ambiguous and unrelated histories are refused in v1. A virtual-base ADR can lift this
  later without changing stored identities.
