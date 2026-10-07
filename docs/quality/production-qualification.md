# Production-qualification matrix

**Status: not production-qualified.** This matrix records what stands between the current
code and a production qualification. It was compiled read-only in Plan 0011 (Phase 6B) from
code and docs at `2a8a678`, then re-verified by an independent review at `795028e`. No
production code changed in between; only the benchmark harness, tests, CI and docs did.
Plan 0012 (Phase 6C) changed the retrieval code of reconstruction and ancestry walks
(`ledger-store`, `ledger-dag`) without a migration or an API change; the two rows it
affects are updated below. One behaviour to note before an upgrade: a `commit_parents`
position-0 row that contradicts the commit bytes now fails reconstruction (ADR-0025;
`deployment.md`, Runtime limits). Nothing here was run against a live Entra tenant, a Sculpin
service, or a production Fuseki or PostgreSQL. Rows marked *(inf)* are inferred from code,
not observed.

Categories:
- **code change**: the repository must change.
- **deployment-config evidence**: the deployment must supply configuration and evidence.
- **external Sculpin dependency**: needs a service the ledger does not own.
- **repository-owner action**: needs a GitHub setting the owner controls.
- **accepted residual risk**: documented and accepted (ADR or tech-debt).

Priorities: P1 means it blocks a qualification claim; P2 means fix before production load;
P3 means hardening.

