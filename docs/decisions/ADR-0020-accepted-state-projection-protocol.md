# Accepted-state projection protocol: target graph identity, marker and write semantics

## Status
Accepted (2026-09-27, Plan 0007 / Phase 3; amended before first release on 2026-09-28 by
the Phase-3 review rounds: compare-and-swap writes on the exact observed marker (round 2,
replacing round 1's version ceiling), target-computed triple count, stream-conflict
recovery, dataset binding and union-default-graph refusal, reconciliation, `main`-only v1). Freezes the
external projection protocol (`sculpin-ledger-projection/v1`): the cognitive graph IRI, the
marker representation, the dataset binding and the guarded-write rules are persistent
identity in a store the ledger does not own; changing them after release needs a
superseding ADR and new vectors.

## Context
Phase 3 makes the ledger's **accepted** state queryable by Sculpin through Fuseki. The ledger
stays authoritative (spec invariants; ADR-0013 writes a `projection_outbox` row in the same
transaction as every accepted ref movement); Fuseki is a derived view that may lag, fail or
be rebuilt, and must never be read back into ledger history. The projector must always be
able to say which exact ledger state a target graph represents, survive crashes at any point,
tolerate duplicate delivery and concurrent workers, and never let an older state overwrite a
newer one.

Evidence gathered before deciding (Plan 0007 "Discoveries"), all against Apache Jena Fuseki
5.1.0 (pinned image) on a TDB2 dataset:
- one SPARQL Update request with several `;`-separated operations runs in **one write
  transaction** — a request whose later operation fails (`LOAD` of an unloadable IRI) returns
  500 and leaves no effect of its earlier `INSERT` (source: `SPARQL_Update.execute`;
  reproduced). A plain `ja:RDFDataset` has no abort and would not give this guarantee. Graph
  Store Protocol writes address one graph per request. Fuseki has no cross-request
  transactions;
- TDB2 stores many literals **by value**: `"01"^^xsd:integer` is stored and returned as
  `"1"` (and merges with a distinct ledger triple `"1"^^xsd:integer`), `"1.50"^^xsd:decimal`
  as `"1.5"`, `@EN` as `@en`, `"1"^^xsd:boolean` as `"true"`. A target graph therefore cannot
  be compared with the ledger state byte for byte, and its triple count can be lower than the
  ledger state's;
- anonymous updates are refused (401) with the shipped dataset configuration; each commit
  costs ~0.5–1 s on the qualification host (single writer).

## Decision

### Only accepted state; one stream per (graph, ref, target); `main` only in v1
A **projection stream** is `(graph_id, branch, target_id)`, enabled explicitly by an operator
(ADR-0021). The projector materializes the accepted state at the stream's ref — nothing from
proposals, rejected candidates, validation-time Virtual A-Box hydration, reasoning output or
temporary validation graphs. Effective Sculpin state remains base KB + accepted cognitive
projection + transient Virtual A-Box + transient derived facts.

v1 projects only the `main` ref: the cognitive graph IRI carries no ref, so a second ref of
the same graph would need its own identity rule. Enabling any other ref is refused.

### Target graph identity (`sculpin-ledger-projection/v1`)
- Input: the ledger graph's `knowledge_base_id` (`graphs.knowledge_base_id`; a bounded opaque
  token, ADR-0010). A graph without one **cannot be enabled**: no identifier is invented.
- Cognitive graph IRI: `urn:sculpin:kb:` + `pct(kb_id)` + `:cognitive`, where `pct`
  percent-encodes every UTF-8 byte outside `A–Z a–z 0–9 - . _ ~` as `%XX` (uppercase hex) and
  keeps those unreserved bytes verbatim. The mapping is injective and round-trippable (decode
  `%XX` back to bytes, then UTF-8); no normalization is applied, so two distinct KB ids
  never share a graph. Example: `urn:exodus:kb:material-science` →
  `urn:sculpin:kb:urn%3Aexodus%3Akb%3Amaterial-science:cognitive`.
