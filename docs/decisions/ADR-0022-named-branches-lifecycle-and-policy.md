# Named branches: identity, lifecycle, policy and authorization

## Status
Accepted (2026-09-28, Plan 0008 / Phase 4). Affects persistent schema (migration 0012), the
database identity model (runtime grant re-issued), the canonical request identity (a new,
separately versioned domain; existing encodings unchanged) and the workflow repository.

## Context
Since migration 0006 a ref `(graph_id, branch)` has a head, a version that moves by exactly one
with every audited head movement (`ref_events`, `refs_movement_audited`), and an immutable
`protected` flag that selects the strict delta policy. A ref comes into existence only
through an accepted genesis change (`ref_events.operation = 'genesis'`) — in practice `main`.
Phase 4 needs durable named branches as lightweight cognitive workspaces (an agent works on
`agent/task-17` through several proposal → validation → acceptance cycles while `main` stays
put), with an audited lifecycle, authorization and policy — without merge (Phase 5).

Constraints found in the existing schema:
- `ref_events (graph_id, branch) → refs` and `decisions`/`projection_outbox → ref_events`:
  a ref with history can never be physically deleted without destroying immutable audit.
- `ref_events` is a movement log with `UNIQUE (graph_id, branch, new_version)`; lifecycle
  events (deleted, restored) do not move the head and do not fit it.
- `refs.protected` already has semantics (strict vs permissive effective delta) and is
  immutable by trigger; the runtime could not even set it (column grant).
- `ledger-admin verify` requires every ref event to have exactly one accepted decision.

## Decision

### Identity
A **branch** is the ref `(graph_id, branch)` plus one `branches` row with the same key. Names
keep the ref grammar (`[A-Za-z0-9._/-]{1,128}`); `/` is part of the name and never path
structure in the API (names travel in bodies and query parameters only). A name is unique per
graph forever: a deleted branch keeps its name, head, version and history; there is no
"recreate" — `restore` is the only way back. `main` is born only by the graph's genesis
acceptance and can never be created, deleted or re-created through branch operations.

### Creation (O(1), no RDF copied)
`create {name, source, from_commit?, policy}`:
1. the source branch must exist, be `active`, and belong to the same graph (tenant-scoped
   lookup: another tenant's graph is indistinguishable from an absent one);
2. the branch point is the source head, or `from_commit`, which must be the source head or
   **reachable from it through parent edges** — proven with the bounded `ledger-dag`
   traversal (visit limit, deadline, cycle and missing-parent detection) over this graph's
   `commit_parents`; an unknown, foreign-graph or unreachable commit fails closed with one
   stable error (`BRANCH_POINT_UNREACHABLE`) that discloses nothing about other graphs;
3. one transaction inserts the ref's first movement event (`ref_events` `genesis`,
   `new_version = 1`, `new_head` = branch point; the ref comes into existence at version 1),
   the ref (`version = 1`, `head` = branch point, `protected` from the policy), the
   `branches` row (`origin = 'created'`, source branch and commit recorded), the lifecycle
   event `created`, and the idempotency result. A concurrent create of the same name loses on
   the ref's primary key (`BRANCH_EXISTS`).
Because history only moves forward (every head movement is a direct descendant), a branch
point reachable from the source head when checked stays reachable from any later head.
The check therefore runs **before** the transaction, on one pooled connection released
before the transaction begins (no lock on the source ref, branch or graph is held while
walking; never two connections at once): an unknown or foreign `from_commit` is refused
without walking (a graph-scoped `commit_index` lookup, same error as unreachable); a walk
that exceeds its bounds (100 000 commits, 5 s) is `413 RESOURCE_LIMIT`; a missing parent or
a parent list disagreeing with `parent_count` is corruption (`CorruptObject`), never "not
reachable". The lock-free read only considers an active graph of the caller's tenant. The
transaction then locks and re-reads the source (exists, active, head and version); a point
checked against an earlier head carries over only if the source moved since **solely by
audited fast-forwards** (one contiguous, chained ref event per version from that head —
each checked by migration 0009); any other movement (a raw import move during an owner's
`importing` flip) refuses the creation for a retry. A commit
that became reachable only because the source head moved after the check is refused; the
client retries. This false negative is safe by construction — a commit is accepted as a
branch point only if it was proven reachable from an authoritative source head, and
reachability is monotone under fast-forward movement — and remains so when Phase 5 merge
commits make second-parent history reachable; holding the source lock across a
potentially long walk to remove it is rejected (it would let any `propose` caller stall
acceptance on the source). Without `from_commit`, the branch point is the head the
creating transaction reads under its share lock: if the source moves first, the branch
starts at the new head; if the creation reads first, the acceptance waits for it. Both
orders, the explicit-historical case and the false negative are forced in `pg_branches`
(`create_racing_a_source_acceptance_branches_from_an_authoritative_head`).