| Item | Status and evidence | Category | P | Next action |
|---|---|---|---|---|
| Live Entra ID issuer | `OidcAuthenticator` is wired (`apps/ledger-server/src/main.rs`); the live-issuer test is still pending (Plan 0005, tech-debt). | deployment-config evidence | P1 | Smoke-test against a dev tenant and record the evidence. |
| PITR / WAL archiving | Required by ADR-0017 §1. The contradiction in `deployment.md` was fixed in Plan 0011. No archive configuration exists in the repository. | deployment-config evidence | P1 | Supply `archive_mode`/`archive_command`, or a streaming-replication standby (ADR-0017 §1 accepts either), and a restore-drill record. |
| Restore writer fencing | Procedure only (ADR-0017 §2). The ledger adds no epoch or fence. | accepted residual risk | P2 | Rehearse the runbook fencing step. |
| Projection reconciliation after restore | The projector reports `MARKER_AHEAD`, and the operator runs `ledger-projector rebuild`. | deployment-config evidence | P2 | Include rebuild-per-stream in the restore drill. |
| Live Sculpin validator | Only the fake validator is exercised (tech-debt). | external Sculpin dependency | P1 | Sculpin provides the endpoint of the design document. |
| Fuseki dataset configuration and write identity | The projector supports a user/password file or a token file. Compose uses the Fuseki `admin` account. ADR-0020 also requires a transactional dataset without `tdb2:unionDefaultGraph`, served over https (`deployment.md`). | deployment-config evidence | P1 | Provision an update-only account, show it is the sole writer, and record the dataset configuration. |
| Private CA for outbound HTTPS | https is enforced outside development. reqwest is built with `rustls-tls` and only `webpki-roots` (`Cargo.toml`, `Cargo.lock`): no OS trust store and no custom bundle. This holds for Fuseki, the validator, JWKS and `ledger-admin probe`. PostgreSQL is different: it honours `sslrootcert` in the DSN. | code change | P1 if any endpoint uses a private CA, else P2 | Add a CA-bundle setting. |
| Audited raw-import activation | No `ledger-admin` activation command exists. An activated imported graph has no `ref_events`, so the verifier fails it. | code change | P2 | Add an `import activate` command that writes the ref events. |
| Admission control for writes | `accept`, `merge_apply`, `reject` and the branch writes take no expensive-operation permit, unlike `prepare`. The defaults use the whole pool: 12 expensive + 4 validation slots = 16 = `max_connections`. Slots are not checked against the pool size. Plan 0013 M0 (2026-10-07) confirmed this and added: prepare holds its slot while waiting up to 10 s for a connection; `/ready` and the auth graph lookup need a connection too. M1 measured it (`pg_api` `p7a_*`): three blocked accepts fill a three-connection pool and the next read and `/ready` starve for the hard-coded 10 s acquire timeout, then 503; a key-less read is told to retry with an idempotency key. | code change | P2 | Plan 0013 M3 per ADR-0026 §5 (proposed): a `db_work` permit taken before the first pooled query bounds heavy connections to N − reserved_cheap; startup validation against the pool. Red test: `future_p7a_reserved_headroom_…`. |
| Cancel PostgreSQL work after an HTTP timeout | The edge drops the handler future only; there is no `pg_cancel_backend`. The default `statement_timeout` (30 s) equals the request timeout, and it is per statement, not per transaction. `idle_in_transaction_session_timeout` (60 s) is above the request timeout. PostgreSQL 17's `transaction_timeout` is unused. Plan 0013 M0 (2026-10-07) verified in the sqlx 0.8.6 source that a dropped statement pins its connection until PostgreSQL finishes it and that `ROLLBACK` is queued behind it, so locks are held meanwhile; M1 measured it (`pg_lifecycle`): pinned for the statement's remaining time, or released ≈ 0.1 s after `statement_timeout` when the limit cuts it (client-observed); a drop between statements rolls back within milliseconds; no transaction bound exists (three sub-limit statements run past any deadline); a late `pg_cancel_backend(pid)` hits the next borrower of the pooled session. | code change | P2 | Plan 0013 M2 per ADR-0026 §2–4 (proposed): validated hierarchy (`statement_timeout` 10 s < transaction bound 20 s < request 30 s), application transaction deadline + PostgreSQL 17 `transaction_timeout`, timeout-only cancellation (no `pg_cancel_backend`; the edge detaches admitted work instead of dropping it), PostgreSQL 17 `transaction_timeout` as a session-terminating backstop (measured: 25P04), `REQUEST_TIMEOUT` envelope. Red tests: `future_a_transaction_exceeding_its_bound_…`, `future_p7a_an_edge_timeout_after_commit_…`. |
| Zero-valued limits | The server rejects 0 for every limit and DB timeout (`env_usize`). The projector's `number()` accepts 0 for its target timeout and reconstruction limits, which fails closed and never progresses *(inf)*. | code change | P3 | Require ≥ 1 in the projector. |
| Server metrics / OpenTelemetry | The server logs with `tracing` only. Only the projector has `/metrics`, and it is unauthenticated. | code change | P2 | Add request, error, slot and pool metrics; decide metrics authentication. |
| Rate limiting | None in code: only global semaphores (not per tenant) and a body limit. | deployment-config evidence | P2 | Configure gateway rate limits; decide per-tenant fairness. |
| Secrets from files, DB TLS | The server and projector DB URLs come from the environment only (`LEDGER_DATABASE_URL`, `LEDGER_PROJECTOR_DATABASE_URL`). `ledger-admin` reads `LEDGER_MIGRATION_DATABASE_URL` and may also take `--database-url` on the command line. The HS256 secret is development-only, since production uses OIDC. The validator token-file reader checks the size, then calls an unbounded `read_to_string` (a FIFO bypasses the cap), and accepts a whitespace-only token. Compare the projector's `secret_file`. No `sslmode` is enforced. | code change | P2 | Add `_FILE` variants, one hardened reader, and a production `sslmode` check. |
| GitHub `main` ruleset | None (observed 2026-10-06). Use the required CI checks (benchmark-ci, container, dependency-review, docker, fast, fuzz, supply-chain). These are check-run (job) names; `fuzz` is the aggregate over the sanitizer matrix. `scripts/check-doc-consistency.py` keeps this list in sync. | repository-owner action | P1 | The owner creates the ruleset. |
| Production deployment configuration | None. `compose.yaml` is a development harness, and `deployment.md` says "not production-qualified". | deployment-config evidence | P1 | Write the production manifests and record their evidence. |
| Backup while projection is active | Not qualified (tech-debt). | code change (qualification script) | P2 | Add a projection-under-backup scenario to `scripts/backup-restore.sh`. |
| Offline verifier residuals | Orphaned detail rows after an FK drop and FK referential actions are unchecked. Merges are verified under fixed limits, and everything is loaded with `fetch_all`. | accepted residual risk | P3 | Add a limits flag and parent-existence checks. |
| Write ceiling at branch depth 10,000 | `prepare` refuses when `base_depth + 1 > max_depth`, and the default limit (`ReconstructionLimits::DEVELOPMENT`) is 10,000. A branch therefore stops accepting writes at depth 10,000. Raising the server's limit without `LEDGER_PROJECTOR_MAX_DEPTH` stalls projection. `deployment.md` does not mention this ceiling. Plan 0012 cut a prepare's cost at depth 3.5× on linear histories (a prepare at the ceiling extrapolates to ≈ 0.3 s) but did not change the ceiling. | code change (ADR first) or accepted residual risk with monitoring | P1 | Document the ceiling and alert on depth now; decide the target depth (Plan 0012 M4). |
| Server and projector limits differ | The server can accept states up to 256 MiB, but the projector's default `LEDGER_PROJECTOR_MAX_STATE_BYTES` is 48 MiB. A state in between blocks its stream with `STATE_TOO_LARGE`. | deployment-config evidence | P2 | Configure the limits consistently, and document the pairing. |
| Sculpin deduplication on `invocation_id` | Required by the ADR-0019 amendment (tech-debt). Without it, concurrent same-key validations may record a result from either environment. | external Sculpin dependency | P1 | Sculpin confirms and tests the deduplication. |
| Runtime identity's residual write authority | It can fabricate a consistent forward ref move or validation records within its tenants (ADR-0016, tech-debt). Closing this needs `SECURITY DEFINER` write paths or signed validator responses. | accepted residual risk | P2 | Decide in an ADR before production. |
| Per-release qualification on the release candidate | `backup-restore.sh` (ADR-0017 §6), `stress.sh` and the upgrade scripts must be re-run on the candidate. Upgrades are stop-the-world (`deployment.md`); there are no rolling upgrades. | deployment-config evidence | P1 | Run them and record the evidence per release. |
| Other recorded residuals | See tech-debt: migration 0009 aborts on a corrupt row without naming ids; lost-response fault injection is non-deterministic over HTTP; the intermittent CI hang (cause unknown, defensive fix); unknown-`kid` refusal during the 60 s JWKS throttle; the `cargo audit` exception and container findings; cross-tenant operator audit attribution. | accepted residual risk / decision | P3 | Track in tech-debt. |
| Reconstruction cost at depth | Parent-0 fold, linear in depth: ≈ 25 µs per ancestor after Plan 0012's windowed retrieval (was ≈ 100 µs on the same host), `2 × ceil(n / 256)` statements for n commits instead of `2n`, measured on linear histories (`docs/quality/performance-baselines.md`). `max_depth` is a hard ceiling with no checkpoints. | code change (ADR first) | P2 | Declare the target depth and budget; checkpoints only if the measured residual exceeds it (Plan 0012 M4: `CHECKPOINT-ADR-READY: NO`). |

## Top code-owned hardening items after Phase 6

1. Cancel abandoned PostgreSQL statements, enforce `statement_timeout` < request timeout,
   and add a per-transaction bound.
2. Put `accept` and the other write paths under a shared admission budget, and check
   slots against the pool size at startup.
3. Server metrics and OpenTelemetry.
4. Secrets and TLS: `_FILE` variants, one hardened secret reader, a private-CA bundle, and
   a production `sslmode` check.
5. An audited import-activation command and a database-enforced graph-status state machine.