- **One cognitive graph per KB per target.** ADR-0010 allows several ledger graphs to carry
  the same `knowledge_base_id`; for projection the database refuses two *non-disabled*
  streams of one target naming the same cognitive graph (partial unique index, ADR-0021), so
  two ledger graphs — of the same or of different tenants — never project into one Fuseki
  graph at the same time. Which ledger graph feeds a KB is the operator's choice at enable
  time; switching it is disable + enable + rebuild (the target then holds the previous
  feed's marker, which the new stream must not overwrite silently — `TARGET_CONFLICT`
  below).
- Marker graph IRI (per target dataset, reserved): `urn:sculpin:ledger-projection:v1:markers`.
  Probe graph (transactional check only, always emptied): `urn:sculpin:ledger-projection:v1:probe`.
- The projector writes nothing else: never Sculpin's base/source graphs, never the default
  graph.

### Dataset binding
A dataset serves exactly one `target_id`. The marker graph holds
`<urn:sculpin:ledger-projection:v1:target> lp:targetId "<target_id>"`, inserted on first use
only if absent. Every projector process that writes (run, rebuild) binds or verifies the
binding at start-up and refuses to start with `TARGET_CONFLICT` when the dataset is bound to
another id or carries more than one binding, and with `TARGET_PROTOCOL` when the binding is
visible in the dataset's default graph (a union default graph). The binding is keyed by the
operator-chosen `target_id` only: two deployments configured with the **same** id (e.g. a
staging copy restored from a production backup) both pass it, and each would see the
other's markers as `MARKER_COMMIT_MISMATCH` or `MARKER_AHEAD`. A restored or cloned ledger
must get its own `target_id` and dataset (runbook); the binding is not re-checked after
start-up (residual, tech debt). Two deployments (two ledgers, or one ledger
with two target ids) pointed at one dataset therefore cannot both write it. `target_id` is
restricted to `[A-Za-z0-9._:-]{1,128}` (database CHECK and client validation), so it is
interpolated into SPARQL only as a validated token.

### Marker
The marker is a set of triples in the marker graph whose subject is the cognitive graph IRI
`<G>` and whose predicates are in the namespace `urn:sculpin:ledger-projection:v1#` (`lp:`):
```
<G> lp:protocol    "sculpin-ledger-projection/v1" ;
    lp:graphId     "<ledger graph_id>" ;
    lp:branch      "<ref name>" ;
    lp:commitId    "sha256:<commit id>" ;
    lp:refVersion  "<n>"^^xsd:integer ;
    lp:stateDigest "sha256:<sculpin-rdf-state/v1 digest of the ledger state>" ;
    lp:writeId     "<unique per write and per fence>" ;
    lp:tripleCount "<n>"^^xsd:integer .
```
A marker is **well formed** only if every predicate (`lp:writeId` included) appears exactly once with the stated
datatype (plain string literals without a language tag for the string fields), no other
`lp:` predicate is present, `protocol` is the v1 value and the identifiers parse under the
ledger's own rules. `lp:stateDigest` is the digest of the **ledger's** accepted state (the
identity of what was projected), not of the target's canonicalized copy. `lp:tripleCount` is
the **target's own count** of `<G>`, computed by the target in the same transaction as the
write (below), so literal canonicalization never makes a correct projection look edited.

The marker is a correctness primitive, not metadata: the projector's decisions depend only
on it and on ledger facts, never on timestamps. It is trusted as far as the target is: the
projector's credential is the only writer of the marker graph in a correct deployment
(`docs/operations/deployment.md`); a party that can write the target can forge a marker, and
periodic reconciliation plus `verify` (below) are the detection controls, not prevention.

