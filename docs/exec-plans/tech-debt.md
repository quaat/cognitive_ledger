# Technical debt and deferred work

- Replace host-local ref locking with PostgreSQL transactional CAS and a real two-writer integration test.
- Complete RDF 1.1 parsing, term normalization, and a stable skolemization protocol with hostile-input limits.
- Pin the official Fluree reference image by digest and implement/run the semantic-state adapter.
- Add Fuseki/Jena candidate validation and projection retry integration without coupling it to history.
- Add OpenAPI, authentication/authorization boundary, request/body and ancestry depth limits.
- Evaluate `cargo-deny`, `cargo-audit`, SBOM, and container scanning with classified findings.
- Establish benchmark baselines and checkpoint policy before performance gates.
