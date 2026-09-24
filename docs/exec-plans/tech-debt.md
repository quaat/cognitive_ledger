# Technical debt and deferred work

- Replace host-local ref locking with PostgreSQL transactional CAS and a real two-writer integration test.
- Design a stable skolemization/import protocol and hostile-input limits around the standards N-Quads parser.
- Pin the official Fluree reference image by digest and implement/run the semantic-state adapter.
- Add Fuseki/Jena candidate validation and projection retry integration without coupling it to history.
- Add OpenAPI, authentication/authorization boundary, request/body and ancestry depth limits.
- Evaluate `cargo-deny`, `cargo-audit`, SBOM, and container scanning with classified findings.
- Establish benchmark baselines and checkpoint policy before performance gates.
