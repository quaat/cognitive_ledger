Called tool
Called tool
Called tool
Called tool

Yes. For the Sculpin use case, I think this is very feasible, provided the goal is explicitly not to reproduce Fluree as a database.

After looking at the current Fluree repository and implementation, the interesting part for Sculpin is surprisingly compact compared with the whole product. Fluree now combines a custom RDF/SPARQL engine, temporal storage, branching/merging, indexing, search, reasoning, SHACL, security policies, replication, consensus, geospatial support, document ingestion and more. Its repository reflects that breadth with a large Rust workspace of specialized crates.

For Sculpin, I would extract only the architectural idea of a content-addressed, immutable DAG of graph changes with mutable branch references and leave RDF query execution, reasoning and validation with Jena/Fuseki.

The license issue is real, but slightly nuanced

The current Fluree DB license is BUSL-1.1. Interestingly, its Additional Use Grant explicitly permits use as an internal component of another application or service provided the application is not exposing a substantial portion of Fluree itself as a database/graph-database/ledger/query service. It prohibits use as a commercial "Database Service", and derivative copies remain under the BSL until the relevant version's four-year change date, after which it converts to Apache 2.0.

That means there are probably Sculpin deployment models where embedding Fluree could be permissible. But for a commercial product that may be deployed at customers, redistributed, exposed through APIs, or eventually sold independently, I would still regard this as an unnecessary licensing dependency unless a legal review confirms the intended deployment model.

More importantly, Sculpin does not need most of Fluree.

There is a strong case for an independently implemented component based on generic concepts and standards. I would treat Fluree as architectural prior art, not as source code to port. Define the Sculpin design from first principles and public standards and avoid copying Fluree implementation code into the new component.

What I would build

I would provisionally call the component Sculpin Cognitive Ledger.

Its responsibility would be narrow:

Maintain immutable, versioned histories of asserted RDF knowledge, with provenance, branching, merging, time travel and auditable feedback, and materialize any chosen state as RDF for Sculpin's existing knowledge stack.

Conceptually:

                         Sculpin Agents
                              │
                              │ proposed knowledge change
                              ▼
                    ┌─────────────────────┐
                    │ Cognitive Ledger    │
                    │                     │
                    │ commits / DAG       │
                    │ branches / refs     │
                    │ provenance          │
                    │ temporal history    │
                    │ diff / merge        │
                    └─────────┬───────────┘
                              │
                       candidate RDF state
                              │
                              ▼
                    ┌─────────────────────┐
                    │ Sculpin KG layer    │
                    │                     │
                    │ Jena / Fuseki       │
                    │ OWL/RDFS reasoning  │
                    │ SHACL validation    │
                    │ virtual A-box       │
                    └─────────┬───────────┘
                              │
                   accepted / rejected state
                              │
                              ▼
                         new commit

This is an important separation. The ledger doesn't reason. It knows that commit C follows B, that it added these quads and removed those quads, who proposed the update, why it was proposed, what evidence supported it, and whether Sculpin validated it.

Sculpin remains responsible for determining what those statements mean.

That fits the existing architecture particularly well because reasoning, SHACL and the virtual A-box remain usable without forcing transient external data into the permanent cognitive history.

The core object model can be very small

A commit could conceptually look like:

Commit
 ├── id                 SHA-256 / content ID
 ├── parents[]          zero, one or multiple parent commit IDs
 ├── patch              reference to immutable RDF changes
 ├── author             user / agent / service
 ├── event_time         when the represented event occurred
 ├── recorded_at        when Sculpin learned/recorded it
 ├── ontology_ref       ontology/schema version used
 ├── provenance         sources/evidence/activity
 ├── message            human-readable intent
 ├── validation         SHACL/reasoning result metadata
 └── metadata           application-specific annotations

A normal commit has one parent:

A ── B ── C ── D

A branch is simply another reference to a commit:

              E ── F
             /
A ── B ── C ── D
             \
              G

A merge produces a commit with two parents:

              E ── F ──────┐
             /              │
A ── B ── C                  M
             \              │
              G ────────────┘

This is essentially the feature Sculpin needs from Fluree. Fluree itself currently uses content-addressed commit heads, common-ancestor traversal, branches and compare-and-set updates of those heads; its public implementation documentation explicitly describes common-ancestor lookup, fast-forward determination and CAS updates of commit-head references.

Do not put branch names into commits

Branches should be lightweight mutable references:

main       -> sha256:a3...
agent-42   -> sha256:c7...
experiment -> sha256:f1...

The immutable commit DAG exists independently.

That is one of the most valuable ideas to retain from Git and Fluree.

RDF Patch is almost tailor-made for the delta representation

There is no reason to invent a Sculpin-specific RDF delta syntax.

Apache Jena already defines RDF Patch, representing atomic additions and deletions of triples/quads and prefixes:

A <alice> <role> "Engineer" .
D <alice> <role> "Researcher" .

RDF Patch even defines metadata for patch IDs and references to a previous patch, explicitly noting that these can form a log of changes.

I would therefore either use RDF Patch directly or use a very thin binary/internal representation with an RDF Patch import/export API.

For example:

Commit
   │
   ├── parents [C41]
   │
   └── patch P42
          ├── DELETE <sensor7> <status> "unknown"
          └── ADD    <sensor7> <status> "operational"

The Commit is the DAG object. The RDFPatch is merely its payload.

That also gives excellent interoperability with Jena.

One thing I would improve over a naive Git model: separate "change" from "state"

Git commits contain a snapshot tree, not merely a textual diff. That avoids ambiguity when traversing merge histories.

For Sculpin I would use:

Commit
    parents[]
    patch
    state_digest

but not necessarily store a complete RDF dataset for every commit.

The state can be:

nearest checkpoint
        +
subsequent patches
        =
requested state

Periodically create a materialized checkpoint:

C100  ─ C101 ─ C102 ─ C103 ─ ... ─ C150
  │                              │
snapshot                      snapshot

Historical reconstruction then does not require replaying thousands of changes.

This is conceptually similar to Fluree's separation between commit history and indexed snapshots, but the Sculpin version can be dramatically simpler because Fuseki remains the actual RDF query engine.

RDF canonicalization deserves special attention

Content addressing sounds straightforward:

commit_id = SHA256(commit)

but RDF blank nodes make hashing RDF datasets surprisingly difficult.

The W3C now has the RDFC-1.0 RDF Dataset Canonicalization Recommendation, specifically intended to allow RDF datasets to be compared, hashed and digitally signed despite blank-node identifiers.

I would nevertheless avoid full-dataset canonicalization on every commit.

For the cognitive layer I would strongly prefer:

anonymous RDF node
        ↓ ingress
stable Sculpin skolem IRI

for versioned entities.

Then canonicalize and sort the patch representation before hashing the commit envelope.

RDFC-1.0 can still be useful for imports, snapshot verification and situations where genuine blank nodes must be retained. The W3C specification itself warns that pathological blank-node structures can make canonicalization expensive.

Where this becomes specifically useful for cognitive Sculpin

Suppose an agent currently knows:

:MaterialA :recommendedTemperature "80" .

An engineer tells it:

For supplier X's material this should actually be 90 °C.

Rather than directly modifying Fuseki:

80 -> 90

Sculpin could create:

main
  │
  C381
  │
  └──── feedback/thomas
           │
           C382

C382 records:

DELETE:
:MaterialA :recommendedTemperature "80" .

ADD:
:MaterialA :recommendedTemperature "90" .

along with:

author       = user:...
source       = feedback-session:...
reason       = "Supplier X material specification"
recordedAt   = ...
eventTime    = ...
confidence   = ...

Sculpin then materializes the candidate state and asks Jena:

candidate KG
    +
ontology
    +
relevant virtual A-box
       │
       ├── reasoning
       └── SHACL

If validation succeeds, that commit can be accepted/merged.

If validation discovers:

recommendedTemperature maxCount 1

and another branch independently proposed 85, Sculpin has a semantic merge conflict.

This is much more useful than ordinary Git conflict detection.

Let Jena determine semantic conflicts

This is where I would intentionally differ from trying to reproduce all of Fluree.

The ledger can detect structural conflicts cheaply:

Branch A:
:s :p "A"

Branch B:
:s :p "B"

That could be flagged because both branches changed (subject, predicate, graph) differently.

But RDF itself permits multiple objects:

:s :p "A", "B" .

Whether that is actually a conflict depends on the ontology and SHACL model.

Therefore:

DAG merge
   │
   ▼
candidate RDF graph
   │
   ├── syntactic/structural conflict detection
   │
   ▼
Sculpin semantic validation
   │
   ├── SHACL
   ├── OWL/RDFS
   └── domain rules

That is a significant advantage of integrating the version store with Sculpin rather than attempting to make it another general-purpose graph database.

Provenance should be first-class

I would make the following distinction fundamental:

Concept	Meaning
Commit provenance	Why the graph changed
Fact provenance	Why a particular assertion is believed
Source provenance	Where evidence originates
Agent provenance	Which human/AI/system proposed something
Validation provenance	Which ontology/shapes/reasoner accepted it
Event time	When something happened in the domain
Recorded time	When Sculpin learned it

The last distinction is particularly important for an adaptive cognitive system.

Consider:

2026-09-01: valve failed
2026-09-10: engineer discovers failure
2026-09-24: data imported into Sculpin

Those are three different things.

A cognitive system should be able to answer both:

What did we believe on September 5?

and

What do we now know was true on September 5?

Fluree has evolved similar temporal distinctions in its temporal architecture; Sculpin should retain the concept even if its implementation is much smaller. Fluree's current public interface supports historical addressing by transaction/commit/time and treats immutable temporal history as a fundamental feature.

I would not implement Git rebase as a primary cognitive operation

This is one area where the cognitive use case is different from software development.

For code:

rebase

is very convenient.

For provenance:

rewrite history

is often undesirable.

For Sculpin I would make merge the normal operation:

agent branch ─────┐
                  ├── merge commit
reviewer branch ──┘

The history shows exactly what happened.

A "rebase" operation could eventually mean replay these proposals against a newer base, but the original commits should remain addressable. Nothing should actually disappear from the audit DAG.

Similarly, force-moving a branch should itself generate an audit event.

A pragmatic implementation

I would use roughly this division:

Component	Recommendation
Core implementation	Rust
Public/internal API	HTTP/JSON + OpenAPI
RDF changes	RDF Patch / canonical N-Quads
Commit ID	SHA-256 content address
Commit metadata	CBOR or deterministic JSON
Metadata/ref database	PostgreSQL
Large immutable objects	Filesystem initially; S3-compatible abstraction
Branch concurrency	Compare-and-set on HEAD
RDF execution	Existing Fuseki/Jena
Validation	Existing SHACL
Reasoning	Existing Jena reasoning
External dynamic data	Existing virtual A-box
Auth	Sculpin/OIDC identity passed into commit provenance

