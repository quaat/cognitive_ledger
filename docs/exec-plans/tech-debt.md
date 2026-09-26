# Technical debt and deferred work

- Design a stable skolemization/import protocol and hostile-input limits around the standards N-Quads parser.
- Run the live Fluree differential adapter; the reference image is already digest-pinned (see test/reference-images.lock), so only running the semantic-state adapter remains, blocked pending BUSL-1.1 license sign-off.
- Add the Sculpin validation-service adapter (Phase 2, ADR-0014) and Fuseki projection retry integration (Phase 3) without coupling either to history.
- Add OpenAPI, authentication/authorization boundary, request/body and ancestry depth limits. Error bodies for `GRAPH_BINDING_CONFLICT`/`CROSS_GRAPH_PARENT` currently echo the other graph's id; once multi-tenant, redact or authorize before returning (P1.4).
- Graph import operator path (ADR-0010): register `status='importing'`, import, activate. Until it exists, migration 0004 fails closed on unowned graphs and `ledger-admin migrate-fs-to-pg` can only target `bootstrap`/`importing` graphs.
- PostgreSQL role separation: migrations run on the runtime pool, so the runtime role owns the tables and could disable the write-once triggers (migration 0005). Decide a migration-role vs DML-only runtime role split before production qualification.
- `ledger-admin migrate-fs-to-pg` loads the whole source store into memory (bootstrap scale only) and cannot catch up with a destination ref that has moved past the source HEAD (it is a cutover tool; live writes must stop first).
- P1.3 composite FK `refs(graph_id, head) → commit_index(graph_id, id)`: until it lands, ref-target graph agreement is enforced in application code (`advance_ref` typed check, migration graph check), not by the schema.
- Evaluate `cargo-deny`, `cargo-audit`, SBOM, and container scanning with classified findings.
- Establish benchmark baselines and checkpoint policy before performance gates.