### State-based projection and the compare-and-swap write
v1 always materializes the **full accepted state** at a ref version `N` (no incremental patch
application): the projector reconstructs the state at the event's commit through the bounded
reconstruction interface, observes the target (marker terms and the graph's triple count in
one query), plans (decision table below), and sends **one** SPARQL Update request whose
precondition is the observation itself:
```
DELETE WHERE { GRAPH <M> { <W> lp:writeFor ?any } } ;                        # clear stray tokens
INSERT { GRAPH <M> { <W> lp:writeFor <G> } } WHERE { CAS(observed) [VERSION(N)] } ;
DELETE { GRAPH <G> { ?s ?p ?o } }   WHERE { TOKEN GRAPH <G> { ?s ?p ?o } } ;
INSERT { GRAPH <G> { …state… } }    WHERE { TOKEN } ;                         # omitted if empty
DELETE { GRAPH <M> { <G> ?p ?o } }  WHERE { TOKEN GRAPH <M> { <G> ?p ?o } } ;
INSERT { GRAPH <M> { …marker(N) without tripleCount… } } WHERE { TOKEN } ;
INSERT { GRAPH <M> { <G> lp:tripleCount ?n } } WHERE { TOKEN
         { SELECT (COUNT(*) AS ?n) WHERE { GRAPH <G> { ?s ?p ?o } } } } ;
DELETE WHERE { GRAPH <M> { <W> lp:writeFor ?any } }                           # token removed
TOKEN      = GRAPH <M> { <W> lp:writeFor <G> }      (<W> = urn:sculpin:ledger-projection:v1:write)
CAS(obs)   = GRAPH <M> { <G> p1 o1 . … <G> pk ok . }
             FILTER NOT EXISTS { GRAPH <M> { <G> ?cp ?co }
                                 FILTER (!((?cp = p1 && sameTerm(?co, o1)) || …)) }
             (an empty observation: FILTER NOT EXISTS { GRAPH <M> { <G> ?cp ?co } })
VERSION(N) = FILTER NOT EXISTS { GRAPH <M> { <G> lp:refVersion ?v } FILTER (COALESCE(?v >= N, true)) }
```
The **compare-and-swap** (CAS) holds only while the marker subject carries exactly the
observed (predicate, object) terms, compared by the target's own term identity, with nothing
added or removed. A write planned from an observation that is no longer current — another
version landed, another stream's marker arrived after a feed switch, a replacement already
repaired a malformed marker, an operator cleaned the subject — inserts no token, so every
data operation is a no-op. This makes any late, stalled or duplicated write harmless
regardless of lease state, across streams and feeds (review round 2: a version-only guard let
a disabled feed's in-flight write, or a queued stale replacement with a high ceiling, land
over a newer projection of another stream). The normal path adds `VERSION(N)` (no
`refVersion` ≥ `N`, a non-numeric one also blocks) as defence in depth; a recovery
replacement has no version rule: it replaces exactly what it observed, including garbage and
out-of-range values.

The token is transaction-local: the first operation clears any stray token, the last one
deletes it, and readers never see it (one transaction). The count operation adds
`lp:tripleCount` from the target's own count of the graph just written. Either every data
operation applies or none does. Terms the adapter cannot write as exact SPARQL constants — a
blank node, an IRI or datatype containing a character `IRIREF` forbids, an invalid language
tag, or more than 64 values — make the write fail with `TARGET_PROTOCOL` (permanent: an
operator clears the marker subject by hand); literal values are escaped with `ECHAR`s only,
never `\u` escapes, which the target decodes before parsing. Ledger state is blank-node free
(ADR-0003), so the state templates are exact.

v1 projects the ledger dataset's **default graph**. A state containing quads in a named graph
is refused with `NAMED_GRAPH_UNSUPPORTED` (the stream blocks visibly); mapping ledger named
graphs to target graphs needs its own identity rules and a superseding ADR.

