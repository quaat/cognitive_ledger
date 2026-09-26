# Sculpin Cognitive Ledger
## Technical Architecture and Implementation Specification

**Status:** Draft specification 0.1  
**Target:** Sculpin cognitive/adaptive knowledge layer  
**Primary implementation language:** Rust  
**Architecture:** Independent containerized service  
**Normative terminology:** **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** indicate requirement strength.

---

# 1. Purpose

The Cognitive Ledger provides Sculpin with a persistent, immutable and auditable mechanism for allowing knowledge to **evolve over time**.

It is responsible for:

- recording graph changes as immutable commits;
- preserving complete knowledge history;
- representing commit ancestry as a directed acyclic graph;
- supporting branches and concurrent proposals;
- supporting deterministic comparison and merge of graph states;
- recording why, when and by whom knowledge changed;
- distinguishing observed/event time from recording time;
- allowing historical graph states to be reconstructed;
- supporting human and agent feedback without immediately mutating accepted knowledge;
- enabling validation of proposed states using Sculpin's existing semantic capabilities;
- providing durable provenance around accepted and rejected changes.

The Cognitive Ledger is **not a graph database replacement**.

Apache Jena/Fuseki remains responsible for:

- SPARQL querying;
- RDF storage optimized for query;
- OWL/RDFS reasoning;
- SHACL validation;
- ontology management;
- Sculpin's virtual A-box and graph-source capabilities.

The ledger is the **system of record for graph evolution**. Fuseki is a **queryable projection and semantic processing environment**.

---

# 2. Design principle

The central architecture is:

```text
                    Sculpin / AI Agents / Users
                              │
                              │ proposed change
                              ▼
                ┌─────────────────────────────┐
                │      Cognitive Ledger       │
                │                             │
                │  immutable commits          │
                │  graph patches              │
                │  DAG                        │
                │  branch refs                │
                │  provenance                 │
                │  temporal history           │
                └──────────────┬──────────────┘
                               │
                      candidate graph state
                               │
                               ▼
              ┌────────────────────────────────┐
              │       Sculpin semantics        │
              │                                │
              │ Fuseki / Jena                  │
              │ SHACL                          │
              │ OWL/RDFS reasoning             │
              │ domain-specific rules          │
              │ virtual A-box / external data  │
              └───────────────┬────────────────┘
                              │
                  validation / reasoning result
                              │
                              ▼
                branch advance / merge / reject
```

The Cognitive Ledger MUST know **how knowledge changed**.

Sculpin MUST remain responsible for determining **what that knowledge means**.

---

# 3. Explicit non-goals

Version 1 MUST NOT attempt to implement:

- a SPARQL engine;
- a general RDF database;
- OWL reasoning;
- RDFS reasoning;
- SHACL execution;
- full-text search;
- vector search;
- GeoSPARQL;
- Cypher;
- GraphQL;
- triple-level authorization;
- distributed consensus;
- generic database replication;
- general-purpose event streaming;
- arbitrary Git compatibility;
- Git-style history rewriting.

These capabilities would move the implementation toward recreating Fluree rather than implementing the Sculpin-specific requirement.

---

# 4. Architectural inspiration

Fluree provides useful architectural precedents for:

- immutable transactions;
- content-addressed commits;
- commit ancestry;
- historical state;
- branches;
- fast-forward merges;
- divergent merges;
- common-ancestor calculation;
- compare-and-set updates of branch heads.

Current Fluree branching supports branches from historical commits, isolated branch evolution, fast-forward and divergent merges, and conflict detection when branches modify the same `(subject, predicate, graph)` combination.

Fluree also separates immutable commit history from optimized indexed state, conceptually similar to the checkpoint architecture specified below.

The Cognitive Ledger SHOULD reproduce the relevant **semantics**, but MUST NOT reproduce Fluree source code or unnecessarily replicate its database architecture.

---

# 5. Fundamental invariants

The following are hard system invariants.

## INV-1 — Commit immutability

Once a commit ID has been created, its contents MUST never change.

## INV-2 — Content identity

A commit ID MUST be derived deterministically from its canonical serialized content.

Changing any content-addressed field MUST produce a different commit ID.

## INV-3 — Parent existence

Every parent referenced by a commit MUST exist before the commit can become reachable from a branch.

## INV-4 — Acyclic ancestry

The commit structure MUST remain a DAG.

A commit cannot directly or indirectly reference itself.

## INV-5 — Branch refs are mutable; commits are not

A branch is a mutable reference to an immutable commit.

```text
main -> C42
```

Advancing `main` to `C43` MUST NOT mutate `C42`.

## INV-6 — Optimistic branch concurrency

Every branch update MUST use compare-and-set semantics.

A client attempting:

```text
expected = C42
new      = C43
```

MUST fail if the current branch head is no longer `C42`.

## INV-7 — Referential integrity

A branch MUST never reference a nonexistent commit.

## INV-8 — Deterministic state reconstruction

Reconstructing the same commit from the same stored objects MUST always produce an RDF-isomorphic state.

Because persisted Cognitive Ledger data will avoid blank nodes, the expected result SHOULD normally be byte-equivalent canonical N-Quads.

## INV-9 — No silent history rewriting

Previously created commits MUST remain addressable even when:

- branches advance;
- branches merge;
- branches are deleted;
- proposals are rejected.

## INV-10 — Projection independence

Failure of Fuseki or another downstream projection MUST NOT corrupt or invalidate committed ledger history.

---

# 6. RDF data model

## 6.1 RDF dataset

The logical state of a Cognitive Ledger graph is an RDF **dataset**, not merely one RDF graph.

A state consists of:

```text
default graph
+
zero or more named graphs
```

Each persisted statement is therefore represented as a quad:

```text
subject
predicate
object
graph
```

The default graph uses an explicit internal sentinel.

---

# 7. Blank-node policy

Blank nodes are problematic for:

- stable identity;
- patching;
- hashing;
- comparison;
- merging;
- provenance.

W3C RDFC-1.0 provides a standardized mechanism for canonicalizing RDF datasets and assigning deterministic blank-node identifiers. It became a W3C Recommendation in 2024.

However, canonicalization itself can be computationally expensive for pathological blank-node structures.

Therefore:

### Persistent Cognitive Ledger states MUST NOT contain anonymous blank nodes.

At ledger ingress, blank nodes MUST either:

1. be deterministically or securely skolemized into Sculpin-controlled IRIs; or
2. cause the transaction to be rejected when safe skolemization cannot be guaranteed.

