# Cognitive Ledger product and technical specification

**Authority:** This document defines product scope and persistent invariants. Changes to an invariant require an accepted ADR and protocol-impact review.

## Purpose and boundaries
The Cognitive Ledger records immutable RDF changes, content-addressed commit history, mutable CAS-protected refs, provenance, historical reconstruction, and eventually three-way merge coordination. It is not a graph database.

| Owner | Responsibility |
|---|---|
| Cognitive Ledger | change history, immutable commits/DAG, refs, provenance, reconstruction and merge coordination |
| Sculpin semantic validation/reasoning layer (currently pySHACL + Python reasoning) | semantics, RDF reasoning, SHACL and domain rules |
| Fuseki | queryable RDF projections |
| Virtual A-box | transient external context |

The ledger does not implement SPARQL, OWL, SHACL, vector/full-text search, generic graph indexing, or distributed consensus. Projection failure never changes accepted history.

## Hard invariants
1. Commits are immutable.
2. Commit identifiers are content-derived and deterministic.
3. Parent commits must exist before a commit becomes reachable.
4. Commit ancestry is a DAG.
5. Branches are mutable references to immutable commits.
6. Every branch advancement uses compare-and-set semantics.
7. A branch may never point to nonexistent content.
8. Reconstruction of the same commit always produces the same RDF dataset.
9. History is never silently rewritten.
10. Fuseki/projection failures cannot corrupt ledger history.
11. Persistent ledger RDF must not contain anonymous blank nodes.
12. Commit identity/canonicalization rules are a persistent protocol and cannot silently change.
13. Fluree must never become a runtime or compiled dependency.
14. The ledger must not implement SPARQL, OWL reasoning, or SHACL itself.

## Walking-skeleton model
`main` starts absent. A patch is an immutable, normalized set of additions/deletions. A commit has zero or one parent in Milestone 0001, a patch ID, and deterministic metadata. The service writes the patch and commit before advancing `main`; advancement supplies the expected current head. Missing parents and targets are rejected. A failed CAS leaves immutable unreachable objects but never moves the ref.

Reconstruction follows the single-parent chain to genesis, applies patches oldest-first to an RDF quad set, and returns sorted canonical N-Quads. Deleting an absent quad and adding an existing quad are idempotent set operations.

## Protocol
IDs are lowercase `sha256:<64 hex>` values over exact canonical bytes. Patch and commit encodings are version-tagged in [canonicalization](../design/canonicalization.md). Protocol vectors are repository-owned compatibility fixtures. Anonymous blank nodes are rejected at ingress until a future explicit skolemization protocol is accepted.

## API slice
The initial server exposes health, current `main`, commit creation with an expected head, and state retrieval by commit. A stale expected head maps to HTTP 409 with stable code `HEAD_CHANGED`. It has no RDF query language.

## Later capabilities
General branches, multi-parent merges, semantic validation coordination, checkpoints, richer provenance, and projection delivery require later execution plans. See [architecture](../../ARCHITECTURE.md), [data model](../design/data-model.md), and [ADRs](../decisions/README.md).

## Normative evolution model

The following requirements remain authoritative even when their implementation is assigned to a later execution plan. “Planned” does not mean optional.

### Datasets and changes

A ledger state is an RDF dataset: a default graph plus zero or more named graphs. Persistent statements are quads. Ingress MUST parse standards-conforming N-Quads and serialize accepted terms into one canonical lexical form before hashing. Prefixes are presentation metadata and do not affect identity. Blank nodes MUST be rejected or transformed by a future accepted, deterministic skolemization protocol before persistence; transient virtual A-box data is outside this rule because it is never committed.

A patch is the immutable, content-addressed set of RDF additions and deletions. Duplicate operations collapse, a quad cannot be both added and deleted in one patch, and operation order in caller input cannot affect `PatchId`. Applying a patch uses set semantics. Object writes are idempotent and a digest collision with different bytes is corruption.

### Commits and ancestry

