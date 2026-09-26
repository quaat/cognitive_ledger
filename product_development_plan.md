# Sculpin Cognitive Ledger — Updated Production Development Plan

> **Terminology note (P0 sign-off, 2026-09-26).** Where this plan says "Jena", read
> "the Sculpin semantic validation/reasoning layer". Sculpin currently validates with
> pySHACL and Python reasoning workers; Jena is a possible implementation detail, not an
> architectural dependency of the ledger (ADR-0014). Where §3.1 lists `correlation_id`
> in the commit envelope, ADR-0009 has since removed it (tracing metadata lives on
> proposal/ref-event/decision records) and canonicalizes `evidence_refs[]` as a sorted,
> unique set. Where §6 implies one ledger graph per KB, ADR-0010 allows many graphs per
> KB. The ADRs are authoritative where they and this plan differ.

## 1. Updated architectural assessment

The Cognitive Ledger should continue as an **independent, narrowly scoped knowledge-evolution service**. Its role is not to become Sculpin’s RDF database. It should own immutable cognitive changes, historical state, proposals, branches, provenance, decisions and merge coordination. Sculpin/Jena should continue to own ontology semantics, reasoning and SHACL; Fuseki should remain the query-oriented semantic store; the Virtual A-Box should continue to provide transient external context. This separation is already explicit in the architecture specification.

The implementation is already a credible walking skeleton rather than a disposable prototype. In the uploaded source:

- `ledger-core` provides strict SHA-256 content identifiers, versioned canonical commit bytes, ordered zero/one/two-parent commits and infrastructure-free storage interfaces.
- `ledger-rdf` parses standards-based N-Quads, rejects persistent blank nodes, normalizes operations, rejects add/delete contradictions and produces deterministic patch bytes.
- `ledger-store` provides content-verified immutable filesystem objects, durable file publication, state reconstruction, filesystem CAS and a PostgreSQL-backed CAS `RefStore`. *(Status 2026-09-26: it now also provides the shared `PostgresImmutableStore` with a verified commit index, the ADR-0010 graph authority schema, and the filesystem→PostgreSQL migration — Plan 0004 P1.2.)*
- `ledger-api` exposes a deliberately minimal HTTP surface with no query language.
- `ledger-server` composes filesystem immutable storage with PostgreSQL ref coordination. *(Status 2026-09-26: with a database URL it defaults to PostgreSQL for both refs and immutable content; filesystem content is an explicit single-host opt-in.)*
- Golden fixtures pin commit and patch identities.
- Integration tests exercise concurrent PostgreSQL CAS and container restart reconstruction.
- The repository already contains ADR discipline, execution plans, independent-review agents, CI separation and architecture checks.

This is the right foundation to keep.

However, it is **not yet a production service**, and the next development steps need to address architecture risks before feature breadth.

---

# 2. Scope decision: version the cognitive overlay first

The first production integration with Sculpin should **not attempt to make the Cognitive Ledger authoritative for every RDF statement currently held by a Sculpin knowledge base**.

Use it initially for the **evolving cognitive overlay**:

```text
Sculpin semantic state
    =
persistent KB / ontology
    +
accepted Cognitive Ledger overlay
    +
transient Virtual A-Box
    +
derived reasoning
```

The authority model should be:

```text
Base KB / ontology
    current Sculpin/Fuseki mechanisms

Cognitive assertions and corrections
    Cognitive Ledger

External structured values
    external data + Virtual A-Box

Inference
    Jena

Queryable accepted state
    Fuseki projections / composed views
```

This keeps adoption incremental and avoids forcing existing Sculpin ingestion, ontology-management and Data Sources functionality through the new ledger.

A later project may decide that all mutable A-box knowledge should become ledger-controlled, but that should not be a prerequisite for cognitive agents.

---

# 3. Immediate architecture corrections before further features

## 3.1 Freeze the real production commit protocol

The repository correctly recognizes that commit `v1` is deliberately minimal and that richer provenance requires a new versioned envelope. The attached architecture also makes provenance, actor identity, validation context and evidence first-class concerns.

Do **not release the current commit v1 as the long-term external protocol** if no persistent production history exists yet.

Create an ADR for `sculpin-cognitive-commit/v2` now.

Recommended identity-bearing fields:

```text
format
ledger_id
parents[]
patch_id

actor:
    principal_id
    principal_type
    optional on_behalf_of

activity
event_time
recorded_at

evidence_refs[]
source_system
correlation_id

message
```

Do **not** put branch name into the commit.

Do **not** put SHACL results, ontology versions or Virtual A-Box snapshots into the commit itself. Those belong to immutable validation records because the same candidate commit may legitimately be revalidated later against a different semantic context.

Do not include raw JWTs, access tokens, arbitrary personal information, or full feedback/conversation text in commit metadata. Store durable references to evidence instead.

Retain the current deterministic binary representation if preferred; there is no need to change back to JCS merely because the original draft proposed JSON. The important requirement is a frozen, versioned, test-vector-backed canonical encoding.

### Gate