Example:

```text
_:b42
```

may become:

```text
urn:sculpin:node:01K...
```

RDFC-1.0 SHOULD still be implemented as an import/interoperability capability for:

- comparing externally supplied RDF datasets;
- snapshot verification;
- interoperability tests;
- data where blank nodes cannot be avoided before ingest.

Transient virtual A-box graphs MAY contain blank nodes because they are not committed to ledger history.

---

# 8. Patch representation

Apache Jena RDF Patch already defines atomic RDF dataset changes containing:

- additions;
- deletions;
- triples;
- quads;
- transaction boundaries;
- patch metadata.

It also explicitly supports patch identifiers and predecessor references.

The Cognitive Ledger SHOULD therefore use an RDF Patch-compatible model.

## 8.1 Canonical Cognitive Patch

Internally, a patch consists of:

```text
deletions[]
additions[]
```

where each entry is a normalized RDF quad.

The canonical serialization MUST:

1. normalize RDF terms;
2. reject persisted blank nodes;
3. eliminate duplicate operations;
4. reject or explicitly resolve contradictory operations;
5. lexicographically sort deletions;
6. lexicographically sort additions;
7. serialize terms using canonical N-Quads lexical representation;
8. use LF line endings;
9. use UTF-8.

Illustratively:

```text
D <urn:x> <urn:status> "unknown" <urn:graph:state> .
A <urn:x> <urn:status> "operational" <urn:graph:state> .
```

Prefix declarations MUST NOT affect semantic patch identity.

Prefixes are presentation metadata, not RDF dataset semantics.

---

# 9. Patch identity

Every patch receives:

```text
patch_id = sha256(canonical_patch_bytes)
```

represented externally as:

```text
sha256:<64-lowercase-hex-characters>
```

Hash algorithm names MUST be explicit to permit future algorithm migration.

---

# 10. Commit model

A commit represents one immutable cognitive change.

Conceptually:

```json
{
  "format": "sculpin-cognitive-commit/v1",
  "graph": "019f...",
  "parents": [
    "sha256:..."
  ],
  "patch": "sha256:...",
  "actor": {
    "subject": "..."
  },
  "activity": "feedback",
  "eventTime": "2026-09-24T12:15:00Z",
  "recordedAt": "2026-09-24T12:16:32.351Z",
  "semanticContext": {
    "ontology": "...",
    "shapes": "..."
  },
  "evidence": [
    "..."
  ],
  "message": "Updated operating temperature from supplier documentation."
}
```

---

# 11. Commit canonicalization

Commit metadata SHOULD use JSON because it is:

- easy to inspect;
- easy to debug;
- language-independent;
- readily exposed through REST APIs.

Content hashing MUST use a deterministic serialization.

JSON Canonicalization Scheme, RFC 8785, defines a deterministic property ordering and serialization intended specifically for repeatable hashing and signing.

Therefore:

```text
canonical_commit = JCS(commit_without_id)
commit_id        = SHA256(canonical_commit)
```

The commit ID MUST NOT be included in its own hash input.

Fields whose values cannot be represented safely within JCS numeric constraints SHOULD be represented as strings.

---

# 12. Parent semantics

## 12.1 Genesis commit

A genesis commit has:

```text
parents = []
```

## 12.2 Ordinary commit

An ordinary commit has exactly one parent:

```text
parents = [previous_head]
```

## 12.3 Merge commit

A merge commit has two parents:

```text
parents = [
    target_head,
    source_head
]
```

Parent ordering is significant.

The first parent defines the state against which the merge patch is applied.

This produces Git-like first-parent history:

```text
target ------ M
             /
source ------+
```

The state of `M` is therefore:

```text
state(target_head)
+
merge_patch
```

The second parent represents ancestry and provenance but is not necessary for materializing the resulting graph state.

Version 1 MUST restrict commits to at most two parents.

---

# 13. State model

A commit is not required to contain a complete snapshot.

Its state is logically:

```text
state(parent[0]) + patch
```

For genesis:

```text
empty RDF dataset + patch
```

This permits compact history.

---

# 14. Checkpoints

Replaying all commits from genesis eventually becomes inefficient.

The ledger MUST therefore support derived **checkpoint objects**.

```text
checkpoint
    commit_id
    canonical RDF dataset
    digest
    compression
    created_at
```

Checkpoint data SHOULD be canonical N-Quads compressed using Zstandard.

A checkpoint is **derived data** and MUST NOT affect the identity of the commit it represents.

Checkpoint creation MAY occur according to:

- number of commits since the last checkpoint;
- accumulated patch bytes;
- reconstructed state size;
- expensive merge;
- administrator request.

Suggested initial defaults:

```text
100 commits
or
64 MiB accumulated patch content
```

These values are operational defaults, not protocol requirements.

A merge commit MAY trigger an immediate checkpoint when the resulting state is expensive to reconstruct.

---

# 15. State reconstruction

Given commit `C`:

1. walk first-parent ancestry toward genesis;
2. locate the nearest checkpoint;
3. load the checkpoint;
4. replay patches forward;
5. optionally calculate a resulting state digest;
6. return or stream the RDF dataset.

The implementation SHOULD cache recently reconstructed states.

---

# 16. State digest

A state digest is useful for:

- integrity testing;
- cross-system comparison;
- checkpoint validation;
- differential testing.

For blank-node-free state:

```text
digest =
    SHA256(
        lexicographically sorted canonical N-Quads
    )
```

State digests SHOULD be stored with checkpoints.

They SHOULD NOT initially form part of commit identity because calculating a complete dataset digest on every commit may be unnecessarily expensive.

A future incremental Merkle-set implementation MAY make state roots practical per commit.

---

# 17. Branch model

A branch is:

```text
(graph_id, branch_name) -> commit_id
```

Examples:

```text
main
feedback/thomas
agent/evaluation-17
experiment/new-rule
review/committee-a
```

`main` SHOULD be the default accepted-knowledge branch.

Branch names MUST be validated and bounded in length.

---

# 18. Branch creation

A branch MAY be created from:

- another branch HEAD;
- any reachable historical commit;
- genesis.

Creating a branch MUST NOT copy commit objects.

It only creates a new ref:

```text
feedback/user -> C100
```

---

# 19. Branch ref storage

A relational representation is recommended:

```text
graph_id
branch
head_commit_id
version
updated_at
updated_by
```

Branch advancement MUST use an atomic compare-and-set update.