The ref-creation event of a created branch has no decision (nothing was accepted);
`ledger-admin verify`'s "one accepted decision per ref event" invariant exempts exactly the
version-1 events that a `created` lifecycle event names (same graph, branch, head).

Genesis acceptance on a ref that does not exist is accepted only for `main` (it creates the
`branches` row with `origin = 'genesis'` in the acceptance transaction). Every other branch
must be created explicitly; an accept or prepare on an unknown non-`main` branch is refused
(`BRANCH_NOT_FOUND`). This removes the pre-Phase-4 implicit "orphan branch by genesis".

### Lifecycle
`status ∈ {active, deleted}`, changed only by `delete` (active → deleted) and `restore`
(deleted → active). Every change increments `branches.lifecycle_version` by exactly one and
is recorded by an append-only `branch_events` row (`UNIQUE (graph_id, branch,
lifecycle_version)`, write-once trigger), enforced in the database by a deferred constraint
trigger (a status change without its event, or an event without its change, is refused) —
the same pattern as `refs`/`ref_events`. `ref_events` stays the only authority for head
movement; lifecycle events never touch it.

Deletion is a **tombstone**: ref head, version, ref events, lifecycle events, proposals,
decisions and validations are all kept. On a deleted branch:
- reads (branch status and history, ref, historical states) are allowed with read authority;
- `prepare` and `accept` are refused (`BRANCH_DELETED`), in the repository and by database
  triggers (defence in depth: a ref head of a deleted branch cannot move; a proposal cannot be
  inserted on it);
- `reject` of a pending proposal remains allowed (it closes evidence, moves nothing);
- projection enable is impossible (v1 projects `main` only, and `main` cannot be deleted).
`restore` returns the branch to `active` with the **same head and version**. Proposals
prepared before deletion stay immutable evidence; after restore they are accepted only if
every ordinary rule still holds (expected head, lineage, validation freshness, policy,
authorization) — nothing is rebased or rewritten.

Concurrency: `accept` locks the ref row (`FOR UPDATE`) and then the branch row (`FOR SHARE`);
`delete`/`restore` lock the branch row (`FOR UPDATE`) and never the ref. The lock order is
the same everywhere a ref is involved, so accept vs delete serializes without deadlock:
exactly one of "accepted on an active branch" or "deleted, acceptance refused" commits.
`prepare` takes `FOR SHARE` on the branch row as well (delete vs prepare). The database
guards on `refs` (head move) and `proposals` (insert) read the branch row `FOR SHARE` too,
so a raw write racing an uncommitted delete waits for it and is refused instead of reading
the pre-delete status (READ COMMITTED). The tests force every pair to interleave in both
orders (`pg_branches`).

A graph that leaves `bootstrap`/`importing` for `active` or `archived` (owner-only) adopts
every ref that has no `branches` row, as migration 0012 did (`graphs_adopt_refs`, principal
`urn:sculpin:ledger:graph-activation`), so imported heads can be accepted onto.

### Policy v1 (small, enforceable, immutable at creation)
| field | meaning | enforced where |
|---|---|---|
| `protected` (`refs.protected`, unchanged authority) | strict effective delta; delete/restore need admin; creation needs admin | repository (delta policy), API (capability), DB (`main` must be protected) |
| `require_validation` | acceptance must cite a conforming validation of the candidate even where the deployment allows unvalidated acceptance (development) | repository, in the acceptance transaction |
| `require_distinct_reviewer` | the accepting and proposing **parties** are distinct: the sets {principal id, on-behalf-of} of proposer and acceptor share no member (principal type does not distinguish; an agent acting for the proposer, or the proposer acting for someone else, is the same party) | repository, in the acceptance transaction |

