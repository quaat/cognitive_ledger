# Cognitive Ledger product and technical specification

**Authority:** This document defines product scope and persistent invariants. Changes to an invariant require an accepted ADR and protocol-impact review.

## Purpose and boundaries
The Cognitive Ledger records immutable RDF changes, content-addressed commit history, mutable CAS-protected refs, provenance, historical reconstruction, and eventually three-way merge coordination. It is not a graph database.

| Owner | Responsibility |
|---|---|
| Cognitive Ledger | change history, immutable commits/DAG, refs, provenance, reconstruction and merge coordination |
| Sculpin/Jena | semantics, RDF reasoning, SHACL and domain rules |
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
General branches, multi-parent merges, semantic validation coordination, checkpoints, richer provenance, PostgreSQL refs, and projection delivery require later execution plans. See [architecture](../../ARCHITECTURE.md), [data model](../design/data-model.md), and [ADRs](../decisions/README.md).