PostgreSQL makes the mutable part almost trivial.

For example, conceptually:

UPDATE graph_refs
SET head = :new_head
WHERE graph_id = :graph
  AND branch = :branch
  AND head = :expected_head;

Zero rows updated means somebody else changed the branch.

That single invariant avoids a huge class of concurrency bugs.

Everything else is immutable.

The API could remain extremely small

A first production API doesn't need SPARQL at all. Sculpin already has SPARQL.

Something like:

POST /graphs/{graph}/commits
GET  /graphs/{graph}/commits/{id}

GET  /graphs/{graph}/branches
POST /graphs/{graph}/branches

GET  /graphs/{graph}/state/{ref}
GET  /graphs/{graph}/diff/{a}/{b}

POST /graphs/{graph}/merge
GET  /graphs/{graph}/history

GET  /graphs/{graph}/refs/{branch}

The critical transaction call would take:

expectedHead
patch
author
provenance
message
eventTime

and return either:

201 commit created

or:

409 head changed

No query language. No custom reasoner. No custom SHACL implementation.

That is what keeps the project manageable.

What I would explicitly leave out

This is the key scope boundary.

Fluree capability	Sculpin ledger
Content-addressed commits	Yes
Multi-parent commit DAG	Yes
Branches	Yes
Historical states	Yes
Diff	Yes
Three-way merge	Yes
Provenance	Yes, strongly
Event/recorded time	Yes
Signatures	Later
SPARQL engine	No
RDF indexes	No
OWL reasoning	No
SHACL engine	No
BM25	No
HNSW vectors	No
GeoSPARQL engine	No
Graph access-control engine	No
R2RML/Iceberg engine	No
Raft consensus	Initially no
Multi-node replication	Later if actually required
Cypher	No
GraphQL	No
MCP	Sculpin already provides the higher-level tool surface

This is why I think the project is realistic.

Fluree is difficult because it is a database.

The proposed component is fundamentally an immutable event/version store for RDF state.

A sensible development sequence

I would deliberately resist implementing all Git functionality initially.

Define invariants first. Specify commit encoding, hash calculation, RDF blank-node policy, parent semantics, HEAD CAS semantics and provenance model. Build property-based tests around these because changing them later would invalidate stored history.
Implement linear history. Immutable RDF Patch commits, main HEAD, checkout/materialization, history and diff. Integrate with Sculpin/Jena immediately so the architecture is proven before branching complexity arrives.
Add branches. Branches should only be mutable refs to existing commits. Test simultaneous writers heavily.
Add candidate validation. Materialize base + patch + virtual A-box, run Jena reasoning/SHACL and store the validation result in commit metadata.
Add DAG merge. Common-ancestor discovery, three-way RDF set diff, structural conflict detection and semantic validation through Sculpin.
Add cognitive workflow semantics. Agent/user feedback branches, review/accept/reject workflows, confidence and evidence provenance, and explicit distinction between proposals and accepted knowledge.
Only then add operational optimizations. Checkpoints, history indexes, S3 storage, signing, replication and possibly distributed coordination.

That sequence means that quite early you have something genuinely useful to Sculpin rather than a half-built graph database.

The most important architectural decision

I would model the system as:

              IMMUTABLE
                  │
           Commit DAG
        ┌─────────┴─────────┐
        │                   │
      patches           provenance
        │
        │
        ▼
   historical state


               MUTABLE
                  │
                 refs
          ┌───────┼─────────┐
          │       │         │
        main    user/x   agent/y

rather than:

versioned database
       +
query engine
       +
reasoner
       +
everything else

The first is a relatively contained piece of infrastructure.

The second is rebuilding Fluree.

Overall assessment

I would therefore rate the approach highly feasible and architecturally appropriate for Sculpin.

The strongest design, in my view, is not "Sculpin's own Fluree." It is:

A small Git-like, content-addressed RDF change ledger underneath Sculpin, responsible only for evolving knowledge and its history, while Sculpin's existing Jena/Fuseki layer continues to provide semantics, reasoning, validation and federated/virtual data access.

That actually gives Sculpin an interesting property that Fluree cannot provide in exactly the same way: merges can be judged using the domain ontology itself. Instead of merely asking whether two RDF patches conflict structurally, Sculpin can ask whether the merged cognitive state remains logically and structurally valid according to the domain model.

And the Apache Jena RDF Patch format plus the W3C RDFC-1.0 standard mean that two of the otherwise awkward foundations—portable RDF deltas and deterministic RDF identity—already have standards-based starting points.

If this direction is pursued, I would make the next design artifact a proper technical specification for the Cognitive Ledger, defining the commit object, hashing/canonicalization rules, storage schema, branch/merge semantics, provenance ontology, REST API and exact integration protocol with Fuseki/virtual A-box. That specification would be sufficiently constrained that an implementation agent could build it incrementally without drifting into creating another general-purpose graph database.

This should very reasonable. Create a proper high quality detailed technical specification for the Cognitive Ledger along with quality gates. For an automatic integration testing, consider using a docker container based on the fluree/server docker image for side-by-side comparisons of the relevant features, along with timings and stress tests (if this is appropriate).

Below is an implementation-grade specification. I’ve treated the Fluree container as a differential reference implementation, not a dependency or performance target: correctness comparisons are valuable, while raw performance comparisons should remain primarily diagnostic because Fluree is a full optimized graph database and the Cognitive Ledger deliberately is not. The official fluree/server image is well suited to this role: it exposes a health endpoint on port 8090 and is intended to run as a self-contained Docker service.

Sculpin Cognitive Ledger — Technical Specification
Sculpin Cognitive Ledger
Technical Architecture and Implementation Specification

Status: Draft specification 0.1
Target: Sculpin cognitive/adaptive knowledge layer
Primary implementation language: Rust
Architecture: Independent containerized service
Normative terminology: MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY indicate requirement strength.

1. Purpose

The Cognitive Ledger provides Sculpin with a persistent, immutable and auditable mechanism for allowing knowledge to evolve over time.

It is responsible for:

recording graph changes as immutable commits;
preserving complete knowledge history;
representing commit ancestry as a directed acyclic graph;
supporting branches and concurrent proposals;
supporting deterministic comparison and merge of graph states;
recording why, when and by whom knowledge changed;
distinguishing observed/event time from recording time;
allowing historical graph states to be reconstructed;
supporting human and agent feedback without immediately mutating accepted knowledge;
enabling validation of proposed states using Sculpin's existing semantic capabilities;
providing durable provenance around accepted and rejected changes.

The Cognitive Ledger is not a graph database replacement.

Apache Jena/Fuseki remains responsible for:

SPARQL querying;
RDF storage optimized for query;
OWL/RDFS reasoning;
SHACL validation;
ontology management;
Sculpin's virtual A-box and graph-source capabilities.

The ledger is the system of record for graph evolution. Fuseki is a queryable projection and semantic processing environment.

2. Design principle

The central architecture is:

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

The Cognitive Ledger MUST know how knowledge changed.

Sculpin MUST remain responsible for determining what that knowledge means.

3. Explicit non-goals

Version 1 MUST NOT attempt to implement:

a SPARQL engine;
a general RDF database;
OWL reasoning;
RDFS reasoning;
SHACL execution;
full-text search;
vector search;
GeoSPARQL;
Cypher;
GraphQL;
triple-level authorization;
distributed consensus;
generic database replication;
general-purpose event streaming;
arbitrary Git compatibility;
Git-style history rewriting.

These capabilities would move the implementation toward recreating Fluree rather than implementing the Sculpin-specific requirement.

4. Architectural inspiration

Fluree provides useful architectural precedents for:

immutable transactions;
content-addressed commits;
commit ancestry;
historical state;
branches;
fast-forward merges;
divergent merges;
common-ancestor calculation;
compare-and-set updates of branch heads.

Current Fluree branching supports branches from historical commits, isolated branch evolution, fast-forward and divergent merges, and conflict detection when branches modify the same (subject, predicate, graph) combination.

Fluree also separates immutable commit history from optimized indexed state, conceptually similar to the checkpoint architecture specified below.

The Cognitive Ledger SHOULD reproduce the relevant semantics, but MUST NOT reproduce Fluree source code or unnecessarily replicate its database architecture.

5. Fundamental invariants

The following are hard system invariants.

INV-1 — Commit immutability

Once a commit ID has been created, its contents MUST never change.

INV-2 — Content identity

A commit ID MUST be derived deterministically from its canonical serialized content.

Changing any content-addressed field MUST produce a different commit ID.

INV-3 — Parent existence

Every parent referenced by a commit MUST exist before the commit can become reachable from a branch.

INV-4 — Acyclic ancestry

The commit structure MUST remain a DAG.

A commit cannot directly or indirectly reference itself.

INV-5 — Branch refs are mutable; commits are not

A branch is a mutable reference to an immutable commit.

main -> C42

Advancing main to C43 MUST NOT mutate C42.

INV-6 — Optimistic branch concurrency

Every branch update MUST use compare-and-set semantics.

A client attempting:

expected = C42
new      = C43

MUST fail if the current branch head is no longer C42.

INV-7 — Referential integrity

A branch MUST never reference a nonexistent commit.

INV-8 — Deterministic state reconstruction

Reconstructing the same commit from the same stored objects MUST always produce an RDF-isomorphic state.

Because persisted Cognitive Ledger data will avoid blank nodes, the expected result SHOULD normally be byte-equivalent canonical N-Quads.

INV-9 — No silent history rewriting

Previously created commits MUST remain addressable even when:

branches advance;
branches merge;
branches are deleted;
proposals are rejected.
INV-10 — Projection independence

Failure of Fuseki or another downstream projection MUST NOT corrupt or invalidate committed ledger history.

6. RDF data model
6.1 RDF dataset

The logical state of a Cognitive Ledger graph is an RDF dataset, not merely one RDF graph.

A state consists of:

default graph
+
zero or more named graphs

Each persisted statement is therefore represented as a quad:

subject
predicate
object
graph

The default graph uses an explicit internal sentinel.

7. Blank-node policy

Blank nodes are problematic for:

stable identity;
patching;
hashing;
comparison;
merging;
provenance.

W3C RDFC-1.0 provides a standardized mechanism for canonicalizing RDF datasets and assigning deterministic blank-node identifiers. It became a W3C Recommendation in 2024.

However, canonicalization itself can be computationally expensive for pathological blank-node structures.

Therefore:

Persistent Cognitive Ledger states MUST NOT contain anonymous blank nodes.

At ledger ingress, blank nodes MUST either:

be deterministically or securely skolemized into Sculpin-controlled IRIs; or
cause the transaction to be rejected when safe skolemization cannot be guaranteed.

Example:

_:b42

may become:

urn:sculpin:node:01K...