Conceptually:

```sql
UPDATE refs
SET
    head_commit_id = :new_head,
    version = version + 1
WHERE graph_id = :graph
  AND branch = :branch
  AND head_commit_id = :expected_head;
```

If zero rows are changed:

```text
HTTP 409 Conflict
```

MUST be returned.

---

# 20. Ref event history

Every ref change MUST generate an immutable ref event:

```text
RefEvent
    graph
    branch
    old_head
    new_head
    principal
    timestamp
    operation
    reason
```

Operations include:

```text
create
advance
fast_forward
merge
reset
delete
restore
```

This permits auditing branch movement without encoding branch identity into commits.

---

# 21. Temporal model

The system MUST distinguish at least:

### `eventTime`

When the represented real-world event occurred.

### `recordedAt`

When the Cognitive Ledger recorded the information.

Example:

```text
eventTime:
2026-08-12T08:00:00Z

recordedAt:
2026-09-24T14:37:00Z
```

The distinction allows Sculpin to answer both:

> What did we know on 20 August?

and:

> What do we now know was true on 20 August?

Fluree similarly distinguishes historical transaction/recording semantics from event-time semantics.

`recordedAt` MUST be server controlled.

`eventTime` MAY come from trusted source data and MUST be preserved as provenance rather than blindly interpreted as commit ordering.

Unlike Fluree's linear transaction sequence, Cognitive Ledger commits on independent branches MAY have overlapping event times.

Commit ancestry—not wall-clock time—is authoritative for graph evolution.

---

# 22. Provenance

Provenance is a primary requirement rather than auxiliary metadata.

W3C PROV-O SHOULD be used as the interoperability model for provenance export. PROV-O is a W3C Recommendation designed specifically for representing entities, activities, agents and derivation relationships.

The ledger MUST distinguish:

| Provenance dimension | Meaning |
|---|---|
| Commit provenance | Why a graph state changed |
| Actor provenance | Human, agent or system responsible |
| Source provenance | Evidence supporting the change |
| Validation provenance | Semantic rules used to evaluate it |
| Temporal provenance | Event time vs recording time |
| Decision provenance | Why a proposal was accepted or rejected |

Commit metadata SHOULD include:

```text
authenticated actor
activity type
message
evidence references
event time
source system
agent identifier/version
correlation ID
```

---

# 23. Fact-level provenance

The ledger MUST NOT prescribe one exclusive method for modeling provenance of individual facts.

Sculpin MAY use:

- named graphs;
- PROV-O;
- explicit claim resources;
- statement annotation mechanisms.

The Cognitive Ledger versions those RDF statements like any other information.

Commit provenance and fact provenance are distinct.

---

# 24. Authentication and actor identity

Commit author identity MUST NOT come from an arbitrary client-supplied string.

The service MUST obtain the principal from authenticated context such as:

- OIDC access token;
- service identity;
- trusted internal workload identity.

The authenticated principal becomes commit actor metadata.

An optional `onBehalfOf` identity MAY be supported where policy permits delegation.

---

# 25. Proposal workflow

The system MUST allow graph changes to exist without modifying accepted knowledge.

Example:

```text
                   P1 --- P2
                  /
main --- C99 --- C100
```

where `P1/P2` represent an agent or human proposal branch.

A proposal can then be:

```text
accepted
rejected
superseded
modified
merged
```

Rejected proposals SHOULD remain auditable unless an explicit retention policy permits their removal.

---

# 26. Transaction pipeline

A normal branch transaction SHOULD execute:

```text
1. Authenticate caller
2. Resolve graph + branch
3. Read current HEAD H
4. Verify expectedHead == H
5. Parse RDF changes
6. Normalize RDF terms
7. Skolemize/reject blank nodes
8. Canonicalize patch
9. Store patch object
10. Build candidate commit C(parent=H)
11. Store immutable commit object
12. Perform configured validation
13. CAS branch H -> C
14. Record ref event
15. Emit projection event
16. Return commit receipt
```

Steps 13–15 MUST occur within one PostgreSQL transaction.

External semantic validation SHOULD occur before branch advancement.

Because validation may take significant time, the branch HEAD must be checked again during the CAS operation.

If another writer advances the branch during validation:

```text
409 HEAD_CHANGED
```

is returned.

The immutable candidate commit MAY remain stored as an unattached proposal.

---

# 27. Validation records

Semantic validation results MUST NOT be inserted into the commit after hashing.

Instead, validation is a separate immutable object:

```text
ValidationRecord
    validation_id
    commit_id
    ontology_ref
    shapes_ref
    external_context_refs
    context_digest
    validator_version
    started_at
    completed_at
    status
    findings
```

This avoids circular commit construction and permits the same commit to be evaluated under different semantic contexts.

---

# 28. Validation levels

The following validation levels SHOULD exist:

### Level 0 — Syntax

- valid RDF;
- valid literals;
- supported RDF terms;
- no forbidden blank nodes;
- valid IRIs.

### Level 1 — Patch consistency

- deletions applicable to base state;
- contradictory operations rejected;
- operation size limits respected.

### Level 2 — SHACL

Sculpin's configured SHACL validation.

### Level 3 — Reasoning

Configured Jena reasoning/domain-rule checks.

### Level 4 — External context

Validation incorporating virtual A-box/external data.

Branch policies determine required levels.

For example:

```text
main:
    require syntax
    require patch consistency
    require SHACL
    require reasoning

experiment/*:
    require syntax only
```

---

# 29. Virtual A-box integration

External data incorporated through Sculpin's virtual A-box MUST remain transient unless explicitly committed.

Validation may conceptually use:

```text
candidate ledger state
+
ontology
+
SHACL
+
temporary external A-box
```

The external graph SHOULD NOT automatically become part of the commit patch.

The validation record MUST identify the external context used as precisely as reasonably possible:

```text
source URI
snapshot/version ID
query parameters
dataset version
object hash
retrieval timestamp
```

Where a source provides no version identifier, Sculpin SHOULD calculate a digest of the relevant materialized external data used during validation when practical.

---

# 30. Accepted-state projection

The Cognitive Ledger is the authoritative change history.

Fuseki is a projection.

After a successful branch advance:

```text
ledger commit
      │
      ▼
durable projection event
      │
      ▼
projector
      │
      ▼
Fuseki dataset
```

The ledger MUST maintain:

```text
projection_head
```

for each configured projection.

---

# 31. Projection consistency

