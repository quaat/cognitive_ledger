# Accepted-state projection protocol: target graph identity, marker and write semantics

## Status
Accepted (2026-09-27, Plan 0007 / Phase 3). Freezes the external projection protocol
(`sculpin-ledger-projection/v1`): the cognitive graph IRI, the marker representation and the
conditional-write rule are persistent identity in a store the ledger does not own; changing
them needs a superseding ADR and new vectors.

## Context
Phase 3 makes the ledger's **accepted** state queryable by Sculpin through Fuseki. The ledger
stays authoritative (spec invariants; ADR-0013 writes a `projection_outbox` row in the same
transaction as every accepted ref movement); Fuseki is a derived view that may lag, fail or
be rebuilt, and must never be read back into ledger history. The projector must always be
able to say which exact ledger state a target graph represents, survive crashes at any point,
tolerate duplicate delivery and concurrent workers, and never let an older state overwrite a
newer one.

Evidence gathered before deciding (Plan 0007 "Discoveries"): in Apache Jena Fuseki 5.1.0
(pinned image) on a TDB2 dataset, one SPARQL Update request with several `;`-separated
operations runs in **one write transaction** — a request whose later operation fails
(`LOAD` of an unloadable IRI) returns 500 and leaves no effect of its earlier `INSERT`
(source: `SPARQL_Update.execute`; reproduced against the pinned image). A plain
`ja:RDFDataset` has no abort and would not give this guarantee. Graph Store Protocol writes
address one graph per request. Fuseki has no cross-request transactions.

## Decision

### Only accepted state; one stream per (graph, ref, target)
A **projection stream** is `(graph_id, branch, target_id)`, enabled explicitly by an operator
(ADR-0021). The projector materializes the accepted state at the stream's ref — nothing from
proposals, rejected candidates, validation-time Virtual A-Box hydration, reasoning output or
temporary validation graphs. Effective Sculpin state remains base KB + accepted cognitive
projection + transient Virtual A-Box + transient derived facts.

### Target graph identity (`sculpin-ledger-projection/v1`)
- Input: the ledger graph's `knowledge_base_id` (`graphs.knowledge_base_id`; a bounded opaque
  token, ADR-0010). A graph without one **cannot be enabled**: no identifier is invented.
- Cognitive graph IRI: `urn:sculpin:kb:` + `pct(kb_id)` + `:cognitive`, where `pct`
  percent-encodes every UTF-8 byte outside `A–Z a–z 0–9 - . _ ~` as `%XX` (uppercase hex) and
  keeps those unreserved bytes verbatim. The mapping is injective and round-trippable (decode
  `%XX` back to bytes, then UTF-8); no normalization is applied, so two distinct KB ids
  never share a graph. Example: `urn:exodus:kb:material-science` →
  `urn:sculpin:kb:urn%3Aexodus%3Akb%3Amaterial-science:cognitive`.
- Marker graph IRI (per target dataset, reserved): `urn:sculpin:ledger-projection:v1:markers`.
  Probe graph (startup check only, always emptied): `urn:sculpin:ledger-projection:v1:probe`.
- The projector writes nothing else: never Sculpin's base/source graphs, never the default
  graph.
- The database refuses two streams on the same target naming the same cognitive graph
  (`UNIQUE (target_id, cognitive_graph)`, ADR-0021): two ledger graphs — of the same or of
  different tenants — can never project into one Fuseki graph.

### Marker
The marker is a set of triples in the marker graph whose subject is the cognitive graph IRI
`<G>` and whose predicates are in the namespace `urn:sculpin:ledger-projection:v1#` (`lp:`):
```
<G> lp:protocol    "sculpin-ledger-projection/v1" ;
    lp:graphId     "<ledger graph_id>" ;
    lp:branch      "<ref name>" ;
    lp:commitId    "sha256:<commit id>" ;
    lp:refVersion  "<n>"^^xsd:integer ;
    lp:stateDigest "sha256:<sculpin-rdf-state/v1 digest of the projected state>" ;
    lp:tripleCount "<n>"^^xsd:integer .
```
A marker is **well formed** only if every predicate appears exactly once with the stated
datatype, `protocol` is the v1 value and the identifiers parse under the ledger's own rules.
The marker is a correctness primitive, not metadata: the projector's decisions depend only
on it and on ledger facts, never on timestamps.

