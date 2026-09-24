# Canonicalization protocol v1

Patch bytes begin `sculpin-rdf-patch-v1\n`. Each normalized operation is one line: `A ` or `D ` followed by canonical N-Quads and LF. The maintained `oxttl` standards parser validates each input and `oxrdf` serialization produces the stored lexical form. Operations sort by the complete encoded line; duplicates collapse; contradictory add/delete operations fail. Blank nodes in subject, object, or graph position fail before a `Quad` exists. `Quad` deserialization reuses this constructor, and `Patch` deserialization re-runs normalization.

Commit bytes begin `sculpin-commit-v1\0`. They contain a big-endian `u32` ordered-parent count (zero through two), each parent as a length-prefixed UTF-8 field, then fixed-order length-prefixed fields: patch ID, actor, message, event time, and ledger-controlled recorded time. Parent zero is the reconstruction parent; parent one is merge ancestry. Parents must be distinct. No locale, map ordering, or client wall clock enters canonicalization.

IDs are `sha256:` plus lowercase digest hex. Fixtures under `fixtures/golden/` pin logical input, inspectable canonical bytes, and expected IDs. Tests never rewrite them. ADR-0006 records the one pre-release v1 vector change from optional-parent encoding to ordered parents. After release, any changed vector requires a new protocol version, ADR, and migration/compatibility analysis.

Skolemization is deliberately not implemented. A future general RDF ingress may accept blank-node input only after a deterministic Sculpin-controlled skolemization protocol and hostile-input limits are accepted.
