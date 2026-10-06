# Diff, merge base, structural conflicts, merge preview/propose and stale-safe apply

## Status
Accepted (2026-10-06, Plan 0009 / Phase 5). This revision includes the independent
pre-implementation reviews (architecture, invariant, storage/concurrency; see "Review
resolutions"). It builds on ADR-0023 (integration commits) and adds:
- two separately versioned identities, the preview token `sculpin-ledger-merge-preview/v1`
  and the request domain `sculpin-ledger-merge-request/v1`;
- migration 0013 (additive).

No existing canonical identity changes.

## Context
Phase 5 must compare and merge divergent branches deterministically. It must preserve
immutable DAG history, migration 0009, the target branch's policy, Phase-2 semantic
validation and stale-safe acceptance. Constraints:
- RDF state is a set of canonical quads (`BTreeSet<Quad>`, `sculpin-rdf-state/v1`), with
  no ordering and no blank nodes.
- The commit graph is a DAG with two-parent commits, not a tree.
- Semantic validity belongs to Sculpin (ADR-0014/0018/0019).
- The product specification requires that merge preview be side-effect free, and that apply
  re-check both heads and fail rather than apply a stale preview.

## Decision

### Ancestry and merge base (`ledger-dag`, infrastructure-free)
- `ancestors(c)` includes `c`. It follows **all** parents, is bounded by
  `TraversalLimits` (a visit limit and a **mandatory** deadline on every merge entry
  point), and fails closed on cycles and missing parents. The provider enforces same-graph
  lookup.
- **Best common ancestors** of T and S are the maximal elements of `A(T) ∩ A(S)` under the
  ancestor order. **Merge base**:
  - one best common ancestor → that commit;
  - none → `UNRELATED_HISTORIES`, refused;
  - more than one (criss-cross) → `AMBIGUOUS_MERGE_BASE`, refused. The response lists the
    candidates in ascending id order.
- **Explicit base.** The client may name `base`. It must be one of the best common
  ancestors: for an ambiguous history this resolves the ambiguity; for a unique one it must
  equal that base. Otherwise the request fails with `422 INVALID_MERGE_BASE`. The base used
  is bound in the token and the merge row, so the choice is the client's, deterministic
  and audited. The ledger never picks by iteration order, time or id.
- `ahead_behind(T, S) = (|A(S) \ A(T)|, |A(T) \ A(S)|)`.
- **Classification** uses one pair of ancestry walks (`analyze`). The checks are, in order:
  1. equal;
  2. source ∈ A(T) → contained;
  3. target ∈ A(S) → fast-forward;
  4. otherwise divergent, with the base above.

  A request with source branch = target branch is invalid (`400`).
- Walks over very long histories hit the visit limit (100 000 by default). The answer is
  then `RESOURCE_LIMIT`, never a wrong result. Generation numbers and checkpoints are
  Phase 6 work.

### State diff (`ledger-rdf`, infrastructure-free)
`diff(A, B) = { deletes: A − B, adds: B − A }` as `BTreeSet<Quad>`, which is the canonical
state byte order.
- The **structural key** of a quad is `(graph, subject, predicate)`, compared on canonical
  N-Triples terms. The default graph is `None` and orders before named graphs, which order
  by term bytes. The object is deliberately not part of the key.
- The diff also exposes the affected keys, the affected `(graph, subject)` pairs, and
  summary counts.

### Three-way structural merge (`ledger-merge`, infrastructure-free, synchronous)
Inputs are the states B (base), T (target) and S (source). For each structural key k, with
`X|k` meaning the quads of X under key k:

| condition | result |
|---|---|
| `T\|k = B\|k` or `T\|k = S\|k` | `S\|k` |
| `S\|k = B\|k` | `T\|k` |
| otherwise | **conflict** — resolved by the strategy only |

Strategies:
- `abort` (the default): no merged state;
- `take-target`: `T|k`;
- `take-source`: `S|k`;
- `union`: `T|k ∪ S|k`.

`union` is set union. It can retain a statement that one side deleted, and it never means
"semantically acceptable".

The slot key also flags multi-valued predicates (for example, two different `rdf:type`
additions on one subject) as conflicts. That is deliberately conservative; a per-quad
strategy can be added later as a new algorithm id.

The merged state is `M = ⋃_k result(k)`. The candidate patch is `diff(T, M)`, which is
exact. `reconstruct(I) = M`, because reconstruction applies the patch to parent 0's state.

The algorithm is identified as **`structural-slot/v1`**. That id is bound in the token and
recorded in the merge row, so any later explanation can recompute exactly the same
comparison.

**Conflict report.** Conflicting keys are listed in ascending key order. At most 1 000 keys
are listed in detail, each with at most 64 quads per side and a `truncated` flag per side;
the total count is always given. The report also has a byte budget and a report-level
`conflicts_truncated` flag (see "Conflict report byte budget" below).

**No-change.** The merge is classified `NO_CHANGE` and creates nothing only if **both**
hold:
- M equals T's state;
- the source contributed no net change from the base, i.e. `diff(B, S)` is empty.

This covers a fast-forward to a source with an equal state, and a source whose changes net
to nothing. It stops two-way synchronization from producing an endless chain of empty
integration commits.

If the source did change something but M still equals T, an **empty integration commit** is
recorded. Examples: a conflict resolved by `take-target`, a convergent change, or a change
the target already has. The resolution then becomes part of history, and a later merge can
never silently reapply what was set aside. This is the explicit-workflow exception of
ADR-0008, made deliberately by the merge proposer (implementation review, invariant P1).

### Three operations: preview (read), propose (persist), apply (accept)

**`POST /v1/graphs/{graph}/merges/preview`** — body `{source, target, strategy, base?}`,
`read` capability. **Side-effect free**: no transaction writes, no idempotency row, nothing
persisted.
- Reads both heads without locks, runs `analyze`, reconstructs B, T and S, and runs the
  three-way merge.
- Returns:
  - classification (`ALREADY_EQUAL`, `ALREADY_CONTAINED`, `NO_CHANGE`, `FAST_FORWARD` or
    `DIVERGENT`);
  - heads, base (or the ambiguous candidate list), and ahead/behind;
  - target-delta and source-delta summaries, plus the conflict report;
  - the merged-state digest;
  - the **preview token** when a candidate would result.
- Runs under the expensive-operation admission control. The memory budget is the
  reconstruction limits applied to every state, and M is checked against them too.

**`POST /v1/graphs/{graph}/merges/propose`** — body `{source, target, strategy, base?,
token, message?, evidence_refs?}`, `propose` capability on the target, `Idempotency-Key`
required.
- The completed result is replayed first, before anything is recomputed (the Phase-4
  lesson).
- It then recomputes exactly as preview does, before any transaction and without locks. If
  the recomputed token differs from `token`, it returns `MERGE_STALE`: something moved, so
  the client previews again.
- `ALREADY_*`, `NO_CHANGE` and `abort`-with-conflicts are refused (`409 MERGE_NOTHING_TO_DO`
  or `409 MERGE_CONFLICT`); nothing is persisted or recorded. A later request with the same
  key may therefore persist a proposal, which is intended: nothing happened the first time.
- Otherwise it runs one transaction. The transaction:
  1. checks the stored result again under the idempotency advisory lock;
  2. locks the target branch `FOR SHARE` (`BRANCH_DELETED` if it is deleted);
  3. re-reads both heads and both branch statuses; on any change it returns `MERGE_STALE`;
  4. relies on the preview for the prepare limits (`depth(T) + 1`, and the size of M), which
     are checked before the transaction; it reuses the patch computed by the preview, so
     nothing is reconstructed while the transaction holds its locks;
  5. persists:
     - the integration commit `I` (parents `[T, S]`, patch `diff(T, M)`, envelope per
       ADR-0023);
     - the proposal on the target (`expected_head = T`, `requested_patch_id =
       effective_patch_id = I.patch`, because a merge has no separately requested patch);
     - the write-once `merge_proposals` row, including `source_parties`: the principals and
       delegators who proposed the commits the source has and the target lacks.

**`POST /v1/graphs/{graph}/merges/apply`** — body `{proposal, token, validation_id?,
semantic_environment_id?, reason?}`, `review` capability, `Idempotency-Key` required. It is
a **separate path** from ordinary accept, which keeps refusing every candidate that has a
merge row or two parents. Ordinary `reject` may close a merge proposal. In one transaction:
1. Check the stored result again (replay first, as above).
2. **Lock protocol.** Both refs, in ascending branch-name order (target `FOR UPDATE`, source
   `FOR SHARE`); then both branch rows `FOR SHARE`, in the same order; then the per-proposal
   lock.
   - Every multi-ref lock is taken before any branch lock, in one total order. Ordinary
     accept and branch creation lock one ref and then its branch; delete and restore lock
     only a branch. So no wait cycle exists.
   - Two opposite merges (A into B, B into A) serialize. The second sees the moved source and
     returns `MERGE_STALE`. This prevents the write-skew that would otherwise create a
     permanent criss-cross.
3. Compare **stored values only**; nothing is reconstructed under the lock. The request's
   token must equal the merge row's token. The current target head and source head must
   equal the row's. Both branches must be active. Any difference returns `MERGE_STALE`.
   If the proposal is already decided, the error names that decision.
4. The **target** branch's policy is authoritative:
   - the deployment floor;
   - `require_validation`;
   - `require_distinct_reviewer`: the applier must be a party distinct from the merge
     proposer **and** from every party in `source_parties`, so a merge cannot carry
     self-reviewed source work into a four-eyes target (implementation review,
     semantic-integration P1).

   Protection is not an extra gate at apply. `protected` selects the strict delta policy,
   which the exact merge patch `diff(T, M)` always satisfies, and applying needs `review`,
   as accepting does on any branch.

   A weaker source policy never applies. Validation binding is the ordinary ADR-0019
   binding on the merge candidate: a conforming, unsuperseded validation of this candidate,
   whose environment is the one the reviewer names. Its `candidate_state_digest` must equal
   the merge row's digest.
5. Install `I` in one transaction: a `merge` ref event, the decision, the outbox row
   (`event_kind = ref_advanced`; projected if the target is `main`) and the idempotency
   result.

Any movement since the preview, or since the propose, returns `MERGE_STALE`. Apply never
recomputes.

### Preview token v1
```text
"sculpin-ledger-merge-preview/v1\0"
field graph_id
field source_branch · field source_head
field target_branch · field target_head
field merge_base
u8    classification   (1 fast_forward, 2 divergent)
u8    strategy         (0 abort, 1 take_target, 2 take_source, 3 union; normalized to 0
                        for fast_forward, which cannot conflict)
field merge_algorithm  ("structural-slot/v1")
field merged_state_digest
```
- `field` is a u32 BE length followed by UTF-8; `u8` values are fixed-width.
- `token = "sha256:" || hex(sha256(bytes))`.
- The token is a **confirmation digest** of what the client previewed. It is not an
  authenticator. It is recomputable on any replica, from the heads and states or from the
  persisted merge row. No correlation id, wall-clock time or replica identity is bound.
- The candidate commit id is not bound, because it does not exist at preview time. It is
  bound to the token through the merge row, which the propose transaction writes together
  with the candidate.
- Golden vectors in Rust are matched by an independent Python reference encoder.
- The **semantic environment is bound at apply**, by ADR-0019. This deliberately departs
  from the product plan §17 field list, because validation happens after the candidate
  exists. Revalidating the same candidate in a new environment requires no new preview.

Virtual A-Box: a change in a source version changes the environment id (ADR-0018). An apply
that names the new environment against a validation made in the old one is
`VALIDATION_STALE`. Until Phase 8 this gate runs against the protocol-conformant fake
validator and is labelled accordingly. Virtual A-Box triples are never persisted in the
commit.

### Idempotency and request identity
- **Operations.** `merge_propose` (result kind `merge_proposed`: proposal and candidate)
  and `merge_apply` (result kind `merge_applied`: decision, ref version and commit). Both
  are enforced by a shape CHECK.
- **Request digest.** The new domain `sculpin-ledger-merge-request/v1` encodes the
  operation, the graph and every field that reaches persistence or the decision:
  - propose: source, target, strategy, base (opt), token, message (opt), evidence (sorted
    set);
  - apply: proposal, token, validation id (opt), environment (opt), reason (opt).

  Golden vectors and the Python reference cover it.
- Preview is a read and has no idempotency. A completed propose or apply replays before
  any recomputation, and the scoped transaction remains the serialization point.

### Concurrency (forced-interleaving tests required)
Each case must have a test:
- apply vs ordinary target accept;
- apply vs source accept, in both orders;
- **opposite applies (A into B and B into A): exactly one succeeds**;
- a three-branch ring (A into B, B into C, C into A);
- two applies of one proposal across replicas;
- apply vs target delete or restore;
- propose vs head movement;
- a lost response after the apply commit.

Exactly one valid target movement commits. No merge lands on a deleted target, and no
criss-cross is created by concurrency.

### Migration 0013 (additive, database-enforced)
- **`merge_proposals`** (write-once trigger). Columns:
  - `proposal_id`, `graph_id`, `target_branch`, `candidate_commit` — a composite FK to
    `proposals_identity`;
  - `target_head`, `source_branch`, `source_head`, `merge_base`, `base_explicit`,
    `classification`, `strategy`, `merge_algorithm`, `conflict_count`,
    `merged_state_digest`, `preview_token` (indexed, **not** unique: a rejected merge may be
    proposed again from the same preview, and two proposals of one preview are allowed;
    implementation review, storage P1), `source_parties`, `created_at`.

  Further constraints:
  - FKs `(graph_id, source_head)` and `(graph_id, merge_base)` to `commit_index`, and
    `(graph_id, source_branch)` to `branches`;
  - CHECKs, written as flat ANDs: enum values; `conflict_count >= 0`; fast-forward implies
    zero conflicts and `merge_base = target_head`; source branch ≠ target branch; the
    token and digest formats;
  - a BEFORE INSERT trigger requiring that the candidate has two parents, that parent 0 is
    the proposal's `expected_head` and the row's `target_head`, and that parent 1 is
    `source_head`.
- **`ref_events`.** The `operation` CHECK and the shape CHECK are replaced:
  - `genesis`: no old head, version 1;
  - `advance` and `merge`: an old head, and `new_version = old_version + 1`.

  A BEFORE INSERT trigger requires:
  - `advance` ⇒ the new head has at most one parent and no merge row;
  - `merge` ⇒ a merge row for the new head on this branch, with `target_head = old_head`.

  `genesis` is unconstrained, because a branch created at an integration commit is a
  genesis on a two-parent commit.
- **`decisions`.** An accepted decision on a candidate with a merge row must reference a
  `merge` event (trigger).
- **`idempotency`.** The operation and result CHECKs and the shape CHECK cover
  `merge_propose` and `merge_apply`.
- **Verifier.**
  - Every new trigger, check and table is modelled in the verifier.
  - `verify` adds:
    - both directions of the merge/advance rules;
    - parent 0 of every candidate equals the proposal's `expected_head`;
    - `merge_apply` results point to `merge` events;
    - an independent offline recomputation for every merge row. It re-walks the ancestry
      to confirm the classification and the base (or an explicit choice among several best
      common ancestors), re-runs the three-way merge of the recorded base, target and
      source states, and checks that the integration commit reconstructs to exactly that
      state with the recorded digest and token. A row that cannot be checked counts as a
      violation and never aborts the run.
- **Grants.** The runtime grant is re-issued with column-level INSERT and SELECT on
  `merge_proposals`. The projector is unchanged.

## Review resolutions (2026-10-06)

| finding (reviewer) | resolution |
|---|---|
| Opposite applies both commit (write skew) → permanent criss-cross (storage P0; invariant, architecture P1) | sorted two-ref lock protocol; source re-checked under its share lock; forced test |
| Ordinary accept could install a merge candidate (all three) | separate apply path; database triggers on `ref_events` and `decisions`; ordinary accept refuses |
| `ref_events_genesis_shape` forbids `merge` rows (all three) | shape CHECK replaced in 0013 |
| Apply would reconstruct under the target lock (storage P1) | digest, heads and token stored in the merge row; apply compares stored values; verify recomputes offline |
| Two-way sync loops and empty commits contradict ADR-0008 (invariant, architecture, storage) | `NO_CHANGE` class creates nothing |
| Preview persisted state, against the spec's "side-effect free" (architecture P1) | preview / propose / apply split |
| Integration-commit envelope undecided (architecture P1) | ADR-0023 envelope; client fields in the request identity |
| No escape from criss-cross (architecture P1) | explicit `base` restricted to the best common ancestors |
| Merge semantics not versioned (invariant, architecture) | `structural-slot/v1` in the token and the row |
| Merge size and depth limits (invariant, storage) | prepare limits on M and on depth |
| Admission control and memory (storage) | expensive-operation slot; mandatory deadline; one ancestry pair per operation |
| Idempotency of outcomes that persist nothing (invariant, storage) | not recorded; stated |
| `requested_patch_id` is NOT NULL (invariant) | equals the effective patch |
| Conflict report unbounded in bytes (invariant, architecture) | 64 quads per side, with `truncated` |

## Implementation-review amendments (2026-10-06)
Seven independent reviews of the implementation (DAG, RDF semantics, storage/concurrency,
semantic validation, security, tests, projection) found no P0. Their P1s are resolved as
follows:
- **Re-proposing after a rejection** returned a 500, because the preview token was unique.
  The token is now non-unique, and every other propose collision maps to `MERGE_STALE`.
- **`NO_CHANGE` could drop a conflict resolution from history.** The new rule records empty
  integrations whenever the source changed something (above).
- **Four-eyes through a merge.** `source_parties` was added and is enforced at apply.
- **An already decided proposal looked stale.** Apply reports the terminal decision before
  any staleness check.
- **A lost propose response retried after a branch moved** looked stale. Propose re-checks
  the stored result before answering `MERGE_STALE`. Completed proposes also replay before
  the API takes any admission permit.
- **Admission.** A merge preview or propose takes 3 expensive-operation slots (never more
  than the configured total), because it rebuilds three or four states.
- **Missing forced races.** All the races listed under "Concurrency" are now forced
  interleavings in `pg_merge`, together with crash atomicity at every merge stage
  (failpoints) and raw-SQL refusals by the 0013 triggers. Every new `verify` check has a
  tampering test.
- **A target deleted and restored between preview and apply** is not stale. Its head is
  unchanged, and history only moves forward (0009), so the merge integrates exactly what was
  previewed onto an active target. This is decided; neither the token nor the merge row
  binds the branch lifecycle version.
- **The preview token is per strategy.** Even when no slot conflicts, a preview with one
  strategy does not confirm a propose with another (`MERGE_STALE`); normalizing that would
  change the v1 bytes.

Codex review of the final candidate (`60c918f`) found no P0 or P1 and two P2s, both fixed:
- **Propose replays before any refusal.** If the recomputed token differs from the
  client's token, propose looks up the stored result first and only then reports a
  classification error or `MERGE_STALE`. Previously a lost-response retry, racing the
  original propose and its apply, could get `MERGE_NOTHING_TO_DO` for a merge that had
  completed. A class with no token (conflicted, no-change, contained, ambiguous, unrelated)
  is still reported as that class, not as stale.
- **`verify` recomputes `source_parties`** from the proposals of the source-only commits.
  Erased four-eyes evidence is a violation.

## Conflict report byte budget and replay before refusal (Plan 0009 closure, 2026-10-06)
**Byte budget.** The detailed report was bounded by count only, and a legal quad can be
large: up to 1 000 × 3 × 64 large quads could be listed. Now:
- The budget is an operational setting, `LEDGER_LIMIT_MERGE_CONFLICT_REPORT_BYTES`
  (`ApiLimits::max_merge_conflict_report_bytes`). It defaults to **2 MiB**. Values outside
  1 KiB..=64 MiB, including 0, are refused at startup and never clamped.
- It is enforced while the report is collected, before any JSON is built.
  `ledger_merge::three_way_reported` takes `ReportLimits`, and a separate collector reads the
  slots the merge has already partitioned. Conflicts are offered in ascending key order and
  sides in base/target/source order. Each quad's cost is its length as an escaped JSON
  string plus a separator. Each listed conflict also pays a fixed framing charge (160 bytes,
  at least the real framing) plus its key terms. Collection stops at the first item that
  does not fit.
- The result is a deterministic prefix of whole quads; no quad or entry is cut in half. The
  serialized `conflicts` array is at most the budget plus its two brackets. A side that lost
  quads has `truncated: true`. `conflicts_truncated: true` means a count or byte limit left
  something out.
- The budget is **diagnostic only.** The merged state, `conflict_count`, the
  classification, the preview token (`sculpin-ledger-merge-preview/v1` bytes unchanged), the
  candidate and the merge row are the same under any budget. Propose collects with the
  smallest budget, because it never returns details.
- The merge computation still holds the full states; that memory is bounded by the
  reconstruction limits and admission (tech-debt).

**Replay before refusal, including recomputation errors.** Propose now consults the stored
result before returning **any** refusal, not only a token mismatch or a class error. That
includes errors raised by the recomputation itself, such as `INVALID_MERGE_BASE` for an
explicit `base` once the source is contained, or `BRANCH_DELETED`. A lost-response retry of
a completed propose therefore always replays it, and a same-key retry with another request
gets `IDEMPOTENCY_CONFLICT`. The forced-race test showed that `beece09` still returned
`INVALID_MERGE_BASE` for a retry with an explicit base.

**Deterministic pause point.** The ordering is pinned by
`pg_merge::a_propose_retry_paused_before_recomputation_replays_the_applied_original`. The
test uses a pause compiled only under the non-default `ledger-store` feature `test-hooks`,
which only that crate's test targets enable. The server, admin and projector binaries
never build it: there is no runtime switch, environment variable or endpoint.

## Alternatives considered
- **Object-level conflict key** `(graph, subject, predicate, object)`: this silently unions
  competing values of a slot. Rejected as the default; it could become a later algorithm id.
- **Choosing a criss-cross base automatically** (first found, newest, smallest id):
  arbitrary. Rejected; the client may choose explicitly instead.
- **Server-side preview sessions**: these do not work across replicas and leave no audit
  trail. Rejected in favour of a recomputable token plus a persisted proposal.
- **Recomputing at apply when heads have moved**: this applies something nobody previewed
  or validated. Rejected.
- **Locking only the target at apply**: this admits the opposite-merge write skew.
  Rejected.

## Consequences
- Merges reuse the proposal, validation, decision, projection and idempotency machinery.
  The projector has no merge-specific logic.
- Exploration (`preview`) never writes anything. Each persisted proposal is a deliberate
  `propose`, and its candidate remains immutable evidence (there is no GC in v1).
- Ambiguous and unrelated histories need an explicit base or are refused. Criss-cross can
  no longer be created by concurrent applies, but it can still arise from deliberate
  historical branching (ADR-0022).