Projection updates MUST be idempotent.

The projector MUST tolerate:

- process crashes;
- duplicate events;
- network errors;
- Fuseki restarts;
- retry after partial failure.

The projection MUST store or otherwise verify which commit it currently represents.

If Fuseki is behind:

```text
ledger head     = C105
projection head = C102
```

the service MUST expose this lag through health/status/metrics.

---

# 32. Projection failure

A failed projection MUST NOT roll back committed ledger history.

Instead:

```text
ledger = authoritative
projection = degraded
```

The projector retries from its last successful commit.

This is essential for avoiding distributed transactions between PostgreSQL/object storage and Fuseki.

---

# 33. Merge model

Merging MUST use three-way merge semantics.

Given:

```text
                 source S
                /
ancestor A ----+
                \
                 target T
```

the system computes:

```text
delta_source = A -> S
delta_target = A -> T
```

and produces candidate merged state `M`.

---

# 34. Common ancestor

The implementation MUST correctly identify a lowest/common reachable ancestor of the source and target DAG histories.

The algorithm MUST:

- support merge commits;
- maintain a visited set;
- terminate on malformed data;
- be bounded by configurable traversal limits;
- provide an explicit error when limits are exceeded.

---

# 35. Fast-forward merge

If:

```text
T is an ancestor of S
```

then:

```text
target ref T -> S
```

is sufficient.

No new commit is required.

A ref event MUST record that a fast-forward merge occurred.

---

# 36. Already-contained merge

If:

```text
S is an ancestor of T
```

the merge is a semantic no-op.

The API SHOULD return:

```text
already_contained = true
```

and MUST NOT generate a meaningless commit.

---

# 37. Divergent merge

For divergent histories, the merged state is calculated and represented by a new merge commit:

```text
parents = [target_head, source_head]
patch   = target_state -> merged_state
```

---

# 38. Structural conflicts

Version 1 SHOULD use the same coarse conflict key as Fluree for differential compatibility:

```text
(graph, subject, predicate)
```

A collision exists where both branches independently modify the resulting object-set for the same key.

This is deliberately called a **structural collision**, because RDF allows multiple objects for one predicate.

Example:

```text
target:
:x :temperature 80 .

source:
:x :temperature 90 .
```

may or may not be invalid according to the ontology.

---

# 39. Structural merge strategies

Supported strategies SHOULD initially be:

```text
abort
take-target
take-source
union
```

`abort` SHOULD be the default for protected branches such as `main`.

`union` MUST NOT imply semantic validity.

Every resolved candidate MUST still pass configured semantic validation.

---

# 40. Semantic conflicts

After structural combination:

```text
merged candidate
       │
       ▼
Sculpin semantic validation
```

may identify conflicts such as:

```text
SHACL maxCount violation
datatype violation
closed-shape violation
ontology inconsistency
domain-specific rule violation
external-data contradiction
```

This allows Sculpin to distinguish:

```text
structural collision
```

from:

```text
semantic conflict
```

This is a central capability of the Cognitive Ledger.

---

# 41. Merge preview

Merge SHOULD be a two-stage operation.

### Stage 1

```text
POST /merge/preview
```

returns:

```text
common ancestor
source delta
target delta
structural collisions
proposed resolutions
semantic validation
preview token
```

### Stage 2

```text
POST /merge/apply
```

takes the preview token.

The token MUST cryptographically bind or identify:

```text
source head
target head
ancestor
resolution policy
candidate result
```

If either head changed after preview:

```text
409 MERGE_STALE
```

MUST be returned.

This avoids time-of-check/time-of-use errors.

---

# 42. Rebase

Git-style rebase MUST NOT be required in version 1.

Rebase rewrites lineage and is generally undesirable for an audit-focused cognitive knowledge system.

A future `replay` operation MAY replay proposals against a newer branch state, but the original commits MUST remain addressable.

---

# 43. Storage architecture

Recommended deployment:

```text
┌──────────────────────────────┐
│ Cognitive Ledger service     │
│ Rust / HTTP                  │
└──────────────┬───────────────┘
               │
       ┌───────┴────────┐
       │                │
       ▼                ▼
 PostgreSQL       Object Store
 metadata         immutable bytes
 refs             patches
 indexes          commits
 audit            checkpoints
```

---

# 44. PostgreSQL responsibilities

PostgreSQL SHOULD store:

```text
graphs
refs
ref_events
commit_index
commit_parents
patch_index
change_index
validation_index
projection_state
projection_outbox
checkpoint_index
idempotency_keys
authorization metadata
```

Immutable commit and patch contents MAY also be stored in PostgreSQL for small installations, but the architecture SHOULD expose an object-store abstraction.

---

# 45. Object store

Required interface:

```rust
trait ObjectStore {
    put(content_id, bytes)
    get(content_id)
    exists(content_id)
    delete(content_id)
}
```

Initial implementations SHOULD include:

```text
filesystem
S3-compatible storage
```

Object writes MUST be idempotent.

Writing different bytes under an existing content ID MUST be considered corruption and fail hard.

---

# 46. Object layout

A filesystem/S3 layout MAY use:

```text
objects/
    sha256/
        ab/
            cd/
                abcdef...
```

Object type SHOULD be encoded in metadata rather than relying exclusively on directory structure.

---

# 47. Change index

The service is not a general RDF query engine, but efficient cognitive history lookup is valuable.

PostgreSQL SHOULD maintain a change index for each patch operation:

```text
commit_id
operation
graph
subject
predicate
object_hash
```

This supports questions such as:

> When did this property change?

without querying complete historical snapshots.

The complete RDF object remains in the immutable patch.

---

# 48. API design

Base path:

```text
/v1
```

Core resources:

```text
/graphs
/refs
/commits
/transactions
/state
/diff
/history
/merge
/validations
/projections
```

---

# 49. Core API surface

Recommended version-1 operations:

```text
POST   /v1/graphs
GET    /v1/graphs/{graph}

GET    /v1/graphs/{graph}/refs
POST   /v1/graphs/{graph}/refs
GET    /v1/graphs/{graph}/refs/{branch}
DELETE /v1/graphs/{graph}/refs/{branch}

POST   /v1/graphs/{graph}/transactions

GET    /v1/graphs/{graph}/commits/{commit}
GET    /v1/graphs/{graph}/history

GET    /v1/graphs/{graph}/state/{ref}
GET    /v1/graphs/{graph}/diff

POST   /v1/graphs/{graph}/merge/preview
POST   /v1/graphs/{graph}/merge/apply

GET    /v1/validations/{id}

GET    /v1/graphs/{graph}/projections
```

