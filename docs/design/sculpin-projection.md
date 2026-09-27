# Reading the accepted-state projection (Sculpin integrator contract)

Normative protocol: [ADR-0020](../decisions/ADR-0020-accepted-state-projection-protocol.md).
Operations: [deployment § projection](../operations/deployment.md). This page states what a
Sculpin component that **reads** the projection may rely on and must not do.

## What is in the target dataset
| Graph | Content | Sculpin |
|---|---|---|
| `urn:sculpin:kb:<pct(kb_id)>:cognitive` | exactly the ledger's accepted state of the ledger graph enabled for that KB, at the version named by the marker | read with `GRAPH <G>` |
| `urn:sculpin:ledger-projection:v1:markers` | one marker per cognitive graph, and the dataset's target binding (subject `urn:sculpin:ledger-projection:v1:target`, `lp:targetId`) | read only, for freshness |
| `urn:sculpin:ledger-projection:v1:probe` | empty except transiently during a start-up probe | ignore |
| anything else (base KB, sources) | Sculpin's own, never written by the ledger | yours |

`pct` keeps `A–Z a–z 0–9 - . _ ~` and writes every other UTF-8 byte as uppercase `%XX`; the
reference implementation and frozen vectors are in `crates/ledger-projection/src/target.rs`.

## Rules for readers
- **Query by graph.** Address the cognitive graph explicitly (`GRAPH <G> { … }` or `FROM
  NAMED`). No service over the dataset — the projector's or any reader's — may use a union
  default graph (`tdb2:unionDefaultGraph`): every cognitive graph, the markers and the
  target binding would merge into the default graph. The projector refuses to start when it
  can see the binding in the default graph; a separate reader service over the same TDB2
  location is the operator's responsibility.
- **Never write** the cognitive, marker or probe graphs, with any credential. The projector
  owns them: the next write for the stream replaces the whole graph, so any foreign write is
  lost. Before that, reconciliation repairs only edits that change the graph's triple count;
  a count-preserving substitution, or a forged marker, stays visible to readers until the
  next write or until an operator runs `ledger-projector verify` / `rebuild`. The target is
  trusted as far as its access control is (ADR-0020).
- **Freshness.** The projection may lag the ledger (asynchronous; the ledger never waits for
  the target). The marker states which ledger state the graph represents:
  ```sparql
  PREFIX lp: <urn:sculpin:ledger-projection:v1#>
  SELECT ?graphId ?branch ?commit ?version WHERE {
    GRAPH <urn:sculpin:ledger-projection:v1:markers> {
      <G> lp:graphId ?graphId ; lp:branch ?branch ; lp:commitId ?commit ; lp:refVersion ?version } }
  ```
  Ref versions count per ledger graph and ref, and a restored ledger reuses lower numbers, so
  a version alone proves nothing. A caller that needs "at least the state I just accepted"
  (accept response `head`, `ref_version`, on graph `g`, ref `main`) may treat the projection
  as fresh only if `?graphId = g`, `?branch = "main"` (v1 projects `main` only), and either
  `?version = ref_version` and `?commit = head`, or `?version > ref_version` and the ledger
  confirms `?commit` is its `main` head or an ancestor at that version (e.g. `GET
  /v1/graphs/{g}/refs?name=main`). Otherwise wait, or read the ledger's own state (`GET
  /v1/graphs/{g}/commits/{head}/state`). Never order by timestamps.
- **`lp:stateDigest`** is the digest of the ledger's accepted state (`sculpin-rdf-state/v1`
  over the exact accepted lexical forms); it cannot be recomputed from the target's content
  (below) and identifies what was projected, not what the target returns.
- **Literal forms.** TDB2 stores many literals by value: `"01"^^xsd:integer` reads back as
  `"1"`, `"1.50"^^xsd:decimal` as `"1.5"`, language tags lowercase. Compare by SPARQL term
  equality, not by lexical form; the ledger (not Fuseki) is the record of the exact accepted
  lexical forms.
- **Only accepted state.** The projection never contains proposals, rejected candidates,
  validation-time Virtual A-Box hydration, reasoning output or temporary validation graphs.
  Validation (`docs/design/validation-protocol.md`) works on the candidate state the ledger
  sends, never on this projection.
- **Default graph only (v1).** Ledger named-graph quads are not projected; a stream whose
  state contains them blocks visibly (`NAMED_GRAPH_UNSUPPORTED`) instead of dropping them.
- **One ledger graph per KB per target.** Which ledger graph feeds a KB is an operator
  decision (`ledger-admin projection enable`); `lp:graphId` names it.

## What readers can observe when something is wrong
| Symptom | Meaning |
|---|---|
| marker absent, graph empty | stream not enabled yet, or its first projection is pending |
| marker `refVersion` lower than the ledger head | lag (target outage, backlog, stream blocked); see `ledger-admin projection status` |
| marker names another `graphId` than expected | the KB's feed was switched and awaits an operator rebuild (`TARGET_CONFLICT`) |
| marker `refVersion` above the ledger head | the ledger was restored to an older point; the stream waits for an operator (`MARKER_AHEAD`) |