Before proceeding:

```text
same v2 logical commit
→ same canonical bytes
→ same CommitId
```

must hold across builds/platforms, with checked-in golden vectors.

`v1` must remain readable forever once released, even if it is only used by development fixtures.

---

## 3.2 Settle patch applicability semantics

There is currently an inconsistency that should be resolved deliberately.

The repository product specification treats patch application as set semantics: deleting an absent quad and adding an existing quad are harmless idempotent operations. The earlier architecture specification describes Level-1 validation as checking whether deletions are applicable to the base state.

Before building cognitive workflows, define the production rule.

Recommended behavior:

```text
request patch
    ↓
resolve expected base state
    ↓
calculate effective delta
    ↓
validate request against base
    ↓
persist effective canonical patch
```

For protected accepted branches:

- deleting a quad that does not exist should normally produce a typed stale/base mismatch;
- adding a quad already present should normally collapse to no change;
- a transaction whose effective change set is empty should not produce a meaningless accepted commit unless explicitly requested for an auditable workflow event;
- the requested intent may be retained in a proposal/decision record without polluting RDF evolution.

This prevents commits from claiming semantic changes that did not actually modify the graph and makes history/diff/blame easier to interpret.

Document the chosen rule in an ADR and property-test it.

---

## 3.3 Replace client-supplied `author`

The current API accepts:

```text
author: String
```

from the request body.

That is appropriate only for the bootstrap.

Production must derive actor provenance from authenticated context. The architecture already requires that author identity not be an arbitrary caller string.

Implement an authentication boundary:

```text
OIDC / trusted workload identity
        ↓
AuthenticatedPrincipal
        ↓
ledger transaction/proposal
```

Normalize the identity stored in the commit:

```text
principal_id
principal_type = human | agent | service
tenant_id
optional on_behalf_of
```

The HTTP body must not be able to override it.

---

## 3.4 Validate temporal and metadata fields

Current `event_time` is an arbitrary string.

Before protocol freeze:

- use a parsed RFC 3339/date-time type;
- define whether event time is optional;
- normalize serialized timestamps;
- define precision;
- cap message length;
- cap evidence count and identifier length;
- reject malformed Unicode/IRIs early.

`recorded_at` remains server-controlled.

---

# 4. Fix the production persistence boundary

This is the most important implementation-level architecture issue.

> **Historical (resolved in Plan 0004 P1.2, 2026-09-26).** The gap described in this section is closed: `PostgresImmutableStore` ships as the default shared backend, and the filesystem backend is single-host only. The text below is kept as the rationale for ADR-0012.

At the time of writing, PostgreSQL made mutable HEAD coordination safe between processes, but immutable commits and patches remained on the ledger container’s local filesystem.

That means two replicas sharing PostgreSQL do **not** yet form a valid horizontally scaled service:

```text
Replica A
    writes commit C42 to local filesystem
    CAS main -> C42 in shared PostgreSQL

Replica B
    reads main = C42
    but does not possess C42 locally
```

This must be resolved **before multi-replica production deployment**.

## Recommended storage abstraction

Refactor `Ledger` so it no longer concretely contains:

```rust
Arc<FileStore>
```

for immutable content.

Introduce a production-ready abstraction approximately equivalent to:

```text
ImmutableStore
    put_content(...)
    get_content(...)
    put_commit(...)
    get_commit(...)
    exists(...)
```

Implement:

```text
FilesystemImmutableStore
PostgresImmutableStore
S3ImmutableStore
```

The best first production default should be evaluated explicitly.

### Recommended practical choice

Use PostgreSQL for relatively small immutable commit/patch objects initially, while retaining S3-compatible storage for large checkpoints and later large immutable artifacts.

Advantages:

```text
single HA persistence system for refs + commit metadata
simpler backup/restore
simpler multi-replica correctness
transactionally indexable content
fewer distributed failure modes
```

Keep the object-store interface so S3-backed immutable commits remain possible if scale measurements justify it.

An alternative is S3/MinIO for all immutable objects. If selected, it must be shared by every replica and its consistency/durability guarantees must become part of deployment qualification.

Do not claim horizontal correctness while PostgreSQL heads reference node-local immutable files. *(Enforced since P1.2 by the server default and the two-replica integration evidence.)*

---

# 5. Replace the simple RefStore with an atomic acceptance repository

The current pure CAS `RefStore` was a good bootstrap abstraction.

Production acceptance needs more than:

```text
head := new_head
```

A successful accepted-state transition must eventually atomically record:

```text
ref movement
ref event
accept/reject decision
idempotency result
projection outbox event
possibly branch policy/version used
```

The attached architecture already requires ref events and durable projection behavior. 

Introduce a PostgreSQL application repository such as:

```text
RefRepository / AcceptanceRepository

advance_ref(
    graph,
    branch,
    expected_head,
    new_head,
    actor,
    operation,
    decision,
    idempotency_key
)
```

executed in one SQL transaction.

That transaction should:

```text
1. verify/CAS expected ref
2. update ref
3. append immutable ref_event
4. append decision/acceptance record
5. insert projection_outbox event
6. store idempotency result
7. commit
```

No message queue acknowledgement should occur before this transaction commits.

The immutable candidate commit may have been written earlier.

---

# 6. Add first-class graph/ledger metadata

The current HTTP/runtime effectively operates one logical graph and one `main`.

Before branching, implement the real ownership model.

Recommended entities:

```text
LedgerGraph
    id
    tenant_id
    knowledge_base_id
    purpose
    created_at
    status

Ref
    graph_id
    name
    head
    ref_type
    protection_policy
    version

Projection
    graph_id
    ref
    target
    projection_head
```

A Cognitive Ledger graph should normally map to one Sculpin KB cognitive overlay.

Graph identity must be part of authorization and preferably part of commit v2 identity.

---

# 7. Introduce proposals as a first-class concept

Branches are useful, but **not every piece of feedback needs a branch**.

Creating a branch for every single user correction will create unnecessary ref churn.

Use two concepts:

```text
Proposal
    candidate immutable commit
    proposed against expected head
    evidence
    validation records
    decision

Branch
    mutable ref for a multi-step line of cognitive work
```

A one-shot feedback event can be:

```text
main C100
   │
   └── candidate C101
           ↓
       validation
           ↓
       accept/reject
```

without creating `feedback/123`.

A reasoning agent performing several related changes may use:

```text
agent/task-17
    C101 → C102 → C103
```

This gives branches genuine cognitive meaning rather than turning them into temporary transaction IDs.

Rejected candidate commits remain immutable/auditable according to retention policy even though no accepted ref reaches them.

---

# 8. Use a two-phase semantic validation protocol

The ledger should coordinate semantic acceptance but should **not embed Jena or Sculpin domain logic**.

I recommend refining the current transaction pipeline into two phases.

## Phase A — prepare candidate

```text
POST proposal / transaction prepare

authenticate
resolve graph/ref
verify expected HEAD
parse + normalize RDF
validate patch against base
persist patch
persist candidate commit
return candidate identity
```

No accepted ref moves.

## Phase B — semantic validation and acceptance

Sculpin obtains the candidate state and constructs:

```text
base Sculpin KB
+
candidate cognitive overlay
+
ontology
+
SHACL shapes
+
reasoning profile
+
temporary Virtual A-Box
```

Jena performs:

```text
SHACL
OWL/RDFS
domain rules
external-data checks
```

Sculpin returns an immutable `ValidationRecord`.

Then:

```text
accept(candidate, validation_id, expected_head)
```

causes the atomic PostgreSQL acceptance transaction.

This avoids a hard runtime dependency:

```text
ledger → Sculpin → ledger
```

and leaves Sculpin in control of semantic composition.

A convenience endpoint may later orchestrate the full workflow synchronously, but the underlying protocol should remain separable.

---

# 9. Define SemanticExecutionContext before implementing validation

This is the integration contract that connects the ledger to Sculpin, Fuseki and the Virtual A-Box.

The architecture already requires external context to be identified by source/version/query information without persisting the transient A-box itself.

Define something like:

```text
SemanticExecutionContext

candidate_commit
candidate_state_digest

base_kb:
    kb_id
    revision_or_digest

ontology:
    id
    version_or_digest

shapes:
    id
    version_or_digest

reasoning:
    profile
    implementation/version

virtual_contexts[]:
    dataset_id
    source_version
    object/version IDs
    query_spec_digest
    hydration_plan_digest

validator:
    service_version
    configuration_version
```

This should be content-addressable or have a deterministic digest.

A `ValidationRecord` references this context.

This gives Sculpin reproducibility:

```text
Why was C101 accepted?

→ exact candidate
→ exact ontology
→ exact shapes
→ exact base graph state
→ exact external dataset versions
→ exact validator configuration
```

Do not require external A-box triples themselves to be committed.

---

# 10. Add a revision concept to the existing Sculpin KB

The Cognitive Ledger can precisely identify its own state, and Data Sources can identify external source versions.

The remaining reproducibility problem is:

```text
Which version of the ordinary Sculpin KB was used?
```

Before production semantic validation, Sculpin needs a stable:

```text
kb_revision
```

or:

```text
base_graph_digest
```

that can be placed in `SemanticExecutionContext`.

This does not require migrating the entire Fuseki KB into the Cognitive Ledger.

It only requires that a validation event can identify the base semantic state against which the candidate was evaluated.

---

# 11. Build the Sculpin validation adapter

Only after the above contracts are frozen should `ledger-validation` become a real crate.

It should contain protocol models and coordination—not Jena.

Recommended dependency direction:

```text
ledger-core
ledger-rdf
ledger-dag
       ↑
ledger-validation-protocol

Sculpin integration adapter
       ↓ HTTP/internal API
Sculpin semantic validator
       ↓
Jena/Fuseki/Virtual A-Box
```

Test cases must include:

```text
valid SHACL candidate
invalid SHACL candidate
reasoning-derived violation
virtual A-box-dependent success
virtual A-box-dependent rejection
external source changed after validation
ontology changed after validation
validator unavailable
```