---

# 50. Transaction request

Example:

```json
{
  "branch": "main",
  "expectedHead": "sha256:...",
  "patch": {
    "format": "rdf-patch",
    "content": "..."
  },
  "eventTime": "2026-09-24T12:15:00Z",
  "activity": "human-feedback",
  "message": "Corrected operating temperature.",
  "evidence": [
    "urn:document:manufacturer-specification"
  ],
  "validation": "branch-policy"
}
```

---

# 51. Commit receipt

Successful response:

```json
{
  "graph": "...",
  "branch": "main",
  "commit": "sha256:...",
  "previousHead": "sha256:...",
  "patch": "sha256:...",
  "recordedAt": "...",
  "validation": {
    "id": "sha256:...",
    "status": "passed"
  },
  "projection": {
    "status": "pending"
  }
}
```

---

# 52. Error model

Stable machine-readable error codes MUST be provided.

Examples:

```text
HEAD_CHANGED
INVALID_RDF
INVALID_PATCH
PATCH_DELETE_MISSING
BLANK_NODE_FORBIDDEN
VALIDATION_FAILED
SHACL_FAILED
REASONING_FAILED
MERGE_CONFLICT
MERGE_STALE
ANCESTOR_LIMIT_EXCEEDED
PROJECTION_UNAVAILABLE
UNAUTHORIZED
FORBIDDEN
OBJECT_CORRUPTION
```

HTTP status codes SHOULD be used conventionally:

```text
400 malformed request
401 unauthenticated
403 unauthorized
404 unknown object
409 optimistic concurrency / merge conflict
413 payload too large
422 semantically invalid transaction
503 unavailable dependency
```

---

# 53. Idempotency

Mutation APIs SHOULD support:

```text
Idempotency-Key
```

Repeated requests with the same authenticated actor, graph and idempotency key MUST either:

- return the same result; or
- return an explicit idempotency conflict if the payload differs.

---

# 54. Resource limits

Configurable limits MUST exist for:

```text
patch bytes
number of operations
metadata bytes
evidence references
branch name length
history traversal depth
merge traversal
state export size
validation duration
```

This prevents accidental or malicious resource exhaustion.

---

# 55. Security model

Version 1 requires graph-level authorization.

Roles MAY include:

```text
reader
contributor
reviewer
maintainer
administrator
```

Protected branches SHOULD restrict:

```text
direct updates
merge approval
force reset
branch deletion
```

`main` SHOULD normally prohibit unaudited force updates.

Triple-level authorization is intentionally outside scope.

---

# 56. Signing

Cryptographic signing is not mandatory for the first MVP, but the data model MUST permit it later.

A future signature object SHOULD sign:

```text
commit_id
actor
signature algorithm
key identifier
timestamp
```

rather than modifying commit content.

---

# 57. Garbage collection

The default policy SHOULD prioritize auditability over aggressive deletion.

Objects fall into three categories:

### Reachable

Referenced from a branch or protected audit root.

MUST NOT be deleted.

### Historical but unreferenced

Previously reachable or intentionally retained proposal history.

SHOULD normally be retained.

### Staged/orphaned technical objects

Created during an operation that never produced a registered commit.

MAY be garbage collected after a configurable grace period.

GC MUST use mark-and-sweep semantics from protected roots.

---

# 58. Observability

The service MUST expose:

```text
/health
/ready
/metrics
```

Metrics SHOULD include:

```text
commit latency
validation latency
CAS conflicts
merge latency
checkpoint latency
state reconstruction latency
projection lag
projection failures
object store latency
commit count
patch bytes
branch count
orphan object count
```

OpenTelemetry tracing SHOULD be supported.

A correlation ID SHOULD propagate through:

```text
Sculpin request
→ ledger
→ validator
→ virtual A-box
→ projector
→ Fuseki
```

---

# 59. Docker deployment

A development deployment SHOULD include:

```text
cognitive-ledger
postgres
fuseki
```

Optional services:

```text
minio
toxiproxy
prometheus
```

The complete system MUST be runnable through:

```text
docker compose up
```

without requiring manually installed databases.

---

# 60. Differential Fluree test environment

Fluree SHOULD be included as a **test-only reference service**.

The official `fluree/server` image currently packages the server as a self-contained container, exposes port `8090`, provides `/health`, and stores data under `/var/lib/fluree`.

Reference topology:

```text
                    Test Runner
                    /         \
                   /           \
                  ▼             ▼
       Cognitive Ledger        Fluree
             │                   │
        PostgreSQL          Fluree storage
             │
           Fuseki
```

The Fluree container MUST NOT be part of the Sculpin runtime dependency graph.

It MUST only be used by testing/benchmark profiles.

---

# 61. Fluree image pinning

CI MUST NOT use:

```text
fluree/server:latest
```

despite this being convenient interactively.

Instead CI MUST pin:

```text
version
+
image digest
```

in a repository-controlled file such as:

```text
test/reference-images.lock
```

Example conceptual structure:

```yaml
fluree:
  image: fluree/server
  version: "..."
  digest: "sha256:..."
```

Updating the reference image MUST be an explicit reviewed change.

Because Fluree uses BSL licensing, the reference container SHOULD be pulled during development/CI rather than redistributed as part of Sculpin artifacts, and the intended CI use SHOULD receive the same legal review applied to other third-party test dependencies.

---

# 62. Differential adapter

The integration test framework SHOULD define a common abstract interface:

```rust
trait VersionedGraphReference {
    create_graph(...)
    transact(...)
    state(...)
    create_branch(...)
    branch_head(...)
    history(...)
    diff(...)
    merge(...)
}
```

Adapters:

```text
CognitiveLedgerAdapter
FlureeAdapter
```

The same deterministic scenario can then execute against both systems.

---

# 63. What to compare with Fluree

The following semantics SHOULD be tested side by side.

| Capability | Differential comparison |
|---|---|
| Linear transactions | Yes |
| Assertion/retraction | Yes |
| Historical state | Yes |
| Branch isolation | Yes |
| Branch from HEAD | Yes |
| Branch from history | Yes |
| Common ancestor | Yes |
| Fast-forward merge | Yes |
| Divergent merge | Yes |
| `(g,s,p)` collision | Yes |
| Repeated merge | Yes |
| Deleted/re-added facts | Yes |
| Commit identity | **No** |
| Transaction numbers | **No** |
| Binary storage format | **No** |
| SHACL behavior | Separate test |
| Reasoner behavior | Separate test |
| Query performance | Not equivalent |
| Search/vector functionality | Out of scope |

