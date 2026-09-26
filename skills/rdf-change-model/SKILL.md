---
name: rdf-change-model
description: Use for RDF Patch, N-Quads, term normalization, blank-node/skolemization behavior, or dataset state comparison.
---

# Work with RDF change protocol

Read `docs/design/canonicalization.md`. Normalize before hashing; compare semantic datasets as sorted canonical quad sets, not textual input or commit IDs. Reject persistent anonymous blank nodes unless an accepted skolemization protocol exists. Test escaping, ordering, duplicates, conflicting operations, malformed terms, and idempotence. Do not implement SPARQL or reasoning.