### Decision table (after reading the marker and the target graph's triple count in one query)
Let `T = (commit, N)` be the work item (the latest accepted outbox event beyond the recorded
projection; the recorded version on reconciliation; the ref head on an operator rebuild),
`H` the ledger's head version and `L(v)` the ref's commit at version `v`.
| Observation | Action |
|---|---|
| no marker, empty graph, nothing recorded | conditional write of `T` (first projection) |
| no marker, empty graph, a projection was recorded | rebuild (`TARGET_LOST`) |
| no marker, populated graph | rebuild (`UNMARKED_CONTENT`) |
| malformed marker whose highest parseable `refVersion` is `> H` | **recovery** `rebuild_required` (`MARKER_AHEAD`): may be a newer protocol's or ledger's marker |
| other malformed marker (including out-of-range numeric values) | rebuild (`MARKER_MALFORMED`); the replacement names the observed terms exactly |
| well-formed marker of **another** graph/branch | **recovery** `rebuild_required` (`TARGET_CONFLICT`): never overwritten automatically |
| marker version `> H` | **recovery** `rebuild_required` (`MARKER_AHEAD`): never regressed automatically |
| marker version `v ≤ H`, commit `≠ L(v)` | rebuild (`MARKER_COMMIT_MISMATCH`) |
| marker count `≠` observed count (edited out of band) | rebuild (`TRIPLE_COUNT_MISMATCH`) |
| marker `v < N` | conditional write of `T` |
| marker `v = N` | already projected: acknowledge |
| marker `N < v ≤ H` | target already beyond: acknowledge up to `v` |

A rebuild uses the replacement write, is counted (`projection_rebuilds_total`, and
`rebuilds` on the stream) and logged with its reason. The `N < v ≤ H` row is defensive: work
is always the latest outbox event and enable refuses heads without one, so a marker beyond
`N` within history can only appear through an out-of-band edit or a restore.
`MARKER_COMMIT_MISMATCH` rebuilds automatically because the marker is this stream's and
within the ledger's history, so the ledger (authoritative) decides; a foreign or ahead
marker may belong to a newer or different ledger and needs an operator. An operator rebuild
(`ledger-projector rebuild`) replaces whatever it observed (any marker, including an ahead or
foreign one), still under CAS, so it cannot overwrite something that changed after its own
observation — with one exception: if the observed marker is this stream's and names the
ledger's own commit at a version above the rebuild's work item, the work item is stale (its
claim reply was delayed while another worker projected on; Codex round 2) and the rebuild
stops as superseded instead of regressing it.

After every write the projector reads the marker and count back in one query: the marker
must name `T`, its count must equal the observed count and be plausible (`0 < count ≤ state
size`, or `0` for an empty state); a newer marker of this stream means the write was
superseded (harmless, released). Rebuilds additionally prove **containment**: one `ASK`
whose pattern is the complete ledger state, evaluated by the target's own term equality, so
canonicalized literals match. Containment plus the target-computed count bounds out-of-band
additions; an out-of-band *replacement* that preserves the count is detected by `verify` and
by reconciliation's re-observation only when it changes the count — full content comparison
(`verify`) is the operator control for that residual.

### Write ids, fencing and authority changes (review round 3)
The compare-and-swap alone is not ABA-free: a write planned from marker `M` would apply again
whenever the marker returns to exactly `M` — e.g. feed B's operator rebuild observes feed A's
marker and stalls, B is disabled and A re-enabled, A's marker is unchanged, and B's late write
would pass (Codex review, P0). Two rules close it:
- **Every write carries a fresh `lp:writeId`** (the digest of the lease, a process counter and
  the clock; unique, not secret), so marker terms never repeat and an observation can never
  become current again once anything wrote.