Fluree currently exposes immutable history, branch isolation, historical branch creation and Git-like merging, making these particularly useful reference behaviors.

---

# 64. Differential state comparison

Commit identifiers MUST NOT be compared.

Instead:

```text
Cognitive Ledger state
        │
        ▼
canonical N-Quads
        │
        ▼
state digest
```

and:

```text
Fluree state
        │
        ▼
canonical N-Quads
        │
        ▼
state digest
```

are compared.

Expected:

```text
digest(cognitive) == digest(fluree)
```

where the scenario is intended to have equivalent semantics.

When blank nodes appear in reference data, RDFC-1.0 MUST be used before comparison.

---

# 65. Differential test scenarios

The deterministic suite MUST include at least:

### D01 — Genesis

Create identical initial graph.

### D02 — Linear append

Perform 100 sequential additions.

Verify every historical state.

### D03 — Retraction

Add and later delete a fact.

Verify state before and after.

### D04 — Replace value

```text
80 -> 90
```

through delete + add.

### D05 — Branch isolation

```text
main     C1 -> C2
              \
experiment     C3
```

Verify neither branch observes the other's new data.

### D06 — Historical branch

Branch from `C1` after `main` has reached `C5`.

### D07 — Fast-forward merge

Branch advances while target remains unchanged.

### D08 — Divergent non-conflicting merge

Both branches modify different `(g,s,p)` keys.

### D09 — Structural conflict

Both change the same `(g,s,p)` differently.

### D10 — Same-result modification

Both independently produce the same final state.

### D11 — Delete versus modify

One branch retracts a relation while another changes its object.

### D12 — Multiple named graphs

Identical SPO terms in different named graphs MUST remain independent.

### D13 — Repeated merge

Merge a branch twice and verify no duplicate semantic changes.

### D14 — Deep DAG

Thousands of commits plus regular branching.

### D15 — Merge ancestry

Multiple merge commits followed by another common-ancestor calculation.

---

# 66. Property-based differential testing

In addition to fixed scenarios, the test harness SHOULD generate deterministic randomized operation sequences.

Operations:

```text
add quad
delete quad
create branch
advance branch
merge
historical read
diff
```

Each run uses a recorded seed.

Failures MUST print:

```text
seed
minimal operation sequence
expected state
actual state
```

Property-based shrinking SHOULD reduce failures to a minimal reproducer.

---

# 67. Fluree as oracle — limits

Fluree MUST NOT be considered normative for every behavior.

It is a reference for common versioned-RDF semantics.

Sculpin intentionally differs by adding:

```text
semantic merge validation
external virtual A-box validation
Sculpin provenance conventions
proposal workflow
projection architecture
branch policy
```

Tests must therefore classify outcomes as:

```text
equivalent semantic expectation
Sculpin extension
intentional divergence
```

---

# 68. Performance comparison with Fluree

Side-by-side timings are useful, but MUST initially be **informational rather than release-blocking**.

Fluree is a sophisticated RDF database with dedicated indexes and query execution. The Cognitive Ledger has a much narrower purpose.

Therefore:

```text
Fluree timing != Cognitive Ledger acceptance threshold
```

The comparison SHOULD identify:

- unexpectedly expensive DAG operations;
- pathological patch handling;
- poor state-reconstruction scaling;
- excessive storage overhead;
- unexpected merge complexity.

Performance release gates SHOULD primarily compare Cognitive Ledger against its own previous baseline.

---

# 69. Benchmark dimensions

Benchmark scenarios SHOULD vary:

### Dataset size

```text
1,000 quads
100,000 quads
1,000,000 quads
```

### Commit count

```text
100
1,000
10,000
100,000
```

### Patch size

```text
1
10
100
1,000
10,000 operations
```

### Branch count

```text
1
10
100
1,000
```

### Concurrency

```text
1
8
32
64 writers
```

### DAG shape

```text
linear
wide branching
deep branching
repeated merges
high merge density
```

---

# 70. Measurements

Each benchmark SHOULD capture:

```text
throughput
p50 latency
p95 latency
p99 latency
CPU
peak RSS
disk bytes
object-store bytes
PostgreSQL bytes
checkpoint bytes
reconstruction depth
```

Results SHOULD be persisted as CI artifacts.

---

# 71. Performance gates

Initial hard absolute numbers SHOULD NOT be invented before representative hardware and realistic Sculpin workloads are established.

Instead:

### Phase A

Establish repeatable baselines on a dedicated CI runner.

### Phase B

Create regression thresholds.

Recommended initial release gate:

```text
No >20% regression in p95
on designated critical benchmarks
without explicit performance-review approval.
```

For low-noise microbenchmarks, a stricter threshold MAY later be adopted.

Fluree comparisons remain informational.

---

# 72. Critical concurrency stress test

One particularly important invariant test is:

```text
HEAD = C100
```

Launch 1,000 writers concurrently, all using:

```text
expectedHead = C100
```

Expected result:

```text
exactly one successful branch advance
999 HEAD_CHANGED results
```

After completion:

```text
branch HEAD points to one valid child of C100
no branch references nonexistent data
all candidate immutable objects remain internally valid
```

This MUST be a release-blocking test.

---

# 73. Parallel independent branches

Create 100 branches from the same base and concurrently commit 100 updates to each.

Expected:

```text
10,000 successful independent commits
zero cross-branch contamination
correct branch HEADs
correct state reconstruction
```

---

# 74. Crash/failure injection

Failure testing SHOULD deliberately interrupt processing at important points:

```text
after patch object write
after commit object write
before PostgreSQL metadata commit
before branch CAS
after branch CAS
before projection outbox processing
during Fuseki update
after Fuseki update but before projector acknowledgement
during checkpoint creation
```

The resulting system MUST satisfy:

```text
no dangling branch refs
no corrupted commit objects
no partial logical commits
safe retry
idempotent projection
recoverable checkpoint generation
```

Toxiproxy MAY be used for network failure injection around PostgreSQL, Fuseki and S3-compatible storage.

Internal Rust failpoints SHOULD be used where network proxies cannot reproduce the desired crash boundary.

---

# 75. PostgreSQL restart testing

Tests MUST cover:

```text
process restart
database restart
unclean service termination
connection loss during transaction
connection-pool exhaustion
```

After restart, all committed branch heads MUST remain valid and reconstructable.

---

# 76. Fuseki failure testing

The ledger MUST remain writable according to branch policy if Fuseki projection is temporarily unavailable **only where semantic validation is not required**.

For protected branches requiring semantic validation:

```text
validator unavailable
→ commit MUST NOT advance protected branch
```

Previously accepted commits remain valid.

Projection retries when Fuseki returns.

---

# 77. Object-store failure testing

Test:

```text
write timeout
read timeout
truncated read
hash mismatch
missing object
duplicate put
```

Hash mismatch MUST be treated as integrity corruption rather than ordinary application failure.

---

# 78. Quality gates

The following gates are required before production release.

## QG-0 — Specification consistency

PASS requires:

- all core invariants represented by automated tests;
- API semantics documented;
- serialization format versioned;
- storage schema versioned;
- upgrade strategy documented.

---

## QG-1 — Build and static analysis

PASS requires:

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Core ledger crates SHOULD use:

```rust
#![forbid(unsafe_code)]
```

unless a reviewed dependency/interface makes this impractical.

---

## QG-2 — Dependency and licence gate

PASS requires:

```text
known dependency vulnerabilities reviewed
license allow-list clean
SBOM generated
container image scanned
no accidental Fluree runtime dependency
```

Tools such as `cargo-deny`, `cargo-audit` and an OCI scanner SHOULD be integrated.

---

## QG-3 — Content determinism

PASS requires:

```text
same logical commit
+ same canonical metadata
+ same patch
→ identical commit ID
```

across:

```text
multiple executions
multiple machines
debug/release builds
supported OS environments
```

Golden vectors MUST be stored in the repository.

---

## QG-4 — RDF correctness

PASS requires:

- canonical quad parser tests;
- datatype tests;
- language-tag tests;
- named-graph tests;
- skolemization tests;
- RDF Patch interoperability tests;
- RDFC test vectors where RDFC is implemented.

Apache Jena RDF Patch behavior SHOULD be part of integration tests because the patch format explicitly supports atomic RDF additions/deletions and patch-log metadata.

---

## QG-5 — DAG invariants

PASS requires automated proof-by-test that:

```text
no invalid parent
no cycle
correct first-parent reconstruction
correct common ancestor
correct fast-forward detection
correct merge-parent ordering
```

Randomized DAG property tests are mandatory.

---

## QG-6 — Persistence and recovery

PASS requires successful:

```text
container restart
PostgreSQL restart
projector restart
object-store restart
crash recovery
```

with no loss of acknowledged commits.

---

## QG-7 — Differential reference suite

PASS requires all designated Fluree-compatible structural scenarios to produce equivalent final/historical RDF states.

A mismatch MUST be classified as:

```text
Cognitive Ledger defect
Fluree-reference difference
intentional documented divergence
test defect
```

Unclassified differences block release.

---

## QG-8 — Sculpin semantic integration

PASS requires end-to-end tests that:

```text
commit candidate
→ materialize graph
→ apply ontology
→ run SHACL
→ run configured reasoning
→ include virtual A-box
→ produce immutable validation record
→ correctly allow/reject ref advancement
```

---

## QG-9 — Concurrency

PASS requires:

- 1,000 same-HEAD competing writer test;
- independent branch stress;
- merge-vs-write races;
- branch-delete/update races;
- no lost updates;
- no silent ref overwrite.

---

## QG-10 — Projection consistency

PASS requires:

```text
ledger head reachable
projection eventually reaches same state
duplicate projection events harmless
projector restart harmless
projection state digest agrees with ledger state digest
```

---

## QG-11 — Performance regression

PASS requires critical Cognitive Ledger benchmark results remain within the accepted regression envelope.

Fluree timing differences do not independently fail this gate.

---

## QG-12 — Security

PASS requires testing for:

```text
unauthenticated write
unauthorized branch update
actor spoofing
oversized patch
metadata exhaustion
malformed RDF
malformed Unicode
hash mismatch
path traversal
SQL injection
branch-name abuse
history traversal exhaustion
```

Fuzzing MUST cover externally supplied RDF Patch and commit metadata parsers.

---

## QG-13 — API compatibility

Every API change MUST be classified:

```text
backward compatible
versioned breaking change
internal
```

OpenAPI schema compatibility SHOULD be checked automatically.

---

## QG-14 — Upgrade/recovery

PASS requires upgrading a persisted previous-release test ledger to the candidate release while retaining:

```text
commit IDs
branch heads
historical reconstruction
validation references
projection recoverability
```

Commit identity rules MUST never change silently.

---

# 79. Test pyramid

Recommended test distribution:

```text
                    E2E
              Sculpin + Fuseki
             Fluree differential

               Integration
        Postgres / object store / API
         projection / validation

             Property tests
          DAG / merge / patches

                 Unit
      serialization / hashing / refs
```

The project SHOULD favor property/invariant tests over chasing an arbitrary global code-coverage percentage.

Critical core modules SHOULD nevertheless maintain high coverage, with coverage regression reported automatically.

---

# 80. CI profiles

## Pull request

Run:

```text
format
clippy
unit tests
property tests
PostgreSQL integration
small Fuseki integration
small deterministic Fluree differential suite
```

## Main branch

Additionally run:

```text
randomized differential suite
restart tests
failure injection subset
performance smoke tests
```

## Nightly

Run:

```text
large differential suite
10k–100k commit scenarios
stress tests
high-concurrency tests
large state reconstruction
full failure injection
fuzzing
performance benchmarks
```

## Release candidate

Run every quality gate.

---

# 81. Reference Docker Compose profile

Conceptually:

```yaml
services:

  ledger:
    # Cognitive Ledger

  postgres:
    # metadata and refs

  fuseki:
    # Sculpin semantic integration

  fluree-reference:
    # test profile only
    # pinned fluree/server image

  toxiproxy:
    # failure-injection profile

  test-runner:
    # deterministic scenario runner
```

Use Compose profiles:

```text
default
integration
differential
stress
fault
```

so the Fluree reference service is not started during ordinary runtime use.

---

# 82. Performance harness architecture

The benchmark runner SHOULD execute identical operation sequences where meaningful:

```text
Scenario
   │
   ├──── CognitiveLedgerAdapter
   │
   └──── FlureeAdapter
```

Outputs:

```text
results/
   scenario.json
   cognitive.json
   fluree.json
   host.json
```