RDFC-1.0 SHOULD still be implemented as an import/interoperability capability for:

comparing externally supplied RDF datasets;
snapshot verification;
interoperability tests;
data where blank nodes cannot be avoided before ingest.

Transient virtual A-box graphs MAY contain blank nodes because they are not committed to ledger history.

8. Patch representation

Apache Jena RDF Patch already defines atomic RDF dataset changes containing:

additions;
deletions;
triples;
quads;
transaction boundaries;
patch metadata.

It also explicitly supports patch identifiers and predecessor references.

The Cognitive Ledger SHOULD therefore use an RDF Patch-compatible model.

8.1 Canonical Cognitive Patch

Internally, a patch consists of:

deletions[]
additions[]

where each entry is a normalized RDF quad.

The canonical serialization MUST:

normalize RDF terms;
reject persisted blank nodes;
eliminate duplicate operations;
reject or explicitly resolve contradictory operations;
lexicographically sort deletions;
lexicographically sort additions;
serialize terms using canonical N-Quads lexical representation;
use LF line endings;
use UTF-8.

Illustratively:

D <urn:x> <urn:status> "unknown" <urn:graph:state> .
A <urn:x> <urn:status> "operational" <urn:graph:state> .

Prefix declarations MUST NOT affect semantic patch identity.

Prefixes are presentation metadata, not RDF dataset semantics.

9. Patch identity

Every patch receives:

patch_id = sha256(canonical_patch_bytes)

represented externally as:

sha256:<64-lowercase-hex-characters>

Hash algorithm names MUST be explicit to permit future algorithm migration.

10. Commit model

A commit represents one immutable cognitive change.

Conceptually:

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
11. Commit canonicalization

Commit metadata SHOULD use JSON because it is:

easy to inspect;
easy to debug;
language-independent;
readily exposed through REST APIs.

Content hashing MUST use a deterministic serialization.

JSON Canonicalization Scheme, RFC 8785, defines a deterministic property ordering and serialization intended specifically for repeatable hashing and signing.

Therefore:

canonical_commit = JCS(commit_without_id)
commit_id        = SHA256(canonical_commit)

The commit ID MUST NOT be included in its own hash input.

Fields whose values cannot be represented safely within JCS numeric constraints SHOULD be represented as strings.

12. Parent semantics
12.1 Genesis commit

A genesis commit has:

parents = []
12.2 Ordinary commit

An ordinary commit has exactly one parent:

parents = [previous_head]
12.3 Merge commit

A merge commit has two parents:

parents = [
    target_head,
    source_head
]

Parent ordering is significant.

The first parent defines the state against which the merge patch is applied.

This produces Git-like first-parent history:

target ------ M
             /
source ------+

The state of M is therefore:

state(target_head)
+
merge_patch

The second parent represents ancestry and provenance but is not necessary for materializing the resulting graph state.

Version 1 MUST restrict commits to at most two parents.

13. State model

A commit is not required to contain a complete snapshot.

Its state is logically:

state(parent[0]) + patch

For genesis:

empty RDF dataset + patch

This permits compact history.

14. Checkpoints

Replaying all commits from genesis eventually becomes inefficient.

The ledger MUST therefore support derived checkpoint objects.

checkpoint
    commit_id
    canonical RDF dataset
    digest
    compression
    created_at

Checkpoint data SHOULD be canonical N-Quads compressed using Zstandard.

A checkpoint is derived data and MUST NOT affect the identity of the commit it represents.

Checkpoint creation MAY occur according to:

number of commits since the last checkpoint;
accumulated patch bytes;
reconstructed state size;
expensive merge;
administrator request.

Suggested initial defaults:

100 commits
or
64 MiB accumulated patch content

These values are operational defaults, not protocol requirements.

A merge commit MAY trigger an immediate checkpoint when the resulting state is expensive to reconstruct.

15. State reconstruction

Given commit C:

walk first-parent ancestry toward genesis;
locate the nearest checkpoint;
load the checkpoint;
replay patches forward;
optionally calculate a resulting state digest;
return or stream the RDF dataset.

The implementation SHOULD cache recently reconstructed states.

16. State digest

A state digest is useful for:

integrity testing;
cross-system comparison;
checkpoint validation;
differential testing.

For blank-node-free state:

digest =
    SHA256(
        lexicographically sorted canonical N-Quads
    )

State digests SHOULD be stored with checkpoints.

They SHOULD NOT initially form part of commit identity because calculating a complete dataset digest on every commit may be unnecessarily expensive.

A future incremental Merkle-set implementation MAY make state roots practical per commit.

17. Branch model

A branch is:

(graph_id, branch_name) -> commit_id

Examples:

main
feedback/thomas
agent/evaluation-17
experiment/new-rule
review/committee-a

main SHOULD be the default accepted-knowledge branch.

Branch names MUST be validated and bounded in length.

18. Branch creation

A branch MAY be created from:

another branch HEAD;
any reachable historical commit;
genesis.

Creating a branch MUST NOT copy commit objects.

It only creates a new ref:

feedback/user -> C100
19. Branch ref storage

A relational representation is recommended:

graph_id
branch
head_commit_id
version
updated_at
updated_by

Branch advancement MUST use an atomic compare-and-set update.

Conceptually:

UPDATE refs
SET
    head_commit_id = :new_head,
    version = version + 1
WHERE graph_id = :graph
  AND branch = :branch
  AND head_commit_id = :expected_head;

If zero rows are changed:

HTTP 409 Conflict

MUST be returned.

20. Ref event history

Every ref change MUST generate an immutable ref event:

RefEvent
    graph
    branch
    old_head
    new_head
    principal
    timestamp
    operation
    reason

Operations include:

create
advance
fast_forward
merge
reset
delete
restore

This permits auditing branch movement without encoding branch identity into commits.

21. Temporal model

The system MUST distinguish at least:

eventTime

When the represented real-world event occurred.

recordedAt

When the Cognitive Ledger recorded the information.

Example:

eventTime:
2026-08-12T08:00:00Z

recordedAt:
2026-09-24T14:37:00Z

The distinction allows Sculpin to answer both:

What did we know on 20 August?

and:

What do we now know was true on 20 August?

Fluree similarly distinguishes historical transaction/recording semantics from event-time semantics.

recordedAt MUST be server controlled.

eventTime MAY come from trusted source data and MUST be preserved as provenance rather than blindly interpreted as commit ordering.

Unlike Fluree's linear transaction sequence, Cognitive Ledger commits on independent branches MAY have overlapping event times.

Commit ancestry—not wall-clock time—is authoritative for graph evolution.

22. Provenance

Provenance is a primary requirement rather than auxiliary metadata.

W3C PROV-O SHOULD be used as the interoperability model for provenance export. PROV-O is a W3C Recommendation designed specifically for representing entities, activities, agents and derivation relationships.

The ledger MUST distinguish:

Provenance dimension	Meaning
Commit provenance	Why a graph state changed
Actor provenance	Human, agent or system responsible
Source provenance	Evidence supporting the change
Validation provenance	Semantic rules used to evaluate it
Temporal provenance	Event time vs recording time
Decision provenance	Why a proposal was accepted or rejected

Commit metadata SHOULD include:

authenticated actor
activity type
message
evidence references
event time
source system
agent identifier/version
correlation ID
23. Fact-level provenance

The ledger MUST NOT prescribe one exclusive method for modeling provenance of individual facts.

Sculpin MAY use:

named graphs;
PROV-O;
explicit claim resources;
statement annotation mechanisms.

The Cognitive Ledger versions those RDF statements like any other information.

Commit provenance and fact provenance are distinct.

24. Authentication and actor identity

Commit author identity MUST NOT come from an arbitrary client-supplied string.

The service MUST obtain the principal from authenticated context such as:

OIDC access token;
service identity;
trusted internal workload identity.

The authenticated principal becomes commit actor metadata.

An optional onBehalfOf identity MAY be supported where policy permits delegation.

25. Proposal workflow

The system MUST allow graph changes to exist without modifying accepted knowledge.

Example:

                   P1 --- P2
                  /
main --- C99 --- C100

where P1/P2 represent an agent or human proposal branch.

A proposal can then be:

accepted
rejected
superseded
modified
merged

Rejected proposals SHOULD remain auditable unless an explicit retention policy permits their removal.

26. Transaction pipeline

A normal branch transaction SHOULD execute:

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

Steps 13–15 MUST occur within one PostgreSQL transaction.

External semantic validation SHOULD occur before branch advancement.

Because validation may take significant time, the branch HEAD must be checked again during the CAS operation.

If another writer advances the branch during validation:

409 HEAD_CHANGED

is returned.

The immutable candidate commit MAY remain stored as an unattached proposal.

27. Validation records

Semantic validation results MUST NOT be inserted into the commit after hashing.

Instead, validation is a separate immutable object:

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

This avoids circular commit construction and permits the same commit to be evaluated under different semantic contexts.

28. Validation levels

The following validation levels SHOULD exist:

Level 0 — Syntax
valid RDF;
valid literals;
supported RDF terms;
no forbidden blank nodes;
valid IRIs.
Level 1 — Patch consistency
deletions applicable to base state;
contradictory operations rejected;
operation size limits respected.
Level 2 — SHACL

Sculpin's configured SHACL validation.

Level 3 — Reasoning

Configured Jena reasoning/domain-rule checks.

Level 4 — External context

Validation incorporating virtual A-box/external data.

Branch policies determine required levels.

For example:

main:
    require syntax
    require patch consistency
    require SHACL
    require reasoning