- **An authority change changes the target.** Disabling a stream is two-phase (ADR-0021): the
  stream becomes `disabling` and keeps its cognitive graph; a projector claims it (after any
  in-flight step's lease is released or expired), **fences** the target — rotates only the
  marker's `lp:writeId` under the compare-and-swap on what it observes now — verifies the new
  id reads back, and only then marks the stream `disabled`, which frees the graph for another
  stream. Every write planned under the old authority observed an older write id and is a
  no-op from then on; a write that landed *before* the fence landed while that stream still
  held the graph. A subject holding only a write id (a fenced graph without a marker) reads
  as no marker. `ledger-admin projection disable --unfenced` skips the fence (escape hatch
  for a target that is gone for good), with that guarantee explicitly waived.

### Reconciliation
The outbox only drives work when the ledger moves. So that a target that lost its data
(restart onto an empty volume, restore of an older target backup) or was edited out of band
is repaired without waiting for the next acceptance, each projector periodically claims one
**idle** stream whose last successful check is older than the reconcile interval, observes
the target and applies the decision table at the recorded version (acknowledging without a
write when consistent).

### Target requirements checked by the projector
- the dataset must be transactional with abort (TDB2 or the transactional in-memory dataset):
  at start-up and periodically the projector sends a probe request (`INSERT DATA` into the
  probe graph followed by a failing `LOAD` of an unloadable URN), requires the request to
  fail with HTTP 500 naming the `LOAD`, and refuses to run (pauses claiming) if the probe's
  insert survived; the probe graph is emptied afterwards;
- the dataset must **not** use a union default graph (`tdb2:unionDefaultGraph`): every
  cognitive graph, the markers and the binding would merge into default-graph queries. The
  projector refuses a service whose default graph shows the binding; a separate reader
  service over the same storage is the operator's responsibility;
- updates go to the configured SPARQL Update endpoint with credentials; endpoint URLs are
  deployment configuration, never request data (https required outside development).

### Error classes
Retryable: connection failure, timeout, HTTP 429, HTTP 5xx, PostgreSQL unavailable or
timing out, a read-back that does not show the write.
Permanent (the stream blocks with a stable code until an operator acts): authentication or
authorization refusal (401/403), 400/404/405/415 from the target, an unexpected response
shape or content type, `NAMED_GRAPH_UNSUPPORTED`, a state beyond the projection size limits,
a rebuild that fails containment. Recovery (`rebuild_required`): `MARKER_AHEAD`,
`TARGET_CONFLICT`.

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
- **An unguarded `DROP`/`INSERT DATA` rebuild.** Simpler, but a rebuild planned by a worker
  that stalls past its lease would overwrite a newer projection (review round 1).
- **Version-number guards (round 1: conditional `refVersion < N`, replacement under a
  ceiling).** Versions are per ledger graph and ref, so they order nothing across streams or
  feeds, and a ceiling taken from garbage or ahead values admits late regressions (review
  round 2). Superseded by the compare-and-swap on the exact observed marker.
- **Fencing the target by lease only** (e.g. keeping a disabled stream's lease until expiry
  before another stream may take its graph). Helps only for workers that honour the clock;
  the target-side precondition holds regardless of timing.
- **A ledger-computed `lp:tripleCount` and byte-exact comparison.** Wrong on TDB2, whose
  value canonicalization makes a correct projection look edited and would rebuild forever.
- **Timestamps for ordering or freshness.** Rejected everywhere in the ledger; `ref_version`
  is the order.
- **Deriving the target graph from the ledger `graph_id`.** Sculpin addresses knowledge by
  KB; the roadmap names one accepted cognitive graph per KB. The uniqueness rule keeps
  it unambiguous.

## Consequences
- Duplicate delivery, crashes before or after the target commit, lost responses, stale
  workers and concurrent workers are harmless by construction; leases (ADR-0021) are for
  exclusivity and efficiency, not correctness of the target state.
- Every projection writes the whole state; large states cost proportionally. Limits bound
  the request size; incremental application is a later optimization behind the same marker.
- Sculpin must read the projection by graph (`GRAPH <G>`) and may check freshness from the
  marker ([docs/design/sculpin-projection.md](../design/sculpin-projection.md)); it must never write the cognitive or
  marker graphs.
- Fuseki must be deployed with a transactional dataset, no union default graph and
  authenticated updates; the projector verifies the first and requires credentials for the
  last in production.
- The protocol strings, the IRI mapping, the marker predicates, the binding and write-token
  subjects are frozen by vectors in `crates/ledger-projection` tests; the request shape by
  `crates/ledger-projection-fuseki` unit tests; the behaviour against the pinned target by
  `apps/ledger-projector/tests/fuseki_projection.rs` (a mutation removing the precondition
  turns three of those tests red).
