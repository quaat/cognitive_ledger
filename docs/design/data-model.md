# Data model

- `ContentId`: validated algorithm-tagged SHA-256 identifier.
- `Patch`: normalized ordered `Add`/`Delete` operations over persistent RDF quads; `PatchId` is its content ID.
- `Commit`: protocol version, optional parent, patch ID, author, message, event time, recorded time. `CommitId` hashes canonical commit bytes.
- `main`: the only mutable ref in the walking skeleton; absence means empty ledger.
- State: a set of canonical N-Quads reconstructed from the parent chain.

Commit timestamps are caller-supplied protocol data and therefore affect identity. The ledger does not infer semantic meaning from metadata.