Protocol v1 commits contain an ordered list of zero, one, or two parents. Zero parents denotes genesis, one denotes a linear commit, and two denotes a merge. Parent zero is the state/reconstruction parent: a merge patch is computed from the first-parent state to the merged state. Parent one records the other ancestry. Both parents MUST exist before reachability, MUST be distinct, and their ordering is identity-bearing.

The initial v1 envelope contains the patch ID, actor, message, event time, and ledger-controlled recorded time. This is deliberately minimal provenance, not the final provenance model. Adding identity-bearing provenance, semantic-context, validation, graph, or activity fields requires a versioned commit-envelope migration (expected v2), compatibility vectors, and an ADR; implementations MUST NOT append fields silently to v1.

Commit creation is staged: all referenced immutable content is durably published first, then an accepted branch ref may advance using CAS. A failed CAS may leave unreachable immutable content but MUST NOT change the ref. General branch creation and merge commits are planned capabilities, not part of the bootstrap HTTP surface.

### Temporal semantics

`event_time` describes when the represented domain event happened. It may be supplied by trusted input and MUST NOT be interpreted as commit order. `recorded_time` is the ledger-assigned time at which the immutable commit envelope is constructed and MUST be generated by the ledger, not a client. Exact accepted-branch chronology belongs to a later server-generated ref event because CAS occurs after commit identity is fixed. Independent branches may have overlapping or out-of-order event times. Commit ancestry and ref events, not timestamps, establish ledger ordering.

The eventual API MUST support both questions without conflation:

- what the accepted branch believed at a prior ledger point; and
- what current knowledge says about a prior event time.

### Provenance

The complete model distinguishes:

- commit provenance: why graph state changed;
- actor provenance: the authenticated human, agent, or service responsible;
- source provenance: evidence supporting the proposed change;
- fact provenance: why a particular assertion is believed;
- validation provenance: ontology, shapes, reasoner, rules, and result used;
- decision provenance: why a proposal was accepted or rejected; and
- temporal provenance: event time versus recorded time.

W3C PROV-O SHOULD be the interoperability model for exported provenance. Commit provenance and fact-level provenance are distinct: the ledger MUST NOT prescribe a single fact-provenance representation. Evidence identifiers and authenticated actor identity must cross the service boundary explicitly; free-form messages are not an audit identity.

### Validation and acceptance

The ledger coordinates validation but does not implement semantics. A candidate state is reconstructed from immutable history and supplied to the Sculpin semantic validation/reasoning layer with explicit ontology/shapes versions and any allowed transient virtual A-box context. That layer owns SHACL, OWL/RDFS reasoning, and domain rules; its implementation (today pySHACL and Python reasoning workers, possibly Jena later) is opaque to the ledger (ADR-0014). A validation record eventually MUST contain at least candidate commit/state identity, semantic-context identifiers, validator identity/version, outcome, recorded time, and an immutable report reference or digest.

Validation failure MUST NOT corrupt existing history. Rejected proposals and their validation/decision provenance SHOULD remain auditable without moving an accepted ref. Whether a policy permits commits while semantic validation is unavailable must be explicit per branch/workflow; projection availability alone MUST NOT decide ledger validity.

### Refs, branch events, and history

Branches are named mutable references to immutable commits. Every create, advance, fast-forward, merge, delete, or administrative repair MUST be an atomic CAS operation and eventually produce an auditable ref event. Force updates and silent rebases are prohibited. Deleting or moving a ref does not delete commits. Garbage collection, when introduced, requires an explicit retention policy and must never race reachability publication.

### Three-way merge semantics (planned)

Merge uses a bounded common-ancestor search and three graph states: base, target, and source. Fast-forward and already-contained cases do not synthesize unnecessary content. Divergent histories produce a two-parent commit whose first parent is the target head and whose patch transforms the target state into the candidate merged state.

Structural conflict detection operates on RDF changes, including competing changes to the same subject/predicate/graph. RDF permits multiple objects, so structural overlap alone is not necessarily a semantic conflict. Sculpin's semantic validation layer evaluates the merged candidate with SHACL, reasoning, and domain rules. Merge preview MUST be side-effect free; merge apply MUST re-check both expected heads/ref preconditions and fail rather than applying a stale preview.