Validation results must never be inserted into already hashed commit bytes.

This follows the current specification’s correct separation of validation records from commits.

---

# 12. Implement accepted cognitive projection separately

Only the accepted cognitive branch should normally be projected into Sculpin’s query environment.

Use a dedicated named graph, for example conceptually:

```text
urn:sculpin:kb:<kb-id>:cognitive
```

Do not mix it invisibly into the existing base A-box storage.

The effective semantic view becomes:

```text
base KB named graphs
+
accepted cognitive named graph
+
transient Virtual A-Box
```

Candidate branches should be materialized transiently for validation rather than projected globally.

## Projection protocol

Ref advancement transaction:

```text
main C100 -> C101
+
outbox event
```

Projector:

```text
read event
load patch/state
apply to dedicated Fuseki graph
atomically update projection marker to C101
acknowledge outbox
```

The Fuseki side should contain a projection marker or equivalent that identifies the exact commit represented.

On retry:

```text
if marker == C101
    operation is already complete
```

If state is ambiguous, rebuild projection from ledger state rather than guessing.

Projection lag is acceptable and observable:

```text
ledger main       C105
Fuseki projection C103
```

Ledger history remains authoritative.

---

# 13. Only now implement general branches

Once storage, provenance, auth, validation and ref events are correct, branches become relatively straightforward.

Create `ledger-dag`.

Required behavior:

```text
create branch from HEAD
create branch from reachable historical commit
read HEAD
list refs
delete ref
restore ref if policy permits
branch protection policy
immutable ref events
```

Adopt the useful Fluree rule that historical branch points should be reachable from the selected source history unless a privileged explicit operation says otherwise.

Branch creation copies no graph content.

Branch policies should specify:

```text
required validation level
direct commits permitted?
review required?
who may accept?
who may delete?
retention policy
```

`main` should default to protected.

---

# 14. Implement DAG algorithms as pure core logic

`ledger-dag` should remain infrastructure-free.

Implement and property-test:

```text
is_ancestor(a, b)
ancestors(commit)
merge_base(source, target)
first_parent_history(commit)
reachable_from(ref)
ahead_behind(source, target)
```

Every traversal must have:

```text
visited set
node limit
depth/work limit
clear typed error
```

Random DAG generation with deterministic seeds should be mandatory.

Do not use wall-clock timestamps to infer ancestry.

---

# 15. Implement diff independently of merge

Before implementing merge, produce reliable:

```text
diff(A, B)
```

at quad-set level.

The API should support:

```text
adds
deletes
affected subjects
affected (graph, subject, predicate) keys
summary counts
```

Internally start with reconstructed set differences.

Do not optimize prematurely.

Add a `change_index` later when measurements show historical lookup/diff requires it.

---

# 16. Implement merge preview before merge apply

The current architecture is correct to specify a two-stage merge.

Implement:

```text
ancestor A
target T
source S

delta_target = A → T
delta_source = A → S
```

Classify first:

```text
source == target
already contained
fast forward
divergent
```

For divergent state:

```text
calculate structural overlaps
apply requested structural strategy
create candidate merged state
calculate patch T → M
create candidate merge commit parents=[T,S]
run semantic validation
```

Recommended strategies:

```text
abort
take-target
take-source
union
```

`abort` should be the protected-branch default.

`union` only means RDF set union; it does not mean semantically acceptable.

---

# 17. Make merge preview tokens immutable and stale-safe

`merge/preview` should produce a stored or cryptographically bound preview identity covering:

```text
source_head
target_head
merge_base
strategy/resolutions
candidate merge commit
validation record
semantic execution context
```

`merge/apply` must re-read:

```text
source HEAD
target HEAD
branch policy
```

If either HEAD has moved:

```text
MERGE_STALE
```

Do not silently recompute during apply.

If the ontology, shapes, relevant base KB revision or required external source version changed and branch policy requires current validation, require revalidation.

This is especially important for Virtual A-Box-dependent decisions.

---

# 18. Preserve first-parent merge state semantics

The current commit format already supports this correctly.

A merge commit:

```text
parents = [target, source]
patch   = target_state → merged_state
```

is reconstructed through parent zero plus merge patch.

Parent one records ancestry rather than requiring graph-state replay through both branches.

Keep this model.

It maps well to both Git reasoning and current Fluree first-parent behavior while keeping state reconstruction deterministic.

---

# 19. Do not implement rebase as history rewriting

Retain the current non-goal.

If cognitive work needs to adapt to a newer accepted state, implement:

```text
replay proposal
```

or:

```text
re-evaluate proposal against new base
```

as a new lineage.

Never destroy the original proposal history.

---

# 20. Introduce explicit decision provenance

Validation and decision are different.

A candidate can be semantically valid and still be rejected by a human reviewer.

Add:

```text
DecisionRecord
    candidate
    branch
    decision = accepted | rejected | superseded
    principal
    reason/ref
    validation_ids[]
    decided_at
```