The report SHOULD show:

```text
correctness result
latencies
throughput
memory
storage
relative timing
```

but MUST clearly label that the architectures have different scopes.

---

# 83. Deterministic fixtures

The repository SHOULD contain inspectable graph fixtures such as:

```text
materials/
organization/
sensor-network/
proposal-evaluation/
```

Fixtures SHOULD deliberately include:

```text
functional properties
multi-valued properties
named graphs
provenance
SHACL constraints
subclass reasoning
external virtual values
```

This is preferable to benchmarks consisting only of meaningless random RDF triples.

---

# 84. Semantic merge fixture

One standard fixture SHOULD demonstrate why Sculpin's merge layer exceeds structural merging.

Base:

```turtle
ex:material ex:temperature 80 .
```

Branch A:

```turtle
ex:material ex:temperature 85 .
```

Branch B:

```turtle
ex:material ex:temperature 90 .
```

Shape:

```text
temperature maxCount 1
```

Expected:

```text
structural collision = true
union merge          = RDF-valid
SHACL-valid          = false
```

This MUST be an end-to-end regression test.

---

# 85. External-context fixture

Base graph:

```text
pump P hasMaximumPressure 100
```

Virtual A-box:

```text
currentPressure = 110
```

Candidate rule/knowledge update is evaluated with temporary external data.

Test MUST demonstrate:

```text
virtual A-box participates in validation
virtual observation is not persisted
validation record identifies external context
ledger patch remains unchanged
```

---

# 86. Repository architecture

Recommended Rust workspace:

```text
cognitive-ledger/
├── crates/
│   ├── ledger-core/
│   ├── ledger-rdf/
│   ├── ledger-store/
│   ├── ledger-dag/
│   ├── ledger-merge/
│   ├── ledger-validation/
│   ├── ledger-projection/
│   ├── ledger-api/
│   └── ledger-testkit/
│
├── apps/
│   └── ledger-server/
│
├── migrations/
├── fixtures/
├── tests/
│   ├── integration/
│   ├── differential/
│   ├── stress/
│   └── fault/
│
├── benchmark/
├── docker/
├── docs/
├── compose.yaml
└── reference-images.lock
```

`ledger-core` MUST remain independent of HTTP, PostgreSQL and Fuseki.

This permits extensive deterministic testing of the actual data model.

---

# 87. Dependency direction

Preferred dependency architecture:

```text
ledger-core
    ↑
ledger-rdf
    ↑
ledger-dag
    ↑
ledger-merge

ledger-store ─────┐
ledger-validation ├─> ledger-api
ledger-projection ┘
                       ↑
                  ledger-server
```

Core model code MUST NOT depend on infrastructure adapters.

---

# 88. Migration discipline

PostgreSQL schema migrations MUST be:

```text
ordered
immutable once released
automatically tested
backward recovery tested
```

Changes to commit serialization require a new explicit format identifier:

```text
sculpin-cognitive-commit/v1
sculpin-cognitive-commit/v2
```

Old commit formats MUST remain readable.

Their identifiers MUST remain unchanged.

---

# 89. Compatibility rule

A future service version MUST be able to verify historical commit IDs produced by older versions.

This is a hard long-term requirement.

Canonicalization rules are therefore effectively a persistent public protocol.

They require stronger review than normal implementation code.

---

# 90. Initial implementation milestones

## Milestone 1 — Immutable linear ledger

Deliver:

```text
canonical RDF patch
content-addressed patch
content-addressed commit
single main ref
CAS transactions
history
state reconstruction
PostgreSQL
filesystem object store
```

Gate:

```text
QG-0 through QG-5
```

---

## Milestone 2 — Sculpin integration

Deliver:

```text
Fuseki projection
SHACL adapter
reasoning adapter
virtual A-box validation
validation records
```

Gate:

```text
QG-8
QG-10
```

---

## Milestone 3 — Branching

Deliver:

```text
branch creation
historical branch
branch deletion
ref audit history
branch policies
```

Add Fluree differential tests.

---

## Milestone 4 — Merge

Deliver:

```text
common ancestor
three-way diff
fast-forward
divergent merge
structural collision
merge preview
semantic conflict detection
```

This is the first complete cognitive-feedback workflow.

---

## Milestone 5 — Operational hardening

Deliver:

```text
S3-compatible object storage
checkpoints
garbage collection
metrics
OpenTelemetry
OIDC
rate/resource limits
fault recovery
```

---

## Milestone 6 — Production gate

Deliver:

```text
stress suite
fuzzing
SBOM
security scans
performance baselines
upgrade tests
operational documentation
backup/restore
```

---

# 91. Definition of MVP success

The project should consider the architecture validated when the following scenario works reliably:

```text
1. Sculpin has accepted state C100.

2. An AI agent proposes new knowledge.

3. Cognitive Ledger creates branch agent/x at C100.

4. Agent commits C101a.

5. Human provides a separate correction,
   creating C101b on another branch.

6. The ledger preserves both histories.

7. Merge preview identifies structural differences.

8. Sculpin combines candidate information with
   ontology + SHACL + virtual A-box.

9. Semantic validation determines whether the
   proposed merged state is acceptable.

10. Reviewer accepts the merge.

11. main advances to resulting commit M102.

12. Fuseki projection asynchronously reaches M102.

13. Sculpin can subsequently explain:
    - what changed;
    - who proposed it;
    - which source supported it;
    - which competing proposal existed;
    - which semantic rules were evaluated;
    - why the accepted state was selected;
    - what the graph looked like before the change.
```

If this works while satisfying the invariants and quality gates above, the Cognitive Ledger provides the core functionality required for an evolving Sculpin cognitive layer.

---

# 92. Architectural conclusion

The Cognitive Ledger should deliberately remain **smaller than a graph database**.

Its core responsibility can be summarized as:

```text
immutable RDF changes
        +
content-addressed commit DAG
        +
mutable CAS-protected refs
        +
provenance
        +
historical reconstruction
        +
three-way merging
        +
Sculpin semantic validation
```

The most important boundary is:

```text
Cognitive Ledger
    owns evolution and history

Sculpin/Jena
    owns semantics and validation

Fuseki
    owns queryable materialized state

Virtual A-box
    owns transient external context
```

This boundary makes a Sculpin-specific implementation substantially more tractable than reproducing Fluree while preserving the architectural mechanisms that make Fluree useful as inspiration and as a differential reference implementation.