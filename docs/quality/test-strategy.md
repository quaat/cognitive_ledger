# Test strategy

Unit tests cover IDs, canonical bytes, RDF normalization, ordering, parsing, and error cases. Golden integration tests pin protocol fixtures. The walking-skeleton test covers two commits, historical reconstruction, stale CAS, and reopen persistence. API tests cover health and conflict mapping. Random/property tests must be deterministic or print replayable seeds.

Docker integration (`scripts/test-integration.sh`) covers the PostgreSQL CAS race, two replicas on the shared PostgreSQL immutable store, and HTTP commit/reconstruction across a service restart; PostgreSQL-backed tests are `#[ignore]`d in the unit gate and never reported as passed when not run; differential tests compare normalized semantic RDF state, never IDs. `stress/` and `fault/` are reserved for explicit later plans. Assertions are not weakened to accommodate defects.
