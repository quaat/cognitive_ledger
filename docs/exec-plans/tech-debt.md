# Technical debt and deferred work

- Design a stable skolemization/import protocol and hostile-input limits around the standards N-Quads parser.
- Run the live Fluree differential adapter; the reference image is already digest-pinned (see test/reference-images.lock), so only running the semantic-state adapter remains, blocked pending BUSL-1.1 license sign-off.
- Add Fuseki/Jena candidate validation and projection retry integration without coupling it to history.
- Add OpenAPI, authentication/authorization boundary, request/body and ancestry depth limits.
- Evaluate `cargo-deny`, `cargo-audit`, SBOM, and container scanning with classified findings.
- Establish benchmark baselines and checkpoint policy before performance gates.