Acceptance creates the ref movement.

Rejection does not.

This is important for cognitive-agent explainability:

```text
"What did the agent propose?"
"Was it valid?"
"Who accepted/rejected it?"
"Why?"
```

must remain separate questions.

---

# 21. Complete idempotency before exposing agent writes

Agents will retry.

Implement `Idempotency-Key` before Sculpin agents receive write access.

Scope it by at least:

```text
tenant
actor
graph
operation
idempotency key
```

Persist request digest + final response.

Same key + same payload:

```text
return original result
```

Same key + different payload:

```text
IDEMPOTENCY_CONFLICT
```

This is especially important because server-controlled `recorded_at` means blindly retrying candidate construction otherwise produces a new commit identity.

---

# 22. Add production request/resource limits early

Do not postpone these until the final hardening pass.

Before untrusted agents can call the ledger, enforce:

```text
HTTP body bytes
patch bytes
operation count
quad/term length
message bytes
evidence count
branch name length
state export bytes/quads
reconstruction depth
ancestry work
merge work
validation report bytes
concurrent operations
deadline
```

Current `state_at` performs unbounded history reconstruction into an in-memory set. That is suitable for the walking skeleton but must not remain an unbounded public path.

Provide streaming state export where practical.

---

# 23. Keep blank nodes rejected for the first production release

Do not add skolemization merely to claim fuller RDF support.

The current rule:

```text
persistent Cognitive Ledger RDF has no blank nodes
```

is easy to reason about and fits cognitive entities that should normally have durable identity.

Add deterministic skolemization later only when an actual Sculpin workflow requires it.

Virtual A-Box graphs may continue to contain transient blank nodes because they are not ledger state.

---

# 24. Build checkpoints after real reconstruction measurements

Checkpoints are needed, but not before baseline data exists.

Implement:

```text
Checkpoint
    commit_id
    canonical dataset
    state_digest
    format_version
    compression
```

Recommended:

```text
canonical N-Quads
Zstandard
content hash
```

A checkpoint must always be discardable.

On load:

```text
verify digest
if corrupt → quarantine/ignore
reconstruct from authoritative patches
```

Benchmark first, then choose thresholds based on:

```text
commit depth
patch bytes
state size
reconstruction p95
```

rather than freezing `100 commits / 64 MiB` as protocol.

---

# 25. Add state digests as derived metadata

A state digest is extremely useful for:

```text
validation identity
projection verification
Fluree comparison
checkpoint integrity
rebuild tests
debugging
```

Do not initially place it in commit identity if computing it is expensive.

Store it as derived metadata when a state has been materialized/validated/checkpointed.

Later investigate an incremental Merkle/set root only if measurements justify it.

---

# 26. Use Fluree as a bounded semantic reference, not an oracle

The original rationale remains correct: Fluree should be architectural prior art, not a runtime dependency.

The current Fluree implementation is especially useful as a reference for:

```text
branch isolation
historical branching
content-addressed commit heads
CAS head advancement
common ancestor
fast-forward
already-contained merges
divergent merge
first-parent history
structural conflict comparison
merge preview
stale merge prevention
```

Current Fluree also validates the staged merge result before commit, which supports the Sculpin design of evaluating semantic validity after structural composition.

Do **not** copy:

```text
Fluree query engine
index/novelty implementation
Raft
policy engine
storage formats
transaction numbering
reasoning engine
SHACL engine
```

Recent Fluree merge fixes are particularly instructive: projection/index roots and transaction ordering caused subtle merge bugs. The Cognitive Ledger should avoid that entire class by ensuring that **ref acceptance is independent of Fuseki projection state**.

---

# 27. Complete the differential harness

The current repository only checks the deterministic comparison seam because live Fluree execution is intentionally disabled pending license approval.

Once approved for CI/test usage:

```text
CognitiveLedgerAdapter
FlureeAdapter
```

should execute the same deterministic scenarios.

Compare:

```text
canonical semantic RDF state
not commit IDs
not transaction numbers
not storage bytes
```

Mandatory scenarios:

```text
genesis
linear additions
delete
replace
historical state
branch isolation
historical branch
fast-forward
already-contained
divergent independent changes
same-key structural collision
delete-vs-modify
same-result merge
multiple named graphs
repeated merge
deep DAG
multiple merge ancestry
```

Classify every mismatch:

```text
ledger defect
reference semantic difference
intentional Sculpin divergence
test defect
```

An unclassified mismatch blocks the relevant feature.

Fluree should remain optional for normal runtime and normal unit testing.

---

# 28. Add a non-Fluree reference model

Production correctness should not depend on a BUSL-licensed container being available.

Create a deliberately slow, obviously correct in-memory reference implementation in `ledger-testkit` for:

```text
commit DAG
state reconstruction
branch refs
ancestor calculation
diff
three-way set merge
```

Use it for property-based tests.

Fluree then becomes a second independent external reference.

This combination is stronger than either alone.

---

# 29. Security milestone

Before external production use:

