# Core beliefs

- Immutable content plus tiny CAS-protected refs is easier to audit than mutable state.
- Canonical bytes are a durable protocol, not an implementation detail.
- RDF semantics belong to Sculpin/Jena; query projection belongs to Fuseki.
- Fail closed before ref movement; projection is recoverable downstream work.
- Evidence accompanies claims: code, tests, docs, and plans are one deliverable.
- Add a boundary only for a current use case; do not grow a general graph database.
