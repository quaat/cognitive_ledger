# Test strategy

Unit tests cover IDs, canonical bytes, RDF normalization, ordering, parsing, and error cases. Golden integration tests pin protocol fixtures. The walking-skeleton test covers two commits, historical reconstruction, stale CAS, and reopen persistence. API tests cover health and conflict mapping. Random/property tests must be deterministic or print replayable seeds.

Docker integration will cover PostgreSQL/service restart; differential tests compare normalized semantic RDF state, never IDs. `stress/` and `fault/` are reserved for explicit later plans. Assertions are not weakened to accommodate defects.