```text
OIDC validation
service/workload identities
tenant isolation
graph-level authorization
protected branch roles
no caller-supplied author
secret-free logs
rate limits
request limits
audit trail
secure production PostgreSQL TLS
secure immutable-store credentials
container hardening
dependency review
cargo-deny
cargo-audit
SBOM
OCI scan
fuzzing
```

Fuzz at least:

```text
ContentId parser
commit decoder
N-Quads ingestion
patch decoder
HTTP mutation models
branch/ref names
merge preview inputs
validation-record inputs
```

The ledger must fail closed on unknown protocol versions.

---

# 30. Privacy and retention design

Resolve this before cognitive agents begin recording human feedback at scale.

Immutable history and personal-data deletion can conflict.

Recommended policy:

```text
commit contains minimum durable semantic/audit metadata
raw conversation/evidence stays in governed external evidence store
commit stores evidence references
```

Define:

```text
retention classes
proposal retention
rejected-proposal retention
tenant deletion
evidence deletion
legal/audit preservation
```

If highly sensitive personal facts can enter ledger content, evaluate per-ledger encryption and crypto-erasure before production.

Do not make arbitrary chat text permanently immutable by default.

---

# 31. Backup, restore and disaster recovery

Production readiness requires proving more than container restart.

Define backups for:

```text
PostgreSQL metadata
immutable content
checkpoints
configuration
encryption/key dependencies
```

Restore tests must prove:

```text
all protected refs resolve
every reachable commit verifies
all patches verify
historical reconstruction matches state digests
projection can be rebuilt from zero
```

Perform a full clean-environment restore in CI/release qualification.

---

# 32. Schema and protocol migration discipline

Keep the current strong ADR discipline.

Before each release:

```text
old database schema → new binary
old commit v1 → readable
old commit v2 → readable
golden IDs unchanged
projection rebuild works
backup restore works
```

Released SQL migrations must never be edited.

New persistent canonicalization requires:

```text
new protocol identifier
new ADR
new golden vectors
dual-read strategy
compatibility tests
```

---

# 33. Observability and operational model

Expose:

```text
/health
/ready
/metrics
```

Health should distinguish:

```text
process healthy
database healthy
immutable store healthy
semantic validator available
projection healthy
```

A validator or Fuseki outage should not necessarily make the ledger process unhealthy.

Key metrics:

```text
proposal rate
accept/reject rate
commit latency
CAS conflicts
validation latency
validation rejection reasons
merge preview/apply latency
reconstruction depth/latency
checkpoint latency
projection lag
projection retries/failures
object-store latency/errors
orphan object count
branch count
state size
```

Propagate OpenTelemetry correlation through:

```text
Sculpin agent request
→ Cognitive Ledger proposal
→ Sculpin semantic validation
→ Data Sources / Virtual A-Box
→ decision
→ projection
```

---

# 34. Performance programme

Do not optimize against Fluree performance.

Establish Cognitive Ledger baselines for actual expected cognitive workloads.

Benchmark dimensions:

```text
state size:
    1k / 100k / 1M+ quads

history:
    100 / 1k / 10k / 100k commits

patch:
    1 / 10 / 100 / 1k / 10k operations

branches:
    1 / 10 / 100 / 1k

writers:
    1 / 8 / 32 / 64

DAG:
    linear
    wide
    deep
    merge-heavy
```

Measure:

```text
p50/p95/p99
throughput
CPU
RSS
PostgreSQL bytes
immutable-store bytes
checkpoint bytes
reconstruction depth
projection lag
```

Then set regression thresholds.

Fluree timings remain diagnostic.

---

# 35. Fault-injection programme

Retain and expand the excellent failure-boundary concept from the architecture.

Inject faults:

```text
during immutable object put
after object put before DB registration
before acceptance transaction
during ref CAS
after ref update before transaction commit
after acceptance commit before outbox consumption
during Fuseki projection
after Fuseki update before projector ack
during validation
during checkpoint write
during object read
during PostgreSQL restart
```

After every scenario verify:

```text
no ref points to missing content
no acknowledged decision disappears
no partial acceptance
retry is safe
outbox delivery is idempotent
projection is rebuildable
checkpoint corruption cannot redefine state
```

---

# 36. Updated development sequence

## Phase 0 — Reconcile specification and repository

**Goal:** remove ambiguity before more persistent behavior is added.

Deliver:

```text
updated product specification
updated architecture
clean tech-debt list
ADR: commit v2
ADR: patch applicability/effective-delta semantics
ADR: graph/tenant identity
ADR: validation protocol
ADR: production immutable storage
ADR: atomic acceptance/ref-event/outbox transaction
```

Also fix currently stale documentation: parts of the repository still describe PostgreSQL CAS as future work even though Plan 0002 implemented it.

Gate:

```text
documentation consistency
golden protocol review
independent architecture/invariant reviews
```

---

## Phase 1 — Production persistence and API foundation

Deliver:

```text
multi-graph model
commit v2 dual read
authenticated principal model
RFC3339 temporal validation
production immutable-store abstraction
shared production store
atomic RefRepository
ref_events
idempotency
resource limits
OpenAPI
stable error taxonomy
```