experiment/*:
    require syntax only
29. Virtual A-box integration

External data incorporated through Sculpin's virtual A-box MUST remain transient unless explicitly committed.

Validation may conceptually use:

candidate ledger state
+
ontology
+
SHACL
+
temporary external A-box

The external graph SHOULD NOT automatically become part of the commit patch.

The validation record MUST identify the external context used as precisely as reasonably possible:

source URI
snapshot/version ID
query parameters
dataset version
object hash
retrieval timestamp

Where a source provides no version identifier, Sculpin SHOULD calculate a digest of the relevant materialized external data used during validation when practical.

30. Accepted-state projection

The Cognitive Ledger is the authoritative change history.

Fuseki is a projection.

After a successful branch advance:

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

The ledger MUST maintain:

projection_head

for each configured projection.

31. Projection consistency

Projection updates MUST be idempotent.

The projector MUST tolerate:

process crashes;
duplicate events;
network errors;
Fuseki restarts;
retry after partial failure.

The projection MUST store or otherwise verify which commit it currently represents.

If Fuseki is behind:

ledger head     = C105
projection head = C102

the service MUST expose this lag through health/status/metrics.

32. Projection failure

A failed projection MUST NOT roll back committed ledger history.

Instead:

ledger = authoritative
projection = degraded

The projector retries from its last successful commit.

This is essential for avoiding distributed transactions between PostgreSQL/object storage and Fuseki.

33. Merge model

Merging MUST use three-way merge semantics.

Given:

                 source S
                /
ancestor A ----+
                \
                 target T

the system computes:

delta_source = A -> S
delta_target = A -> T

and produces candidate merged state M.

34. Common ancestor

The implementation MUST correctly identify a lowest/common reachable ancestor of the source and target DAG histories.

The algorithm MUST:

support merge commits;
maintain a visited set;
terminate on malformed data;
be bounded by configurable traversal limits;
provide an explicit error when limits are exceeded.
35. Fast-forward merge

If:

T is an ancestor of S

then:

target ref T -> S

is sufficient.

No new commit is required.

A ref event MUST record that a fast-forward merge occurred.

36. Already-contained merge

If:

S is an ancestor of T

the merge is a semantic no-op.

The API SHOULD return:

already_contained = true

and MUST NOT generate a meaningless commit.

37. Divergent merge

For divergent histories, the merged state is calculated and represented by a new merge commit:

parents = [target_head, source_head]
patch   = target_state -> merged_state
38. Structural conflicts

Version 1 SHOULD use the same coarse conflict key as Fluree for differential compatibility:

(graph, subject, predicate)

A collision exists where both branches independently modify the resulting object-set for the same key.

This is deliberately called a structural collision, because RDF allows multiple objects for one predicate.

Example:

target:
:x :temperature 80 .

source:
:x :temperature 90 .

may or may not be invalid according to the ontology.

39. Structural merge strategies

Supported strategies SHOULD initially be:

abort
take-target
take-source
union

abort SHOULD be the default for protected branches such as main.

union MUST NOT imply semantic validity.

Every resolved candidate MUST still pass configured semantic validation.

40. Semantic conflicts

After structural combination:

merged candidate
       │
       ▼
Sculpin semantic validation

may identify conflicts such as:

SHACL maxCount violation
datatype violation
closed-shape violation
ontology inconsistency
domain-specific rule violation
external-data contradiction

This allows Sculpin to distinguish:

structural collision

from:

semantic conflict

This is a central capability of the Cognitive Ledger.

41. Merge preview

Merge SHOULD be a two-stage operation.

Stage 1
POST /merge/preview

returns:

common ancestor
source delta
target delta
structural collisions
proposed resolutions
semantic validation
preview token
Stage 2
POST /merge/apply

takes the preview token.

The token MUST cryptographically bind or identify:

source head
target head
ancestor
resolution policy
candidate result

If either head changed after preview:

409 MERGE_STALE

MUST be returned.

This avoids time-of-check/time-of-use errors.

42. Rebase

Git-style rebase MUST NOT be required in version 1.

Rebase rewrites lineage and is generally undesirable for an audit-focused cognitive knowledge system.

A future replay operation MAY replay proposals against a newer branch state, but the original commits MUST remain addressable.

43. Storage architecture

Recommended deployment:

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
44. PostgreSQL responsibilities

PostgreSQL SHOULD store:

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

Immutable commit and patch contents MAY also be stored in PostgreSQL for small installations, but the architecture SHOULD expose an object-store abstraction.

45. Object store

Required interface:

trait ObjectStore {
    put(content_id, bytes)
    get(content_id)
    exists(content_id)
    delete(content_id)
}

Initial implementations SHOULD include:

filesystem
S3-compatible storage

Object writes MUST be idempotent.

Writing different bytes under an existing content ID MUST be considered corruption and fail hard.

46. Object layout

A filesystem/S3 layout MAY use:

objects/
    sha256/
        ab/
            cd/
                abcdef...

Object type SHOULD be encoded in metadata rather than relying exclusively on directory structure.

47. Change index

The service is not a general RDF query engine, but efficient cognitive history lookup is valuable.

PostgreSQL SHOULD maintain a change index for each patch operation:

commit_id
operation
graph
subject
predicate
object_hash

This supports questions such as:

When did this property change?

without querying complete historical snapshots.

The complete RDF object remains in the immutable patch.

48. API design

Base path:

/v1

Core resources:

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
49. Core API surface

Recommended version-1 operations:

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
50. Transaction request

Example:

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
51. Commit receipt

Successful response:

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
52. Error model

Stable machine-readable error codes MUST be provided.

Examples:

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

HTTP status codes SHOULD be used conventionally:

400 malformed request
401 unauthenticated
403 unauthorized
404 unknown object
409 optimistic concurrency / merge conflict
413 payload too large
422 semantically invalid transaction
503 unavailable dependency
53. Idempotency

Mutation APIs SHOULD support:

Idempotency-Key

Repeated requests with the same authenticated actor, graph and idempotency key MUST either:

return the same result; or
return an explicit idempotency conflict if the payload differs.
54. Resource limits

Configurable limits MUST exist for:

patch bytes
number of operations
metadata bytes
evidence references
branch name length
history traversal depth
merge traversal
state export size
validation duration

This prevents accidental or malicious resource exhaustion.

55. Security model

Version 1 requires graph-level authorization.

Roles MAY include:

reader
contributor
reviewer
maintainer
administrator

Protected branches SHOULD restrict:

direct updates
merge approval
force reset
branch deletion

main SHOULD normally prohibit unaudited force updates.

Triple-level authorization is intentionally outside scope.

56. Signing

Cryptographic signing is not mandatory for the first MVP, but the data model MUST permit it later.

A future signature object SHOULD sign:

commit_id
actor
signature algorithm
key identifier
timestamp

rather than modifying commit content.

57. Garbage collection

The default policy SHOULD prioritize auditability over aggressive deletion.

Objects fall into three categories:

Reachable

Referenced from a branch or protected audit root.

MUST NOT be deleted.

Historical but unreferenced

Previously reachable or intentionally retained proposal history.

SHOULD normally be retained.

Staged/orphaned technical objects

Created during an operation that never produced a registered commit.

MAY be garbage collected after a configurable grace period.

GC MUST use mark-and-sweep semantics from protected roots.

58. Observability

The service MUST expose:

/health
/ready
/metrics

Metrics SHOULD include:

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

OpenTelemetry tracing SHOULD be supported.

A correlation ID SHOULD propagate through:

Sculpin request
→ ledger
→ validator
→ virtual A-box
→ projector
→ Fuseki
59. Docker deployment

A development deployment SHOULD include:

cognitive-ledger
postgres
fuseki

Optional services:

minio
toxiproxy
prometheus

The complete system MUST be runnable through:

docker compose up

without requiring manually installed databases.

60. Differential Fluree test environment

Fluree SHOULD be included as a test-only reference service.

The official fluree/server image currently packages the server as a self-contained container, exposes port 8090, provides /health, and stores data under /var/lib/fluree.

Reference topology:

                    Test Runner
                    /         \
                   /           \
                  ▼             ▼
       Cognitive Ledger        Fluree
             │                   │
        PostgreSQL          Fluree storage
             │
           Fuseki

The Fluree container MUST NOT be part of the Sculpin runtime dependency graph.

It MUST only be used by testing/benchmark profiles.

61. Fluree image pinning

CI MUST NOT use:

fluree/server:latest

despite this being convenient interactively.

Instead CI MUST pin:

version
+
image digest

in a repository-controlled file such as:

test/reference-images.lock

Example conceptual structure:

fluree:
  image: fluree/server
  version: "..."
  digest: "sha256:..."

Updating the reference image MUST be an explicit reviewed change.

Because Fluree uses BSL licensing, the reference container SHOULD be pulled during development/CI rather than redistributed as part of Sculpin artifacts, and the intended CI use SHOULD receive the same legal review applied to other third-party test dependencies.

62. Differential adapter

The integration test framework SHOULD define a common abstract interface:

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

Adapters:

CognitiveLedgerAdapter
FlureeAdapter

The same deterministic scenario can then execute against both systems.

63. What to compare with Fluree

The following semantics SHOULD be tested side by side.

Capability	Differential comparison
Linear transactions	Yes
Assertion/retraction	Yes
Historical state	Yes
Branch isolation	Yes
Branch from HEAD	Yes
Branch from history	Yes
Common ancestor	Yes
Fast-forward merge	Yes
Divergent merge	Yes
(g,s,p) collision	Yes
Repeated merge	Yes
Deleted/re-added facts	Yes
Commit identity	No
Transaction numbers	No
Binary storage format	No
SHACL behavior	Separate test
Reasoner behavior	Separate test
Query performance	Not equivalent
Search/vector functionality	Out of scope

Fluree currently exposes immutable history, branch isolation, historical branch creation and Git-like merging, making these particularly useful reference behaviors.

64. Differential state comparison

Commit identifiers MUST NOT be compared.

Instead:

Cognitive Ledger state
        │
        ▼
canonical N-Quads
        │
        ▼
state digest

and:

Fluree state
        │
        ▼
canonical N-Quads
        │
        ▼
state digest

are compared.

Expected:

digest(cognitive) == digest(fluree)

where the scenario is intended to have equivalent semantics.

When blank nodes appear in reference data, RDFC-1.0 MUST be used before comparison.

65. Differential test scenarios

The deterministic suite MUST include at least:

D01 — Genesis

Create identical initial graph.

D02 — Linear append

Perform 100 sequential additions.

Verify every historical state.

D03 — Retraction

Add and later delete a fact.

Verify state before and after.

D04 — Replace value
80 -> 90

through delete + add.

D05 — Branch isolation
main     C1 -> C2
              \
experiment     C3

Verify neither branch observes the other's new data.

D06 — Historical branch

Branch from C1 after main has reached C5.

D07 — Fast-forward merge

Branch advances while target remains unchanged.

D08 — Divergent non-conflicting merge

Both branches modify different (g,s,p) keys.

D09 — Structural conflict

Both change the same (g,s,p) differently.

D10 — Same-result modification

Both independently produce the same final state.

D11 — Delete versus modify

One branch retracts a relation while another changes its object.

D12 — Multiple named graphs

Identical SPO terms in different named graphs MUST remain independent.

D13 — Repeated merge

Merge a branch twice and verify no duplicate semantic changes.

D14 — Deep DAG

Thousands of commits plus regular branching.

D15 — Merge ancestry

Multiple merge commits followed by another common-ancestor calculation.

66. Property-based differential testing

In addition to fixed scenarios, the test harness SHOULD generate deterministic randomized operation sequences.

Operations:

add quad
delete quad
create branch
advance branch
merge
historical read
diff

Each run uses a recorded seed.

Failures MUST print:

seed
minimal operation sequence
expected state
actual state

Property-based shrinking SHOULD reduce failures to a minimal reproducer.

67. Fluree as oracle — limits

Fluree MUST NOT be considered normative for every behavior.

It is a reference for common versioned-RDF semantics.

Sculpin intentionally differs by adding:

semantic merge validation
external virtual A-box validation
Sculpin provenance conventions
proposal workflow
projection architecture
branch policy

Tests must therefore classify outcomes as:

equivalent semantic expectation
Sculpin extension
intentional divergence
68. Performance comparison with Fluree

Side-by-side timings are useful, but MUST initially be informational rather than release-blocking.

Fluree is a sophisticated RDF database with dedicated indexes and query execution. The Cognitive Ledger has a much narrower purpose.

Therefore:

Fluree timing != Cognitive Ledger acceptance threshold

The comparison SHOULD identify:

unexpectedly expensive DAG operations;
pathological patch handling;
poor state-reconstruction scaling;
excessive storage overhead;
unexpected merge complexity.

Performance release gates SHOULD primarily compare Cognitive Ledger against its own previous baseline.

69. Benchmark dimensions

Benchmark scenarios SHOULD vary:

Dataset size
1,000 quads
100,000 quads
1,000,000 quads
Commit count
100
1,000
10,000
100,000
Patch size
1
10
100
1,000
10,000 operations
Branch count
1
10
100
1,000
Concurrency
1
8
32
64 writers
DAG shape
linear
wide branching
deep branching
repeated merges
high merge density
70. Measurements

Each benchmark SHOULD capture:

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

Results SHOULD be persisted as CI artifacts.

71. Performance gates

Initial hard absolute numbers SHOULD NOT be invented before representative hardware and realistic Sculpin workloads are established.

Instead:

Phase A

Establish repeatable baselines on a dedicated CI runner.

Phase B

Create regression thresholds.

Recommended initial release gate:

No >20% regression in p95
on designated critical benchmarks
without explicit performance-review approval.

For low-noise microbenchmarks, a stricter threshold MAY later be adopted.

Fluree comparisons remain informational.

72. Critical concurrency stress test

One particularly important invariant test is:

HEAD = C100

Launch 1,000 writers concurrently, all using:

expectedHead = C100

Expected result:

exactly one successful branch advance
999 HEAD_CHANGED results

After completion:

branch HEAD points to one valid child of C100
no branch references nonexistent data
all candidate immutable objects remain internally valid

This MUST be a release-blocking test.

73. Parallel independent branches

Create 100 branches from the same base and concurrently commit 100 updates to each.

Expected:

10,000 successful independent commits
zero cross-branch contamination
correct branch HEADs
correct state reconstruction
74. Crash/failure injection

Failure testing SHOULD deliberately interrupt processing at important points:

after patch object write
after commit object write
before PostgreSQL metadata commit
before branch CAS
after branch CAS
before projection outbox processing
during Fuseki update
after Fuseki update but before projector acknowledgement
during checkpoint creation

The resulting system MUST satisfy:

no dangling branch refs
no corrupted commit objects
no partial logical commits
safe retry
idempotent projection
recoverable checkpoint generation

Toxiproxy MAY be used for network failure injection around PostgreSQL, Fuseki and S3-compatible storage.

Internal Rust failpoints SHOULD be used where network proxies cannot reproduce the desired crash boundary.

75. PostgreSQL restart testing

Tests MUST cover:

process restart
database restart
unclean service termination
connection loss during transaction
connection-pool exhaustion

After restart, all committed branch heads MUST remain valid and reconstructable.

76. Fuseki failure testing

The ledger MUST remain writable according to branch policy if Fuseki projection is temporarily unavailable only where semantic validation is not required.

For protected branches requiring semantic validation:

validator unavailable
→ commit MUST NOT advance protected branch

Previously accepted commits remain valid.

Projection retries when Fuseki returns.

77. Object-store failure testing

Test:

write timeout
read timeout
truncated read
hash mismatch
missing object
duplicate put

Hash mismatch MUST be treated as integrity corruption rather than ordinary application failure.

78. Quality gates

The following gates are required before production release.

QG-0 — Specification consistency

PASS requires:

all core invariants represented by automated tests;
API semantics documented;
serialization format versioned;
storage schema versioned;
upgrade strategy documented.
QG-1 — Build and static analysis

PASS requires:

cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test

Core ledger crates SHOULD use:

#![forbid(unsafe_code)]

unless a reviewed dependency/interface makes this impractical.

QG-2 — Dependency and licence gate

PASS requires:

known dependency vulnerabilities reviewed
license allow-list clean
SBOM generated
container image scanned
no accidental Fluree runtime dependency

Tools such as cargo-deny, cargo-audit and an OCI scanner SHOULD be integrated.

QG-3 — Content determinism

PASS requires:

same logical commit
+ same canonical metadata
+ same patch
→ identical commit ID

across:

multiple executions
multiple machines
debug/release builds
supported OS environments

Golden vectors MUST be stored in the repository.

QG-4 — RDF correctness

PASS requires:

canonical quad parser tests;
datatype tests;
language-tag tests;
named-graph tests;
skolemization tests;
RDF Patch interoperability tests;
RDFC test vectors where RDFC is implemented.

Apache Jena RDF Patch behavior SHOULD be part of integration tests because the patch format explicitly supports atomic RDF additions/deletions and patch-log metadata.

QG-5 — DAG invariants

PASS requires automated proof-by-test that:

no invalid parent
no cycle
correct first-parent reconstruction
correct common ancestor
correct fast-forward detection
correct merge-parent ordering

Randomized DAG property tests are mandatory.

QG-6 — Persistence and recovery

PASS requires successful:

container restart
PostgreSQL restart
projector restart
object-store restart
crash recovery

with no loss of acknowledged commits.

QG-7 — Differential reference suite

PASS requires all designated Fluree-compatible structural scenarios to produce equivalent final/historical RDF states.

A mismatch MUST be classified as:

Cognitive Ledger defect
Fluree-reference difference
intentional documented divergence
test defect

Unclassified differences block release.

QG-8 — Sculpin semantic integration

PASS requires end-to-end tests that:

commit candidate
→ materialize graph
→ apply ontology
→ run SHACL
→ run configured reasoning
→ include virtual A-box
→ produce immutable validation record
→ correctly allow/reject ref advancement
QG-9 — Concurrency

PASS requires:

1,000 same-HEAD competing writer test;
independent branch stress;
merge-vs-write races;
branch-delete/update races;
no lost updates;
no silent ref overwrite.
QG-10 — Projection consistency

PASS requires:

ledger head reachable
projection eventually reaches same state
duplicate projection events harmless
projector restart harmless
projection state digest agrees with ledger state digest
QG-11 — Performance regression

PASS requires critical Cognitive Ledger benchmark results remain within the accepted regression envelope.

Fluree timing differences do not independently fail this gate.

QG-12 — Security

PASS requires testing for:

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

Fuzzing MUST cover externally supplied RDF Patch and commit metadata parsers.

QG-13 — API compatibility

Every API change MUST be classified:

backward compatible
versioned breaking change
internal

OpenAPI schema compatibility SHOULD be checked automatically.

QG-14 — Upgrade/recovery

PASS requires upgrading a persisted previous-release test ledger to the candidate release while retaining:

commit IDs
branch heads
historical reconstruction
validation references
projection recoverability

Commit identity rules MUST never change silently.

79. Test pyramid

Recommended test distribution:

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

The project SHOULD favor property/invariant tests over chasing an arbitrary global code-coverage percentage.

Critical core modules SHOULD nevertheless maintain high coverage, with coverage regression reported automatically.

80. CI profiles
Pull request

Run:

format
clippy
unit tests
property tests
PostgreSQL integration
small Fuseki integration
small deterministic Fluree differential suite
Main branch

Additionally run:

randomized differential suite
restart tests
failure injection subset
performance smoke tests
Nightly

Run:

large differential suite
10k–100k commit scenarios
stress tests
high-concurrency tests
large state reconstruction
full failure injection
fuzzing
performance benchmarks
Release candidate

Run every quality gate.

81. Reference Docker Compose profile

Conceptually:

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

Use Compose profiles:

default
integration
differential
stress
fault

so the Fluree reference service is not started during ordinary runtime use.

82. Performance harness architecture

The benchmark runner SHOULD execute identical operation sequences where meaningful:

Scenario
   │
   ├──── CognitiveLedgerAdapter
   │
   └──── FlureeAdapter

Outputs:

results/
   scenario.json
   cognitive.json
   fluree.json
   host.json

The report SHOULD show:

correctness result
latencies
throughput
memory
storage
relative timing

but MUST clearly label that the architectures have different scopes.

83. Deterministic fixtures

The repository SHOULD contain inspectable graph fixtures such as:

materials/
organization/
sensor-network/
proposal-evaluation/

Fixtures SHOULD deliberately include:

functional properties
multi-valued properties
named graphs
provenance
SHACL constraints
subclass reasoning
external virtual values

This is preferable to benchmarks consisting only of meaningless random RDF triples.

84. Semantic merge fixture

One standard fixture SHOULD demonstrate why Sculpin's merge layer exceeds structural merging.

Base:

ex:material ex:temperature 80 .

Branch A:

ex:material ex:temperature 85 .

Branch B:

ex:material ex:temperature 90 .

Shape:

temperature maxCount 1

Expected:

structural collision = true
union merge          = RDF-valid
SHACL-valid          = false

This MUST be an end-to-end regression test.

85. External-context fixture

Base graph:

pump P hasMaximumPressure 100

Virtual A-box:

currentPressure = 110

Candidate rule/knowledge update is evaluated with temporary external data.

Test MUST demonstrate:

virtual A-box participates in validation
virtual observation is not persisted
validation record identifies external context
ledger patch remains unchanged
86. Repository architecture

Recommended Rust workspace:

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

ledger-core MUST remain independent of HTTP, PostgreSQL and Fuseki.

This permits extensive deterministic testing of the actual data model.

87. Dependency direction

Preferred dependency architecture:

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

Core model code MUST NOT depend on infrastructure adapters.

88. Migration discipline

PostgreSQL schema migrations MUST be:

ordered
immutable once released
automatically tested
backward recovery tested

Changes to commit serialization require a new explicit format identifier:

sculpin-cognitive-commit/v1
sculpin-cognitive-commit/v2

Old commit formats MUST remain readable.

Their identifiers MUST remain unchanged.

89. Compatibility rule

A future service version MUST be able to verify historical commit IDs produced by older versions.

This is a hard long-term requirement.

Canonicalization rules are therefore effectively a persistent public protocol.

They require stronger review than normal implementation code.

90. Initial implementation milestones
Milestone 1 — Immutable linear ledger

Deliver:

canonical RDF patch
content-addressed patch
content-addressed commit
single main ref
CAS transactions
history
state reconstruction
PostgreSQL
filesystem object store

Gate:

QG-0 through QG-5
Milestone 2 — Sculpin integration

Deliver:

Fuseki projection
SHACL adapter
reasoning adapter
virtual A-box validation
validation records

Gate:

QG-8
QG-10
Milestone 3 — Branching

Deliver:

branch creation
historical branch
branch deletion
ref audit history
branch policies

Add Fluree differential tests.

Milestone 4 — Merge

Deliver:

common ancestor
three-way diff
fast-forward
divergent merge
structural collision
merge preview
semantic conflict detection

This is the first complete cognitive-feedback workflow.

Milestone 5 — Operational hardening

Deliver:

S3-compatible object storage
checkpoints
garbage collection
metrics
OpenTelemetry
OIDC
rate/resource limits
fault recovery
Milestone 6 — Production gate

Deliver:

stress suite
fuzzing
SBOM
security scans
performance baselines
upgrade tests
operational documentation
backup/restore
91. Definition of MVP success

The project should consider the architecture validated when the following scenario works reliably:

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

If this works while satisfying the invariants and quality gates above, the Cognitive Ledger provides the core functionality required for an evolving Sculpin cognitive layer.

92. Architectural conclusion

The Cognitive Ledger should deliberately remain smaller than a graph database.

Its core responsibility can be summarized as:

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

The most important boundary is:

Cognitive Ledger
    owns evolution and history

Sculpin/Jena
    owns semantics and validation

Fuseki
    owns queryable materialized state

Virtual A-box
    owns transient external context

This boundary makes a Sculpin-specific implementation substantially more tractable than reproducing Fluree while preserving the architectural mechanisms that make Fluree useful as inspiration and as a differential reference implementation.

One point I would make especially firm before implementation: the Fluree test should be a semantic oracle, not a benchmark target. Fluree already maintains optimized historical indexes and a dedicated RDF query engine; its own documentation describes snapshot/index selection for time-travel queries. The more meaningful production gate is that the Cognitive Ledger does not regress against its own pinned benchmark baseline, while Fluree timings provide context and can expose pathological algorithms.

I would also make the 1,000-writers/same-HEAD CAS test, deterministic commit hash golden vectors, randomized DAG tests, crash recovery, and the semantic merge fixture mandatory very early. Those four areas are much harder to fix after real data has begun accumulating than API/UI details.

Using codex cloud for starting the development of the cognitive ledger, provide a high quality instruction for codex to set up the harnessing, all appropriate agentic configurations for codex and claude (skills, hooks, agent.md, tools) along with an explicit work plan that ensure the development doesn't go off rails.

I would use the first Codex Cloud task to build both the engineering harness and a very small walking skeleton, while explicitly prohibiting it from jumping ahead into branching, merging, or performance optimization. The prompt below follows current OpenAI guidance to keep AGENTS.md concise and make structured repository documentation the durable source of truth; it also uses portable SKILL.md skills and Claude's native CLAUDE.md, subagents, rules, and hooks rather than maintaining two unrelated instruction systems.

Initial Codex Cloud Bootstrap Instruction — Sculpin Cognitive Ledger

You are the lead/bootstrap engineer for a new software project: Sculpin Cognitive Ledger.

Your task is not merely to generate application code. Your first responsibility is to establish a durable, agent-friendly engineering environment in which Codex Cloud, future Codex sessions, Claude Code, human developers, and CI all work from the same architectural rules, execution plans, quality gates, and test evidence.

Treat this as the first engineering session of a long-running production software project.

Do not optimize for producing the maximum amount of code in this session. Optimize for making subsequent development reliable, reviewable, reproducible, and difficult to accidentally steer away from the intended architecture.

1. Mission

Build the initial repository, engineering harness, agent configuration, documentation system, CI/test infrastructure, and a minimal executable vertical slice for the Sculpin Cognitive Ledger.

The Cognitive Ledger is a narrowly scoped versioned RDF knowledge-evolution service inspired by useful architectural ideas from Fluree and Git, but it is not a replacement graph database and must not evolve into one.

Its eventual responsibility is:

immutable RDF changes
        +
content-addressed commit DAG
        +
CAS-protected mutable branch refs
        +
provenance and temporal history
        +
state reconstruction
        +
three-way graph merging
        +
Sculpin/Jena semantic validation

Its boundaries are:

Cognitive Ledger
    owns change history, immutable commits, DAGs,
    branches, provenance and merge coordination

Sculpin/Jena
    owns semantics, SHACL, reasoning and domain rules

Fuseki
    owns queryable RDF projections

Virtual A-box
    owns transient external context

Do not blur these boundaries.

2. Before modifying the repository

First inspect the repository and execution environment.

Determine:

whether the repository is empty or already contains files;
current Git state and branch;
available Rust toolchain;
whether Docker/Compose is actually available in the Codex Cloud execution environment;
what Codex skills/plugin/hook configuration mechanisms are currently supported;
current Claude Code project configuration conventions;
whether network access permits retrieving dependencies and public documentation.

Use current official documentation when agent configuration details are uncertain. Do not invent obsolete .codex conventions or unsupported configuration files.

For OpenAI/Codex-specific behavior, consult the current official OpenAI developer documentation.

For Claude Code configuration, consult the current Anthropic/Claude Code documentation.

If Docker cannot run inside the current Codex Cloud environment:

still create and validate the Docker/Compose configuration statically;
create CI jobs that execute Docker integration tests;
run every test locally available without Docker;
clearly record Docker tests as not executed in the current environment rather than pretending they passed.

Do not stop the task merely because Docker is unavailable.

3. Source specification

Create:

docs/product-specs/cognitive-ledger.md

and make it the authoritative product/technical specification.

Populate it from the Cognitive Ledger requirements supplied with this task.

At minimum the specification must preserve these core requirements.

Hard invariants
Commits are immutable.
Commit identifiers are content-derived and deterministic.
Parent commits must exist before a commit becomes reachable.
Commit ancestry is a DAG.
Branches are mutable references to immutable commits.
Every branch advancement uses compare-and-set semantics.
A branch may never point to nonexistent content.
Reconstruction of the same commit always produces the same RDF dataset.
History is never silently rewritten.
Fuseki/projection failures cannot corrupt ledger history.
Persistent ledger RDF must not contain anonymous blank nodes.
Commit identity/canonicalization rules are persistent protocol and cannot silently change in future releases.
Fluree must never become a runtime or compiled dependency.
The ledger must not implement SPARQL, OWL reasoning or SHACL itself.

Treat changes to these invariants as architectural changes requiring an ADR.

4. Harness philosophy

Follow this hierarchy:

AGENTS.md / CLAUDE.md
       │
       │ small navigation + non-negotiable rules
       ▼
structured docs/
       │
       ├── architecture
       ├── specifications
       ├── ADRs
       ├── active execution plans
       ├── quality gates
       ├── testing strategy
       └── generated evidence
       │
       ▼
code + automated enforcement

Do not create a giant agent instruction file.

AGENTS.md should function primarily as a map.

CLAUDE.md should contain only always-relevant Claude Code instructions.

Task-specific knowledge belongs in skills, rules, design documents and execution plans.

Whenever practical, replace prose requirements with mechanical enforcement.

5. Required repository knowledge structure

Establish at least:

AGENTS.md
CLAUDE.md
ARCHITECTURE.md

docs/
├── README.md
│
├── product-specs/
│   └── cognitive-ledger.md
│
├── design/
│   ├── core-beliefs.md
│   ├── data-model.md
│   ├── canonicalization.md
│   ├── storage-boundaries.md
│   └── agent-harness.md
│
├── decisions/
│   ├── README.md
│   └── ADR-0001-*.md
│
├── exec-plans/
│   ├── README.md
│   ├── active/
│   ├── completed/
│   └── tech-debt.md
│
├── quality/
│   ├── quality-gates.md
│   ├── test-strategy.md
│   ├── differential-testing.md
│   ├── performance-testing.md
│   └── security.md
│
└── generated/
    └── README.md

Do not produce large amounts of redundant prose.

Cross-link documentation.

docs/README.md should act as the documentation index.

6. AGENTS.md

Create a concise root AGENTS.md, ideally around 100 lines and certainly not an encyclopedia.

It must tell Codex:

what the project is;
where the authoritative specification lives;
where architecture decisions live;
where the active execution plan lives;
the principal architectural boundaries;
commands for formatting, linting, testing and integration testing;
that no implementation task begins before reading the active execution plan;
that completed work must update the execution plan and evidence;
that architectural changes require an ADR;
that Fluree is test-reference-only;
that Jena/Fuseki owns semantics;
that unsafe history rewriting is prohibited;
that deterministic serialization/hash behavior must never be changed casually;
that tests and docs are part of the implementation;
that fabricated test results are forbidden.

Also explicitly instruct Codex to use subagents for independent review/research slices when that improves quality, while keeping authoritative implementation ownership in the main session.

7. CLAUDE.md

Create a root CLAUDE.md.

Keep it below approximately 200 lines.

It should mirror the essential project-level behavior from AGENTS.md, without copying the full project specification.

It must provide:

common commands;
architecture boundaries;
invariant pointers;
repository navigation;
requirement to read the active execution plan;
requirement to use specialized subagents when appropriate;
requirement to update project documentation after meaningful architectural decisions;
no destructive Git history operations;
no copying of Fluree source code;
no disabling quality gates to make CI green.

Where supported, prefer the current opus alias for deep-review Claude subagents rather than pinning a dated model identifier.

Do not hard-code a specific future model version unless required by current Claude Code syntax.

8. Provider-neutral skills

Create a canonical provider-neutral skill collection:

skills/
├── ledger-invariants/
│   └── SKILL.md
├── rdf-change-model/
│   └── SKILL.md
├── execution-plan/
│   └── SKILL.md
├── architecture-review/
│   └── SKILL.md
├── differential-testing/
│   └── SKILL.md
├── quality-gate/
│   └── SKILL.md
└── release-readiness/
    └── SKILL.md

Use the current open Agent Skills SKILL.md structure.

Keep each skill narrow.

Do not put everything in every skill.

ledger-invariants

Use when modifying:

commits;
hashes;
parent relationships;
refs;
canonicalization;
persistence.

Require explicit invariant review.

rdf-change-model

Use for:

RDF Patch;
N-Quads;
term normalization;
blank-node/skolemization behavior;
dataset state comparison.
execution-plan

Teach agents how to:

create an executable plan;
update progress;
record discoveries;
distinguish planned work from tangents;
close a plan only after evidence exists.
architecture-review

Provide an explicit review checklist around:

service boundaries;
dependency direction;
accidental database functionality;
infrastructure leakage into core crates;
protocol stability.
differential-testing

Define how Fluree is used:

reference implementation only
not source-code dependency
not runtime dependency
not release-performance target

Define scenario/result classification:

equivalent semantics
intentional Sculpin divergence
reference difference
test defect
quality-gate

Teach the agent to execute and report all gates honestly.

release-readiness

Eventually coordinates:

tests;
static analysis;
dependency audit;
security;
migrations;
compatibility;
Docker;
documentation;
benchmark evidence.

It does not need full implementation yet, but establish its structure now.

9. Codex integration

Configure the repository for current Codex capabilities only after verifying current official support.

Where supported:

expose skills/ through Codex's current skill/capability mechanism;
create an appropriate .codex-plugin/plugin.json if a repository/plugin configuration is the current supported mechanism;
use only supported command hooks;
do not invent repository-local configuration that Codex Cloud ignores.

Create:

docs/design/codex-cloud.md

documenting:

which repository files Codex reads automatically;
which skills need explicit capability/plugin registration;
how Codex Cloud should be configured for this repository;
recommended tools;
expected network requirements;
limitations of the current hosted environment;
how multi-agent/subagent work should be used.

Codex currently supports managed multi-agent workflows. Use subagents for bounded independent tasks, not uncontrolled parallel modification of the same code.

Suitable subagent work includes:

reviewing the core invariants;
reviewing RDF canonicalization assumptions;
reviewing test coverage;
researching current Fluree public behavior;
security review;
benchmark design.

Do not have multiple agents concurrently edit the same core files.

10. Claude Code project configuration

Create the current project-native Claude Code structure as appropriate:

.claude/
├── settings.json
├── rules/
└── agents/

Also make the canonical skills/ available to Claude using the simplest currently supported mechanism without maintaining two independently editable copies.

If Claude requires .claude/skills/, create thin adapters/imports or another mechanically synchronized mechanism.

Do not manually duplicate long skill bodies.

Document the arrangement in:

docs/design/claude-code.md
11. Claude rules

Create path-scoped rules where useful rather than bloating CLAUDE.md.

Candidate rules:

.claude/rules/rust-core.md
.claude/rules/tests.md
.claude/rules/migrations.md
.claude/rules/docs.md

Examples:

Rust core

For crates/ledger-core/**, crates/ledger-dag/**, etc.:

forbid infrastructure dependencies;
prefer deterministic pure functions;
use explicit error types;
forbid hidden global state;
avoid unsafe;
invariants require tests.
Tests

For tests/**:

tests must be deterministic unless explicitly stress/performance;
randomized tests must emit reproducible seeds;
do not weaken assertions because implementation fails.
Migrations

For migrations/**:

released migrations are immutable;
new migrations are additive;
upgrades must be tested.
12. Claude specialist subagents

Create a small number of high-value specialist subagents under the currently supported Claude location, such as:

.claude/agents/
├── architecture-reviewer.md
├── invariant-reviewer.md
├── test-reviewer.md
├── security-reviewer.md
└── performance-reviewer.md

Do not create a huge agent zoo.

Each subagent must have a narrow scope.

Use the latest appropriate Opus-class model alias where project configuration permits this without hard-coding stale version numbers.

Architecture reviewer

Checks:

architecture boundaries;
dependency direction;
accidental reimplementation of graph database functionality;
unnecessary abstractions.
Invariant reviewer

Checks:

commit immutability;
content-addressability;
DAG correctness;
ref concurrency;
deterministic canonicalization.
Test reviewer

Looks for:

untested invariants;
happy-path-only tests;
flaky integration behavior;
missing concurrency tests;
misleading mocks.
Security reviewer

Checks:

untrusted RDF/metadata parsing;
resource exhaustion;
authentication boundary assumptions;
object-store integrity;
SQL safety;
path traversal;
secret handling.
Performance reviewer

Does not prematurely optimize.

It identifies:

asymptotic problems;
unbounded ancestry scans;
unnecessary full-state materialization;
benchmark methodology problems.
13. Shared hooks

Use deterministic hooks only where deterministic enforcement is valuable.

Prefer shared repository scripts such as:

scripts/agent-hooks/

and have Codex/Claude adapters call those scripts where supported.

Do not duplicate hook logic.

Useful hooks include:

Pre-tool / pre-command safety

Block or warn on commands such as:

git push --force
git reset --hard
git clean -fdx
rm -rf /

and similar destructive operations.

Do not implement simplistic matching that blocks ordinary safe commands.

Secret protection

Prevent accidental edits/commits of known credential files such as:

.env
*.pem
*.key
credentials.*

while allowing tracked example files such as:

.env.example
Post-edit formatting

For Rust changes, formatting MAY run automatically if the hook mechanism makes this reliable and inexpensive.

Do not run the entire test suite after every edit.

Stop/session completion

A Stop hook MAY run a lightweight repository sanity command, but it must not make long-running tests unavoidable on every interaction.

Use a dedicated command such as:

./scripts/check-fast.sh

The agent still must explicitly run the required quality gate before claiming completion.

For Claude, configure hooks using the current supported .claude/settings.json schema.

For Codex, use current supported command hook/plugin semantics only after verifying them.

If Codex Cloud cannot automatically activate project hooks, document how they are enabled rather than assuming they execute.

14. Tool policy

Agents should have only tools useful to software development.

Expected capabilities:

filesystem
git
shell
Rust toolchain
Cargo
Docker/Compose when available
curl
jq
Python for test utilities
web/documentation search when available

Potential developer tools:

rustfmt
clippy
cargo-nextest
cargo-deny
cargo-audit

Add them only if they provide clear project value.

Do not add dependencies or MCP servers merely because they exist.

MCP

MCP should be conservative.

Useful optional connections:

official documentation;
read-only GitHub access where needed for reference research.

Do not provide write-capable external MCP tools to ordinary development agents unless a real workflow requires them.

Credentials must never be stored in repository files.

15. Repository implementation architecture

Create a Rust workspace designed around dependency inversion.

Initial target structure:

crates/
├── ledger-core/
├── ledger-rdf/
├── ledger-store/
├── ledger-dag/
├── ledger-merge/
├── ledger-validation/
├── ledger-projection/
├── ledger-api/
└── ledger-testkit/

apps/
└── ledger-server/

tests/
├── integration/
├── differential/
├── stress/
└── fault/

fixtures/
benchmark/
migrations/
docker/
scripts/

Do not create empty crates merely to make the tree visually match this design.

Create only crates needed for the current walking skeleton, while reserve/documenting the intended boundaries.

At minimum begin with something like:

ledger-core
ledger-rdf
ledger-store
ledger-api
ledger-server
ledger-testkit

unless inspection shows an even cleaner decomposition.

The core must not depend on:

Axum
SQLx
PostgreSQL
Fuseki
Docker
S3 clients

Infrastructure depends on core abstractions, never the reverse.

16. Technology choices

Primary implementation language:

Rust

Use the current stable Rust edition/toolchain suitable for production.

Pin the toolchain in:

rust-toolchain.toml

Use a committed:

Cargo.lock

Choose current stable crate versions based on compatibility and maintenance status.

Do not use prerelease dependencies without a documented reason.

Likely infrastructure choices include:

Axum
Tokio
Serde
thiserror
tracing
SQLx
PostgreSQL

but verify current versions and suitability rather than blindly adopting this list.

Create an ADR for important foundational technology choices.

17. Canonicalization protocol

Treat hashing/canonicalization as protocol code.

Do not casually change it.

For initial work establish:

ContentId
PatchId
CommitId

using explicit algorithm-tagged values such as:

sha256:<hex>

Implement deterministic serialization using standards where appropriate:

canonical N-Quads representation for RDF terms/quads;
deterministic sorting of patch operations;
RFC 8785/JCS or another explicitly documented deterministic representation for commit metadata.

Persisted ledger state must not contain anonymous blank nodes.

Skolemization rules do not need to be fully production-complete in the first session, but the interface and invariant must be explicit.

Add golden test vectors.

Example principle:

same canonical logical input
→ same bytes
→ same ID

Run the golden tests multiple times.

18. Fluree reference harness

Fluree exists only as a differential testing reference.

Use the official:

fluree/server

Docker image.

Do not:

copy Fluree source code;
vendor Fluree;
link to Fluree crates;
depend on Fluree at runtime;
expose Fluree through Sculpin;
treat Fluree performance as the release target.

Create an optional Compose profile such as:

differential

containing a service like:

fluree-reference

Do not use latest in CI.

Establish:

test/reference-images.lock

or an equivalent machine-readable file recording:

image
version
digest
purpose

If obtaining a stable digest is impossible in the current environment, create the mechanism and record the unresolved pinning as an explicit blocker in the active execution plan. Do not invent a digest.

The eventual comparison adapter should compare semantic graph states, not commit IDs.

19. Docker/Compose harness

Establish a development Compose topology that can evolve toward:

ledger
postgres
fuseki

with profiles for:

differential -> fluree-reference
fault        -> toxiproxy or equivalent later

Do not add MinIO unless it is needed in the current milestone.

Filesystem-backed immutable storage is sufficient initially.

Use health checks.

Ensure services have deterministic startup dependencies.

The default runtime must not start Fluree.

20. CI

Set up appropriate CI for the repository's hosting platform.

If GitHub Actions is appropriate, establish workflows such as:

ci-fast
ci-integration
ci-differential
ci-security

Avoid one giant opaque workflow.

Pull-request gate

At minimum:

cargo fmt --check

cargo clippy \
  --workspace \
  --all-targets \
  --all-features \
  -- \
  -D warnings

cargo test --workspace

Also validate:

Docker Compose syntax
documentation link/path sanity
migration consistency where implemented

Run small integration/differential tests where CI supports Docker.

Nightly or scheduled later

Reserve longer jobs for:

large differential tests
stress tests
fault injection
fuzzing
performance benchmarks

Do not make PR feedback unreasonably slow.

21. Security/dependency harness

Add reasonable supply-chain checks.

Consider:

cargo-deny
cargo-audit
SBOM generation
container scanning

Do not make the first session fail permanently because a third-party vulnerability database reports a non-actionable warning.

Classify findings.

Create:

deny.toml

if cargo-deny is adopted.

Maintain an explicit license policy.

Make clear that Fluree's BSL-licensed image is a test reference, not shipped runtime functionality.

22. Fast developer commands

Provide stable scripts or just/make tasks for common workflows.

Do not require developers or agents to memorize long commands.

At minimum provide equivalents of:

check
test
test-integration
test-differential
lint
format
compose-up
compose-down

Choose just, make, or scripts based on simplicity.

Avoid adding a task runner if plain scripts are cleaner.

The commands documented in AGENTS.md and CLAUDE.md must actually work.

23. Execution-plan discipline

Create the first active execution plan:

docs/exec-plans/active/0001-bootstrap-and-walking-skeleton.md

The plan must be a living engineering document.

It must contain:

Goal
Scope
Non-goals
Relevant invariants
Assumptions
Work breakdown
Dependency/order constraints
Current status
Decisions made
Discoveries
Risks
Quality gates
Test evidence
Deferred work
Completion criteria

Use explicit task checkboxes.

Update this document during the session after meaningful milestones.

Do not mark an item complete because code exists.

Mark it complete only after relevant verification passes.

When this milestone is complete, move the plan to:

docs/exec-plans/completed/

and create the next active plan.

There should normally be one primary active milestone plan.

24. ADR discipline

Create ADR infrastructure and record decisions that would be expensive to reverse.

Initial ADRs likely include:

ADR-0001 Rust and service boundaries
ADR-0002 content-addressed immutable commit model
ADR-0003 RDF patch representation and canonicalization
ADR-0004 PostgreSQL mutable-ref metadata vs immutable object storage
ADR-0005 Fluree differential-reference policy

Only create ADRs for genuine architectural decisions.

Avoid bureaucratic ADRs for trivial code choices.

Each ADR should contain:

Context
Decision
Alternatives considered
Consequences
Status
25. First implementation scope: walking skeleton

After the harness, documentation, CI skeleton and execution plan exist, implement only enough functionality to validate the architecture.

Do not implement branching or merging in this milestone.

Target a vertical slice:

canonical RDF patch
       ↓
content-addressed patch
       ↓
immutable commit with zero/one parent
       ↓
immutable object store
       ↓
single main ref with CAS semantics
       ↓
state reconstruction
       ↓
minimal HTTP health/API surface

A useful first acceptance scenario is:

1. Create an empty ledger.
2. HEAD is genesis or empty according to the documented model.
3. Commit patch P1.
4. main advances to C1 using expected HEAD.
5. Commit patch P2.
6. main advances C1 -> C2.
7. Reconstruct state at C1.
8. Reconstruct state at C2.
9. Verify expected RDF differences.
10. Attempt another update using stale expected HEAD C1.
11. Receive HEAD_CHANGED/409-equivalent.
12. Verify main still points to C2.

This demonstrates:

immutability
deterministic identities
linear ancestry
patch application
history reconstruction
optimistic concurrency

without prematurely implementing the whole system.

26. Store abstractions

Use explicit interfaces/traits.

Examples conceptually:

ObjectStore
CommitStore
RefStore

Do not over-abstract.

A filesystem-backed object store and an in-memory/test ref implementation are acceptable for the earliest core tests.

PostgreSQL should be introduced for real mutable ref semantics as part of this milestone if feasible.

Object writes must be idempotent.

Writing different bytes under the same Content ID is corruption and must fail.

27. PostgreSQL CAS test

If PostgreSQL integration is established, add a real integration test proving atomic branch advancement.

Conceptually:

HEAD = C1

writer A expects C1
writer B expects C1

both race

exactly one advances successfully
the other receives a conflict

Do not rely solely on mocked concurrency behavior.

The larger 1,000-writer test belongs to a later stress milestone, but design toward it.

28. Test taxonomy

Create clear test categories.

Unit

For:

hashing
serialization
RDF term normalization
patch sorting
ContentId parsing
Property tests

For:

serialization determinism
patch normalization idempotence
DAG-related invariants as DAG functionality appears

Use a Rust property-testing framework if justified.

Randomized failures must report reproducible seeds.

Integration

For:

PostgreSQL
filesystem/object persistence
HTTP
service restart
Differential

For Fluree-compatible semantics.

Keep this minimal in the first milestone.

Stress

Created later.

Fault

Created later.

29. Golden protocol vectors

Create repository-owned fixtures for deterministic protocol behavior.

For example:

fixtures/golden/
├── patches/
└── commits/

For every vector store:

logical input
canonical bytes or inspectable representation
expected SHA-256 ID

These are protocol compatibility tests.

They must not be regenerated silently by normal tests.

If a change modifies a golden ID, the developer must explicitly explain why.

That change requires architectural review.

30. Initial differential test

If Docker is available, implement one minimal side-by-side scenario against Fluree:

initial graph
+
one addition
+
one deletion/replacement
+
historical state comparison

Compare normalized RDF state, not internal identifiers.

If Docker is unavailable in Codex Cloud:

create the adapter/test;
wire it into Docker-capable CI;
validate as much as possible statically;
explicitly state that it was not executed locally.

Do not block the entire bootstrap on Fluree availability.

31. No premature benchmarks

Set up the benchmark directory and methodology, but do not spend this milestone optimizing throughput.

Performance work should begin with baselines rather than guesses.

Document eventual benchmark dimensions:

state size
commit count
patch size
branch count
concurrent writers
DAG shape

Record:

p50
p95
p99
throughput
CPU
RSS
storage

Fluree timing is informational only.

Cognitive Ledger performance regression against its own baseline will eventually become the release gate.

32. Quality gates for this first milestone

The session must not claim success until all applicable gates pass.

Gate A — repository structure
AGENTS.md exists and is concise.
CLAUDE.md exists and is concise.
docs are indexed.
active plan is current.
architectural boundaries are written down.
Gate B — Rust quality

Applicable workspace commands pass:

cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
Gate C — determinism

Golden tests prove stable patch and commit IDs.

Run determinism tests repeatedly.

Gate D — architecture

Check:

core crates do not depend on infrastructure crates
Fluree is not a runtime/compile dependency
no SPARQL engine has been introduced
no reasoning implementation has been introduced
Gate E — concurrency

At least the initial CAS conflict test passes.

Gate F — persistence

Acknowledged commits survive service/database restart where the current implementation provides persistence.

Gate G — Docker configuration
docker compose config

or equivalent static validation passes.

If runtime Docker is available, smoke tests pass.

Gate H — CI

CI definitions are syntactically valid and correspond to commands that exist.

Gate I — documentation

Every command in the agent instructions has been verified.

No documentation claims an unexecuted test passed.

33. Quality-gate runner

Create a canonical developer/agent command such as:

./scripts/quality-gate.sh

or equivalent.

It should run the appropriate local release-independent gates.

It may support modes:

fast
integration
full

Do not hide failures.

Do not automatically rewrite code during a check mode.

Formatting fixes belong in a separate command.

34. Prevent development from going off rails

Throughout the session enforce these rules.

Do not broaden the product

If you find yourself implementing:

SPARQL query optimization
OWL reasoning
SHACL engine
vector search
full-text search
generic graph database indexing
distributed consensus
Cypher
GraphQL

stop.

Record the observation as out-of-scope.

Do not prematurely implement later milestones

Do not implement:

general branching
three-way merge
semantic merge conflicts
S3
Raft
signing
complex GC
large performance optimization

during the bootstrap unless a tiny interface is required to prevent architectural dead ends.

Do not create speculative abstraction layers

Every abstraction needs a current use case or clear boundary reason.

Do not weaken tests

A failing invariant test means the implementation is wrong until proven otherwise.

Do not edit protocol behavior casually

Canonicalization and hash identity are long-lived compatibility surfaces.

Do not copy Fluree

Study public behavior and architecture.

Implement independently.

Do not let TODOs disappear

Deferred work goes into:

docs/exec-plans/tech-debt.md

or the next execution plan.

35. Agent workflow for this session

Use the following order.

Phase 0 — inspect

Inspect repository, tools, platform constraints and current documentation.

Produce findings in the active execution plan.

Phase 1 — establish source of truth

Create:

specification
architecture
core beliefs
quality gates
active execution plan
ADR framework

Do not begin substantial implementation before this exists.

Phase 2 — establish agent harness

Create:

AGENTS.md
CLAUDE.md
skills
Claude rules
Claude subagents
shared hooks
Codex configuration/documentation

Verify current tool compatibility.

Phase 3 — establish engineering harness

Create:

Rust workspace
toolchain pin
developer scripts
CI
Docker/Compose
dependency/security checks
logging/tracing foundation
Phase 4 — implement walking skeleton

Implement only the linear ledger slice.

Phase 5 — independently review

Use specialized subagents where available for separate reviews:

architecture/invariants;
tests/concurrency;
security/dependency issues;
documentation/harness coherence.

Do not ask reviewers merely whether the code “looks good.”

Give each reviewer explicit failure-finding objectives.

Phase 6 — fix confirmed defects

Resolve findings based on evidence.

Do not mechanically implement every speculative reviewer suggestion.

Phase 7 — execute gates

Run every applicable quality gate.

Record exact commands and outcomes.

Phase 8 — close milestone

Update:

active execution plan
ADRs
tech debt
documentation

Only move the plan to completed/ if its acceptance criteria actually pass.

Otherwise leave it active with an accurate status.

36. Subagent usage rules

When multi-agent capability is available:

Use subagents for independent context-heavy work.

Good examples:

review protocol determinism
research current Fluree behavior
review Rust crate boundaries
review concurrency tests
review security

Do not delegate the overall architecture to independent agents and blindly merge their outputs.

The main session owns integration and consistency.

Do not have two implementation agents edit the same files concurrently.

Ask reviewers to inspect actual code, not only implementation reports.

Reviewer outputs must distinguish:

confirmed defect
risk requiring decision
optional improvement
no issue found
37. Required end-of-session report

At completion, provide a concise but evidence-based report containing:

Implemented

What actually exists.

Architecture

Key choices and ADRs.

Agent harness

What Codex and Claude configuration was established.

Verification

Exact quality commands executed and their results.

Separate:

executed and passed
executed and failed
not executable in current environment
not yet implemented
Docker/Fluree

State whether the Fluree differential test actually ran.

Do not imply it ran if Docker was unavailable.

Remaining risks

Only material open issues.

Next milestone

Recommend the next active execution plan.

The next milestone should probably deepen Milestone 1: immutable linear ledger or begin Sculpin semantic integration, depending on how much of the walking skeleton was completed.

Do not begin that next milestone in this session unless there is substantial remaining context and every current acceptance gate is green.

38. Definition of success for this Codex task

This initial task succeeds when another Codex or Claude Code session can enter the repository and immediately determine:

what is being built
what is explicitly not being built
where authoritative requirements live
what the current work plan is
which architectural invariants cannot be violated
how to build the project
how to test it
how to run integration tests
how Fluree is used
how architectural decisions are recorded
how to obtain specialist review
what has already been verified
what remains to be done

and when the repository contains a small, tested, deterministic executable slice demonstrating that the architecture is viable.

A large amount of feature code is not a success criterion.

A controlled foundation from which the Cognitive Ledger can be safely evolved is.

One practical point I intentionally included is the Docker fallback: Codex Cloud environments can differ in whether a usable Docker daemon is available, so the agent should create the Fluree/Fuseki integration harness regardless, but distinguish “wired into CI” from “actually executed here.” That avoids the common failure mode where an agent either abandons integration work because Docker is unavailable or claims validation it could not perform.

I also recommend keeping the provider-neutral skills/ directory as the canonical workflow layer. Codex can discover skills through its capability/plugin mechanisms, while Claude can expose the same material through thin .claude/skills adapters; current OpenAI tooling explicitly uses the open SKILL.md format, and Claude similarly treats skills as on-demand reusable guidance.