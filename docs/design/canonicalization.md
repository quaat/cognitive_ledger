# Canonicalization protocol v1

Patch bytes begin `sculpin-rdf-patch-v1\n`. Each normalized operation is one line: `A ` or `D ` followed by a canonical N-Quad and newline. Operations sort by the complete encoded line and duplicate operations collapse. A quad uses absolute `<IRI>` subject/predicate, an IRI or escaped quoted literal object, and optional absolute graph IRI. This milestone rejects blank nodes, language/datatype literal suffixes, relative IRIs, control characters, and conflicting add/delete of one quad.

Commit bytes are a binary envelope beginning `sculpin-commit-v1\0`. Each field is emitted in fixed order as a big-endian `u32` byte length followed by UTF-8 bytes. The optional parent is encoded as an empty field. Fields are: parent, patch ID, author, message, event time, recorded time. No locale, map ordering, or wall clock enters canonicalization.

IDs are `sha256:` plus lowercase digest hex. Fixtures under `fixtures/golden/` pin logical input, inspectable canonical bytes, and expected IDs. Tests never rewrite them. A changed vector requires an ADR and migration/compatibility analysis.

This restricted subset is intentionally not a claim of full RDF Dataset Canonicalization. A future general RDF ingress must define deterministic parsing and skolemization before persisted blank-node-shaped input is accepted.