### State-based projection and the conditional write
v1 always materializes the **full accepted state** at a ref version `N` (no incremental patch
application): the projector reconstructs the state at the event's commit through the bounded
reconstruction interface, then sends **one** SPARQL Update request:
```
DELETE { GRAPH <G> { ?s ?p ?o } }  WHERE { GUARD(N) GRAPH <G> { ?s ?p ?o } } ;
INSERT { GRAPH <G> { …state… } }   WHERE { GUARD(N) } ;
DELETE { GRAPH <M> { <G> ?p ?o } } WHERE { GUARD(N) GRAPH <M> { <G> ?p ?o } } ;
INSERT { GRAPH <M> { …marker(N)… } } WHERE { GUARD(N) }
GUARD(N) = OPTIONAL { GRAPH <M> { <G> lp:refVersion ?v } } FILTER (!BOUND(?v) || ?v < N)
```
Within the one transaction the guard is evaluated against the pre-request marker by the first
three operations and against the (then removed) marker by the last, so either every
operation applies or none does. A write for version `N` is a no-op when the target already
represents `N` or a later version: duplicate delivery and a stale worker whose lease expired
can never move the marker backwards or overwrite newer state (reproduced: v1 → v2, then a
stale v1 write and a duplicate v2 write leave v2 untouched). An **unconditional rebuild**
(`DROP SILENT GRAPH <G>`, `INSERT DATA`, marker replaced) is used only by the explicit recovery
paths below. Ledger state is blank-node free (ADR-0003), so the templates are exact.

v1 projects the ledger dataset's **default graph**. A state containing quads in a named graph
is refused with `NAMED_GRAPH_UNSUPPORTED` (the stream blocks visibly); mapping ledger named
graphs to target graphs needs its own identity rules and a superseding ADR.

### Decision table (after reading the marker and the target graph's triple count in one query)
Let `T = (commit, N)` be the stream's latest accepted outbox event beyond the recorded
projection, and `L(v)` the ledger's head commit at version `v` of the stream's ref.
| Observation | Action |
|---|---|
| well-formed marker for this stream, `(commit, version) = T`, observed count `= lp:tripleCount` | already projected: acknowledge |
| well-formed marker for this stream, version `v < N`, commit `= L(v)` | conditional write of `T` |
| well-formed marker for this stream, version `N < v ≤` ledger head, commit `= L(v)` | target already beyond: acknowledge up to `v` |
| no marker and the cognitive graph is empty | conditional write of `T` (first projection) |
| marker version `>` ledger head version | **recovery state** `rebuild_required` (`MARKER_AHEAD`): never regress automatically; an operator rebuild decides |
| marker malformed, for another graph/branch, commit `≠ L(v)`, observed count `≠ lp:tripleCount` (graph edited out of band), or absent while the graph is populated | **rebuild**: unconditional replacement with `T`, counted and logged |

After every write the projector reads the marker and count back in one query: the marker must
equal `T` and `tripleCount` must equal both the projected state's size and the observed count;
otherwise the projection is ambiguous and the next attempt rebuilds. The explicit rebuild
operation additionally compares the complete target graph with the reconstructed state.

### Target requirements checked by the projector
- the dataset must be transactional with abort (TDB2 or the transactional in-memory dataset):
  at startup the projector sends a probe request (`INSERT DATA` into the probe graph followed
  by a failing `LOAD` of an unloadable URN) and refuses to run if the probe's insert
  survived; the probe graph is emptied afterwards;
- updates go to the configured SPARQL Update endpoint with credentials; the endpoint URL is
  deployment configuration, never request data.

### Error classes
Retryable: connection failure, timeout, HTTP 429, HTTP 5xx, PostgreSQL unavailable.
Permanent (the stream blocks with a stable code until an operator acts): authentication or
authorization refusal (401/403), 400/404/405/415 from the target, an unexpected response
shape or content type, `NAMED_GRAPH_UNSUPPORTED`, a state beyond the projection size limits.

## Alternatives considered
- **Incremental RDF-patch application.** Smaller writes, but correctness then depends on
  the target holding exactly the predecessor state; deferred until benchmarks justify it
  (the marker already encodes the predecessor check).
- **Graph Store PUT plus a separate marker write.** Not atomic across two graphs; an
  interrupted pair is exactly the ambiguity this protocol avoids.
- **Quad PUT of a TriG document to the dataset.** Atomic, but clears the entire dataset,
  including Sculpin's base graphs.
- **Marker triples inside the cognitive graph.** Mixes protocol data into the domain graph
  Sculpin queries and reasons over.
- **Timestamps for ordering or freshness.** Rejected everywhere in the ledger; `ref_version`
  is the order.
- **Deriving the target graph from the ledger `graph_id`.** Sculpin addresses knowledge by
  KB; the roadmap names one accepted cognitive graph per KB. The uniqueness constraint keeps
  it unambiguous.

## Consequences
- Duplicate delivery, crashes before or after the target commit, stale workers and
  concurrent workers are harmless by construction; leases (ADR-0021) are for exclusivity and
  efficiency, not correctness of the target state.
- Every projection writes the whole state; large states cost proportionally. Limits bound
  the request size; incremental application is a later optimization behind the same marker.
- Fuseki must be deployed with a transactional dataset and authenticated updates; the
  projector verifies the first and requires credentials for the second in production.
- The protocol strings, the IRI mapping and the marker predicates are frozen by vectors in
  `crates/ledger-projection` tests.