The **deployment security floor always applies**: production refuses unvalidated acceptance
for every branch (ADR-0019); a branch policy can only add requirements, never remove them —
there is no field that weakens authentication, validation or review. Policies are immutable
after creation (no policy-change events, no mutable second source of truth); a different
policy means a new branch. `refs.protected` stays the single authority for protection: the
runtime identity gains `INSERT (protected)` on `refs` so a created branch records it, and a
CHECK makes `main` always protected. Defaults: `main` — protected, deployment floor for
validation, no distinct-reviewer rule; created branches — whatever the request asks within
the caller's authority (unprotected by default). Pre-Phase-4 refs are adopted by migration
0012 as `origin = 'adopted'`, `active`, `require_validation = false`,
`require_distinct_reviewer = false`, keeping their `protected` value. Policy is **not
inherited**: a branch created from a strict branch may be unprotected and unvalidated (it
only moves itself). Phase 5 merge must therefore enforce the **target** branch's policy.

### Authorization (existing capabilities, no per-branch ACLs)
| capability | may |
|---|---|
| `read` | list branches, read status, lifecycle and movement history |
| `propose` | create **unprotected** branches; prepare on active branches |
| `review` | accept / reject subject to branch policy |
| `admin` | create protected branches (together with `propose`); delete and restore branches |
Cross-tenant access stays non-disclosing: `404` for another tenant's graph (and so its
branches); a branch point naming another graph's commit is the same `422
BRANCH_POINT_UNREACHABLE` as an unknown or unreachable one.
There is no hidden override: `main` cannot be deleted by anyone through the API or the
runtime identity (database CHECK), and any future emergency operation is an explicit,
audited operator command.

### Idempotency and request identity
`create`, `delete` and `restore` require `Idempotency-Key` and use the existing idempotency
table (complete-actor scope) with new operations `branch_create`, `branch_delete`,
`branch_restore`, result kinds `branch_created`, `branch_deleted`, `branch_restored` and a
`result_branch_event_id` bound to the lifecycle event by a shape CHECK and FK. The request
digest is a new, separately versioned domain `sculpin-ledger-branch-request/v1` (golden
vectors, independent Python reference); no existing request, commit, validation,
state-digest or projection identity changes:
```text
"sculpin-ledger-branch-request/v1\0"
field operation  branch_create | branch_delete | branch_restore
field graph_id · field name
-- create --  field source · opt from_commit · u8 protected · u8 require_validation
              · u8 require_distinct_reviewer          (u8 = 0x00 | 0x01)
-- delete / restore --  opt reason
```
Same key + same normalized request → the original durable result; same key + different
request → `IDEMPOTENCY_CONFLICT`. An omitted `from_commit` means "the source head when first
executed"; a replay returns that original branch point. Omitted and explicit `from_commit`
are **distinct identities** even when the explicit commit is the current head (a retry must
resend what it first sent). Normalization, pinned by alias vectors: an omitted `policy` is
all-false flags; an empty or null `reason` is no reason.

### Projection
Projection v1 stays `main`-only (ADR-0020). Acceptance on any branch still writes its
`projection_outbox` row atomically (unchanged semantics; future protocols may use them), but
projection **observability counts only projection-eligible refs**: `unconfigured_pending`
and the backlog gauges consider `main` under protocol v1, so cognitive branch traffic does
not raise projection alarms. Branch creation writes no outbox row (nothing was accepted).

### History surface
Two histories, exposed separately: the **lifecycle** (created — source branch/commit and
creator —, deleted, restored, each with actor, reason, time, head/version at the event) from
`branch_events`, and the **movement** history (every head change with version, old/new head,
actor, time) from `ref_events`, plus bounded first-parent commit history via `ledger-dag`.

## Alternatives considered
- **Physical deletion of refs.** Destroys or weakens immutable history (FKs from events,
  decisions, outbox). Rejected.
- **Lifecycle events in `ref_events`.** Would need to weaken its uniqueness and movement
  semantics and its decision/outbox FKs. Rejected; `branch_events` is separate.
- **Mutable branch policy.** Needs audited policy events and makes "policy at the time of
  acceptance" a join over history; nothing in the cognitive workflow needs it yet. Deferred.
- **A second `protected` column on `branches`.** Two sources of truth. Rejected;
  `refs.protected` stays authoritative.
- **Per-branch ACLs.** Premature; the capability model covers the workflow.
- **Keeping implicit orphan branches by genesis.** Bypasses creation authority, provenance
  and policy. Rejected (only `main` is born by genesis).

## Consequences
- A branch is cheap (one ref, one row, two events); nothing is copied.
- Branch existence and status are part of every prepare/accept transaction (one extra
  indexed row lock).
- Clients that created non-`main` refs by genesis acceptance must call branch create first.
- Merge (merge-base, three-way, conflicts, merge commits) remains Phase 5; checkpoints,
  incremental projection, S3 and GC remain later phases.