Do not add merge yet.

Gate:

```text
multi-process/restart correctness
no client actor spoofing
idempotent retries
no node-local content references from shared refs
```

---

## Phase 2 — Proposal and semantic-validation protocol

Deliver:

```text
ProposalRecord
DecisionRecord
prepare candidate
state export/stream
SemanticExecutionContext
ValidationRecord
validator authentication
accept/reject API
branch validation policies
```

Implement the Sculpin semantic-validator adapter.

Test with:

```text
Jena SHACL
reasoning
Virtual A-Box
source-version pinning
base-KB revision pinning
```

Gate:

the end-to-end validation flow specified by the architecture must pass.

---

## Phase 3 — Fuseki cognitive projection

Deliver:

```text
projection_outbox
projector
dedicated cognitive named graph
projection commit marker
retry/idempotency
lag metrics
full rebuild
projection verification
```

Gate:

```text
accepted ledger state == rebuilt projection state
duplicate events harmless
projector restart harmless
Fuseki outage never corrupts ledger
```

---

## Phase 4 — Branches and cognitive workflows

Deliver:

```text
named refs
branch from HEAD
branch from history
branch deletion
protected branches
branch policies
ref event history
multi-step proposal branches
review workflow
```

Add first live Fluree differential suite if legal approval is complete.

Gate:

```text
branch isolation
100-way independent branch stress
auth/policy tests
historical branch reconstruction
```

---

## Phase 5 — Diff and merge

Deliver:

```text
DAG traversal
merge base
diff
fast-forward
already-contained
three-way merge
structural collision detection
merge preview
semantic validation of merged candidate
preview token
stale apply protection
merge commit
repeated merge semantics
```

Gate:

```text
property-based DAG suite
Fluree-compatible scenarios
semantic conflict fixtures
Virtual A-Box-dependent merge validation
merge-vs-write concurrency
```

---

## Phase 6 — Reconstruction scalability

Deliver:

```text
checkpoints
checkpoint verification
state digest
reconstruction cache
bounded streaming state export
change index
history lookup APIs
orphan cleanup
```

Only implement optimizations justified by measurements.

Gate:

```text
10k–100k commit histories
large-state reconstruction
checkpoint corruption recovery
performance regression envelope
```

---

## Phase 7 — Production security and resilience

Deliver:

```text
complete OIDC/authorization
tenant isolation
rate limits
security fuzzing
fault injection
backup/restore
upgrade qualification
SBOM
dependency/license gate
container scan
production deployment configuration
TLS/secrets
OpenTelemetry
dashboards/alerts
```

Gate:

all security, failure, recovery and upgrade quality gates.

---

## Phase 8 — Sculpin cognitive-agent product integration

Expose a higher-level Sculpin tool, not raw ledger primitives.

Suggested operations:

```text
propose_knowledge_change
record_feedback
explain_current_fact
show_history
compare_states
review_proposal
accept_proposal
reject_proposal
create_experiment
merge_experiment
```

Agents should not receive unrestricted low-level ref manipulation.

Sculpin should determine when feedback is sufficiently meaningful to create a proposal.

The ledger remains deterministic infrastructure.

---

# 37. Cognitive-agent data model above the ledger

Do not make the ledger itself understand preferences, confidence, observations or personality.

Define this at the Sculpin ontology/policy layer.

Useful distinctions:

```text
Observation
Evidence
Assertion
Correction
Retraction
Supersession
Preference
Skill estimate
Goal
Context
Assessment
```

The ledger versions their RDF representation.

Sculpin determines:

```text
what counts as evidence
how confidence changes
whether recency matters
whether an assertion supersedes another
whether user confirmation is required
```

This preserves a clean boundary between **knowledge evolution infrastructure** and **cognitive policy**.

---

# 38. Feedback-loop protection

Do not allow a reasoning result to become independent evidence merely because Sculpin inferred it.

Track:

```text
external observation
human feedback
source document
derived assertion
accepted assertion
```

A derived assertion promoted into accepted cognitive state must retain provenance showing what evidence and reasoning produced it.

Otherwise repeated reasoning can create a self-reinforcing evidence loop.

Add an end-to-end regression test specifically for this.

---

# 39. Relationship with S3/Parquet and Virtual A-Box

The Cognitive Ledger must remain outside the physical S3 access boundary.

It should store:

```text
logical dataset references
semantic corrections
quality assessments
calibration facts
source-version references
validation provenance
```

It should never store or receive:

```text
S3 credentials
bucket authorization
raw user-supplied object paths
DuckDB SQL
```

A cognitive correction may affect the effective semantic interpretation of external data without changing the Parquet source.

`query_data` should continue to report source values.

Reasoning over the composed semantic state may report corrected/effective values.

This distinction must be tested explicitly.

---

# 40. Updated definition of production-ready success

The service should not be declared production-ready until this scenario is demonstrably reliable:

```text
1. Sculpin KB has base revision K17.

2. Cognitive main points to C100.

3. External dataset is source version D31.

4. An authenticated AI agent proposes RDF change P.

5. Ledger creates immutable candidate C101
   without moving main.

6. Sculpin builds SemanticExecutionContext:
       K17
       C101
       ontology O8
       shapes S5
       dataset D31
       validator V4.

7. Jena + Virtual A-Box validate C101.

8. Immutable validation V101 is recorded.

9. Reviewer/policy accepts the proposal.

10. One PostgreSQL transaction:
       verifies main == C100
       advances main -> C101
       records decision
       records ref event
       writes projection outbox
       records idempotency result.

11. Request success is returned.

12. Projector updates the dedicated Fuseki
    cognitive graph to C101.

13. Fuseki crashes and is rebuilt.

14. Rebuilt projection is identical to C101.

15. A competing agent branch and human correction
    later diverge.

16. Merge preview:
       finds common ancestor
       identifies structural differences
       builds merge candidate
       validates it semantically
       includes current Virtual A-Box context.

17. The underlying external dataset changes.

18. Old merge validation is detected as stale.

19. Merge is revalidated.

20. Reviewer accepts.

21. Merge commit becomes main.

22. Sculpin can explain:
       what changed
       who proposed it
       what evidence supported it
       which alternative existed
       which base KB/ontology/shapes were used
       which external dataset version was used
       what validation concluded
       who accepted it
       what state existed before and after.

23. Historical states reconstruct after:
       application restart
       PostgreSQL restart
       immutable-store restart
       Fuseki rebuild
       software upgrade
       backup/restore.
```

When this scenario is green together with the security, stress, fault, differential and upgrade gates, the Cognitive Ledger is a credible production knowledge-evolution service rather than merely a versioned RDF prototype.

---

# 41. Priorities for the next Claude Code session

The next implementation session should **not start coding branches or merges**.

Its first task should be a focused architectural consolidation:

```text
P0
    reconcile docs with actual implementation
    define commit v2
    define graph/tenant identity
    define effective patch semantics
    define Proposal / Validation / Decision contracts
    define SemanticExecutionContext
    choose production immutable storage
    redesign atomic ref acceptance transaction

P1
    implement those foundations
    add auth/idempotency/resource bounds/OpenAPI
    prove multi-instance persistence correctness

P2
    integrate Sculpin/Jena/Virtual A-Box validation
    add accepted cognitive projection

P3
    branches

P4
    merge

P5
    scale/security/recovery/production qualification
```

Each P0 decision that affects persistent identity or atomicity should receive an ADR before implementation.

---

# 42. Claude Code development discipline

Claude Code should operate as planner/orchestrator rather than allowing one long session to modify every layer opportunistically.

For each phase:

1. Read `AGENTS.md`, `CLAUDE.md`, the authoritative product specification, relevant ADRs and the last completed execution plan.
2. Create a new execution plan containing explicit scope, non-goals, invariants, migration impact, affected crates and acceptance evidence.
3. Use specialized sub-agents for bounded tasks with disjoint ownership:
   - protocol/invariant review;
   - storage/concurrency;
   - API/security;
   - semantic/Sculpin integration;
   - testing/fault injection;
   - performance.
4. Keep canonicalization and protocol files owned by the main session unless the task explicitly concerns them.
5. Do not combine a protocol identity change with unrelated feature development.
6. Update documentation in the same change that changes behavior.
7. Run the fastest deterministic gates continuously.
8. Run integration/differential/fault gates before closing the execution plan.
9. Request independent architecture, invariant, security and test reviews.
10. Resolve every confirmed P0/P1 finding before declaring the phase complete.
11. Preserve explicit evidence of what was executed versus what was unavailable.
12. Do not mark future placeholders or seam-only tests as completed functionality.

A development phase is complete only when its acceptance conditions are executable and pass—not when the implementation merely exists.

---

# 43. Final architectural position

The project should continue toward:

```text
Cognitive Ledger
    immutable knowledge evolution
    provenance
    proposals
    DAG
    branch history
    diff / merge
    decisions

Sculpin
    cognitive policy
    agent orchestration
    evidence interpretation
    ontology mapping

Jena
    semantic reasoning
    SHACL
    domain rules

Fuseki
    query projection
    ontology/base KG

Virtual A-Box
    transient external reality

S3 / Parquet / databases
    source data
```

The implementation should remain deliberately **smaller than Fluree**.

The strongest parts to continue borrowing from Fluree are:

```text
content-addressed immutable history
separate mutable heads
CAS
cheap branches
first-parent evolution
historical state
merge-base calculation
side-effect-free merge preview
stale-state detection
separation of committed history from query/index state
```

The strongest Sculpin-specific advantage is what happens *after* structural merge:

```text
candidate semantic state
        +
domain ontology
        +
SHACL
        +
reasoning
        +
current external Virtual A-Box
        ↓
semantic acceptance decision
```

That capability is what turns the Cognitive Ledger from a generic RDF version store into infrastructure for genuinely evolving, explainable cognitive agents.