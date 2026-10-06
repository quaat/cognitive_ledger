# Production-qualification matrix

**Status: not production-qualified.** This matrix records what stands between the current
code and a production qualification. It was compiled read-only in Plan 0011 (Phase 6B) from
code and docs at `2a8a678`. Nothing here was run against a live Entra tenant, a Sculpin
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
| PITR / WAL archiving | Required by ADR-0017 §1. The contradiction in `deployment.md` was fixed in Plan 0011. No archive configuration exists in the repository. | deployment-config evidence | P1 | Supply `archive_mode`/`archive_command` and a restore-drill record. |
| Restore writer fencing | Procedure only (ADR-0017 §2). The ledger adds no epoch or fence. | accepted residual risk | P2 | Rehearse the runbook fencing step. |
| Projection reconciliation after restore | The projector reports `MARKER_AHEAD`, and the operator runs `ledger-projector rebuild`. | deployment-config evidence | P2 | Include rebuild-per-stream in the restore drill. |
| Live Sculpin validator | Only the fake validator is exercised (tech-debt). | external Sculpin dependency | P1 | Sculpin provides the endpoint of the design document. |
| Fuseki dedicated write identity | The projector supports a user/password file or a token file. Compose uses the Fuseki `admin` account. | deployment-config evidence | P1 | Provision an update-only account and show it is the sole writer. |
| Private CA for outbound HTTPS | https is enforced outside development, but no client accepts a custom CA bundle. This affects Fuseki, the validator and JWKS *(inf)*. | code change | P2 | Add a CA-bundle setting. |
| Audited raw-import activation | No `ledger-admin` activation command exists. An activated imported graph has no `ref_events`, so the verifier fails it. | code change | P2 | Add an `import activate` command that writes the ref events. |
| Admission control for `accept` | `accept` takes no expensive-operation permit, unlike `prepare`. Slots are not checked against the pool size. | code change | P2 | Add a shared budget and refuse `slots >= max_connections` at startup. |
| Cancel PostgreSQL work after an HTTP timeout | The edge drops the handler future only; there is no `pg_cancel_backend`. The default `statement_timeout` (30 s) equals the request timeout, and it is per statement, not per transaction. | code change | P2 | Cancel on drop, require `statement_timeout < request timeout`, and bound transactions. |
| Zero-valued limits | The server rejects 0 for every limit and DB timeout (`env_usize`). The projector's `number()` accepts 0 for its target timeout and reconstruction limits, which fails closed and never progresses *(inf)*. | code change | P3 | Require ≥ 1 in the projector. |
| Server metrics / OpenTelemetry | The server logs with `tracing` only. Only the projector has `/metrics`, and it is unauthenticated. | code change | P2 | Add request, error, slot and pool metrics; decide metrics authentication. |
| Rate limiting | None in code: only global semaphores (not per tenant) and a body limit. | deployment-config evidence | P2 | Configure gateway rate limits; decide per-tenant fairness. |
| Secrets from files, DB TLS | The server DB URL and HS256 secret come from the environment only, and `ledger-admin` takes `--database-url` on the command line. The validator token-file read lacks the projector's hardened checks. No `sslmode` is enforced. | code change | P2 | Add `_FILE` variants, one hardened reader, and a production `sslmode` check. |
| GitHub `main` ruleset | None (observed 2026-10-06). The required checks are job names: `benchmark-ci`, `container`, `dependency-review`, `docker`, `fast`, `fuzz`, `supply-chain` (tech-debt; kept in sync by `scripts/check-doc-consistency.py`). | repository-owner action | P1 | The owner creates the ruleset. |
| Production deployment configuration | None. `compose.yaml` is a development harness, and `deployment.md` says "not production-qualified". | deployment-config evidence | P1 | Write the production manifests and record their evidence. |
| Backup while projection is active | Not qualified (tech-debt). | code change (qualification script) | P2 | Add a projection-under-backup scenario to `scripts/backup-restore.sh`. |
| Offline verifier residuals | Orphaned detail rows after an FK drop and FK referential actions are unchecked. Merges are verified under fixed limits, and everything is loaded with `fetch_all`. | accepted residual risk | P3 | Add a limits flag and parent-existence checks. |
| Reconstruction cost at depth | Parent-0 fold, linear in depth (Plan 0011 characterization; `docs/quality/performance-baselines.md`). `max_depth` is a hard ceiling with no checkpoints. | code change (ADR first) | P2 | See the Plan 0011 recommendation and the checkpoint ADR inputs. |

## Top code-owned hardening items after Phase 6

1. Cancel abandoned PostgreSQL statements, enforce `statement_timeout` < request timeout,
   and add a per-transaction bound.
2. Put `accept` and the other write paths under a shared admission budget, and check
   slots against the pool size at startup.
3. Server metrics and OpenTelemetry.
4. Secrets and TLS: `_FILE` variants, one hardened secret reader, a private-CA bundle, and
   a production `sslmode` check.
5. An audited import-activation command and a database-enforced graph-status state machine.