### Projection behavior

Fuseki is a disposable query projection, never the ledger system of record. Accepted ref movement and durable projection intent will eventually be committed atomically through an outbox or equivalent mechanism. Projection delivery MUST be idempotent, retryable, ordered per projection, and observable. A projection tracks/verifies the exact commit it represents. Failure, lag, or rebuild of Fuseki MUST NOT roll back, rewrite, or invalidate ledger history. Rebuilding from immutable history/checkpoints must produce the same RDF dataset.

### Checkpoints and reconstruction

Patches are authoritative. Checkpoints are derived acceleration objects keyed by the commit/state digest and may be discarded and rebuilt. Reconstruction chooses a verified checkpoint and applies the deterministic first-parent patch sequence. A corrupt checkpoint MUST be detected and ignored or quarantined; it cannot redefine commit state. Traversal, bytes, quads, and wall time MUST be bounded before exposing reconstruction to untrusted callers.

### API and error semantics

The eventual resource model covers graphs, commits, states, refs/branches, diffs, merge preview/apply, validation records, history, and projection status. It does not expose a ledger-owned query language. Mutation calls carry expected ref heads and idempotency semantics. Stable conflict categories include malformed input, missing immutable content, `HEAD_CHANGED`, merge conflict, validation rejection/unavailability, traversal/resource limit, and internal corruption. Authentication happens at the service boundary; authorization policy and authenticated actor provenance must not be inferred from client-supplied text.

### Resource and security limits

Before production use, configurable limits MUST cover HTTP body size, patch operation count, term and metadata length, ancestry depth/work, reconstructed quad/byte count, merge traversal, validation report size, concurrent writers, and processing time. Randomized/stress failures must emit replayable seeds. RDF and metadata parsers are untrusted-input boundaries. Object paths derive only from parsed IDs; deployments must protect the data root from symlink/external mutation and keep credentials outside repository/config artifacts.

### Storage and durability

Immutable object durability and mutable-ref durability have distinct implementations but the same acknowledgement rule: data and containing directory entries MUST be synchronized before success is returned. A branch must never expose a commit whose patch or parents are absent. PostgreSQL transactional CAS for the mutable ref is implemented (ADR-0007), and a shared PostgreSQL immutable store with a verified commit index is implemented behind the version-neutral `ImmutableStore` interface (ADR-0012). A shared ref MUST NOT reference node-local content: the filesystem immutable backend is single-host only, and only the shared backend is a supported multi-replica topology. PostgreSQL stores mutable ref/outbox metadata and, in the shared backend, immutable content; the interface keeps object-store backends possible.

### Compatibility and migrations

Canonical patch bytes, commit envelopes, ID algorithms, first-parent rules, and blank-node policy are persistent protocol. Released migrations and protocol vectors are immutable. Any intentional identity change requires a new protocol version, ADR, migration/dual-read strategy, explicit golden-vector change, and compatibility tests. Unknown versions fail closed.

## Quality roadmap

Real PostgreSQL two-connection CAS tests and HTTP commit/state/restart tests against the deployed container are delivered baseline gates. In addition to those and the other bootstrap gates, production milestones MUST add:

- migration upgrade tests;
- property tests for normalization, determinism, and DAG invariants with replayable seeds;
- differential semantic-state scenarios against a digest-pinned external reference;
- projection retry, duplicate delivery, lag, and rebuild tests;
- merge fast-forward, already-contained, divergent, repeated-merge, and stale-preview tests;
- malicious RDF/metadata, resource exhaustion, object corruption, and path/symlink tests;
- dependency/license audit, SBOM and container scanning with classified findings;
- fault tests at object publication, ref transaction, projection delivery, and restart boundaries; and
- benchmark baselines varying state size, commit count, patch size, branch count, writers, and DAG shape, reporting p50/p95/p99, throughput, CPU, RSS, and storage.

Fluree timings remain diagnostic only. Release performance gates compare the Cognitive Ledger against its own accepted baseline.
