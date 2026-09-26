# Storage boundaries

`ImmutableStore::put_content` accepts bytes addressed by their digest. Repeating the same write succeeds; different bytes under an ID are corruption. Filesystem objects live below digest-derived paths and are published by atomic rename after sync.

`ImmutableStore::put_commit` is version-neutral (`AnyCommit`, ADR-0009) and validates a commit's ID, typed parent existence, and patch existence before storage; `get_commit` returns `None` for an object that is not a commit envelope. Per ADR-0007 `RefStore` is a pure atomic compare-and-set primitive; ref-target existence is enforced in `Ledger::advance_ref` as a *typed* check (the target must decode as a commit), not in the store. The filesystem adapter serializes writers with an OS file lock (released on process exit) and atomically replaces the `main` file; its guarantees assume one host and one filesystem. The PostgreSQL `PgRefStore` provides horizontally safe transactional CAS for the mutable ref.

## Shared immutable store (PostgreSQL, ADR-0012)
`PostgresImmutableStore` is the production immutable backend: `immutable_objects` holds the
content-addressed bytes and `commit_index`/`commit_parents` hold a derived, verified index
written in the same transaction as the commit. Publication is truthful — after every
`INSERT … ON CONFLICT DO NOTHING` the authoritative row is read back and compared, so
identical bytes are idempotent success and different bytes under the same id are
`ObjectCollision`; a commit whose authoritative index row carries another graph binding is
`GraphBindingConflict` (concurrent incompatible bindings resolve to exactly one winner at
READ COMMITTED, which each transaction pins). `put_content` refuses commit-envelope bytes.
Parents must be indexed commits **of the same graph** (`CrossGraphParent` otherwise), the
patch must be content rather than a commit, the graph must exist in `graphs` (ADR-0010,
`UnknownGraph`), and v1 history may only bind to a `bootstrap`/`importing` graph.
`verify_commit_index`/`verify_commits` re-derive every byte-derivable column and the
same-graph parent relation from bytes and fail on any tampering. Reads distinguish
"not a commit" (`None`) from a corrupt or unknown-version envelope (error). Two replicas sharing
one database agree and reconstruct each other's commits; this is the only supported
multi-replica topology, because a shared ref must never reference node-local content.

## Atomic workflow persistence (PostgreSQL, ADR-0013)
`WorkflowRepository` (reached through the `PostgresLedgerStore` composition root) makes
every accepted transition one transaction: idempotency result, active-graph check, ref row
lock, lineage predicates against the verified `commit_index`/`commit_parents`, ref advance
with `version + 1`, `ref_events` row, accepted `decisions` row, `projection_outbox` row.
`prepare` is a transaction too (effective delta, requested + effective patch, v2 candidate,
proposal, idempotency), so a retried prepare replays the exact original `CommitId`.
`reject` records a decision without moving the ref; `mark_superseded` is explicit. Fault
injection (`FailPoint`) proves that no failure before COMMIT leaves any mutable effect and
that a lost response after COMMIT replays. Raw `PgRefStore` CAS is the bootstrap/admin
primitive only (it bumps `version`, writes no event, and is refused on `active` graphs);
since P1.4 no public HTTP route reaches it or `Ledger::commit` — every public write is a
`WorkflowRepository` transaction producing a v2 commit. Migration 0007 scopes idempotency
by the complete actor (principal id, type, on-behalf-of), records an optional bounded
`correlation_id` on proposals, ref events and decisions (never part of any identity), and
ties every audit row's `(graph_id, tenant_id)` to `graphs`. `ReconstructionLimits`
(depth, quads, bytes) bound both the workflow's base reconstruction and public state
reads. Migration 0006's composite FK means a PostgreSQL ref can only
target an indexed commit of its graph.

## Runtime configuration (`ledger-server`)
| Variable | Meaning |
|---|---|
| `LEDGER_ADDR` | Listen address. Default `127.0.0.1:8080`. A non-loopback bind requires production authentication (`LEDGER_AUTH_MODE=oidc`) or the conspicuous development switch `LEDGER_ALLOW_INSECURE_NON_LOOPBACK=allow-insecure-non-loopback-development-only`; filesystem-only mode is always loopback-only. |
| `LEDGER_AUTH_MODE` | Required with a database URL: `oidc` (production; needs `LEDGER_AUTH_ISSUER`, `LEDGER_AUTH_AUDIENCE`, https `LEDGER_AUTH_JWKS_URL`) or `dev-hs256` (development/CI; needs issuer, audience and a ≥32-byte `LEDGER_AUTH_DEV_HS256_SECRET`). There is no unauthenticated or header-trusting mode. |
| `LEDGER_AUTH_*_CLAIM`, `LEDGER_AUTH_ROLE_MAP`, `LEDGER_AUTH_AGENT_CLIENT_IDS`, `LEDGER_AUTH_SERVICE_CLIENT_IDS` | Claims policy: tenant/principal/principal-type/roles/on-behalf-of claim names, `role=capability` map (`read|propose|review|admin`), client ids that identify agents or services. See `docs/quality/security.md`. |
| `LEDGER_UNVALIDATED_ACCEPTANCE` | Only `allow-unvalidated-acceptance-development-only` enables `accept` without semantic validation (Phase 2); any other value refuses to start; unset → `accept` returns `VALIDATION_REQUIRED`. |
| `LEDGER_LIMIT_BODY_BYTES`, `_PATCH_OPERATIONS`, `_TERM_BYTES`, `_METADATA_BYTES`, `_RECONSTRUCTION_DEPTH`, `_RECONSTRUCTION_QUADS`, `_RECONSTRUCTION_BYTES`, `_EXPORT_BYTES`, `_REQUEST_SECONDS`, `_CONCURRENT_EXPENSIVE` | Resource limits below the untrusted boundary (defaults in `ledger_api::ApiLimits`); exceeding one is `RESOURCE_LIMIT`. |
| `LEDGER_DATA_DIR` | Filesystem root for the filesystem backend (default `./data`). |
| `LEDGER_DATABASE_URL` | When set: PostgreSQL holds the ref head **and, by default, the immutable objects**. Unset: filesystem-only development mode. Set but empty, or not valid Unicode: startup error (no silent filesystem fallback). Carries credentials; never logged. |
| `LEDGER_IMMUTABLE_BACKEND` | `postgres` (default when a database URL is set). `filesystem` is accepted only without a database URL (development mode); combined with a database URL it is refused since migration 0006 (PostgreSQL refs must target indexed commits). `postgres` without a database URL is a configuration error. Any other value refuses to start. |
| (shutdown) | Graceful shutdown on SIGINT and, on Unix, SIGTERM: stop accepting, drain open connections, exit after at most 30 s regardless (PostgreSQL rolls back any unfinished publication). |
| (startup) | Filesystem mode verifies that HEAD resolves and serves a read-only inspection router (`/v1/refs/main`, `/v1/states/{id}`). Shared mode connects with `V1Binding::Reject`, so this process can never publish a v1 envelope. |

## Public API (`ledger-api`, P1.4)
Contract: `docs/api/openapi.json` (served at `/openapi.json`; a unit test fails if the
document and the router disagree). Routes are graph-scoped and authenticated:
`POST /v1/graphs/{graph}/proposals` (prepare; `propose`), `POST
/v1/graphs/{graph}/proposals/{candidate}/accept|reject` (`review`), `GET
/v1/graphs/{graph}/refs?name=<ref>` and `GET /v1/graphs/{graph}/commits/{commit}/state`
(`read`, bounded). Ref names are body/query fields because they may contain `/`.
Mutations require `Idempotency-Key`; the server computes the canonical request digest
(`sculpin-ledger-request/v1`, golden vectors in `fixtures/golden/requests/`, reference
encoder `scripts/golden/request_v1_reference.py`). Errors are `{code, message,
correlation_id}`; `X-Correlation-Id` is accepted (bounded, printable ASCII) or generated
and echoed on every response. `/health` is liveness; `/ready` checks the database.

## Graph provisioning (administrative)
`LEDGER_DATABASE_URL=… ledger-admin graph create --graph <id> --tenant <id> [--status
active|importing] [--kb <id>] [--purpose <text>]`. Graphs are created by operators, never
through HTTP (ADR-0010); `bootstrap` is reserved and `archived` is a lifecycle transition,
not a creation state.

## Filesystem → PostgreSQL content migration (administrative)
`LEDGER_DATABASE_URL=… ledger-admin migrate-fs-to-pg --source <dir> [--graph default] [--branch main] [--json]`
runs `FsToPgMigration` (`crates/ledger-store/src/migrate_fs_to_pg.rs`). Prefer the
environment variable for the URL (`--database-url` is accepted but visible in process
listings; argument values are never echoed). The source is opened read-only and must
already exist. Steps: enumerate and digest-verify every source object (commit-family
objects that do not decode are errors, not content); require every v2 commit to belong to
the target graph and v1 commits to be importable under the destination's binding, which
must name the target graph; order commits topologically (missing parents and cycles abort
before anything is published); resolve HEAD from the filesystem ref or the existing shared
`refs` row — both must agree and the HEAD must be among the imported commits, so a wrong or
partial source cannot succeed; publish patches, then commits; verify destination bytes, the
scoped commit index, and that every commit is indexed under the target graph; reconstruct
HEAD through both backends and compare; only then install an absent destination ref
(`RefInstalled`), report `AlreadyMigrated` if it already equals HEAD, or abort if it
differs. It never assigns new ids, rewrites envelopes, deletes source content, or moves a
ref onto unverified or foreign-graph history. Orphaned content after a failure is
acceptable; an invalid ref is not. The target graph must be `bootstrap` or `importing`
for v1 history (ADR-0010). It is an offline cutover tool: source writers MUST be quiesced;
the tool detects movement, it cannot prevent it. Immediately before cutover it re-reads the
heads and fails closed on any movement — a moved source HEAD is `MIGRATION_SOURCE_MOVED`,
a moved existing destination HEAD (or one that appeared during a no-HEAD run) is
`HEAD_CHANGED`, an absent destination is installed by CAS and only a concurrently
installed identical HEAD counts as idempotent success. A final agreement check runs after
the ref step; if it fails the ref *was* installed and then moved by a writer, and the error
names the mover's HEAD. Every ref value the tool writes or accepts is the verified HEAD; a
reported success means the destination HEAD equalled it at the final check. Migration
impact: history written before the patch-validity rule whose "patch" is not a canonical
patch fails closed (`INVALID_PATCH` from the destination store, `CorruptObject` from
verification); such sources need repair before cutover, and the server refuses to start on
such a HEAD (`verify_head` checks the head's patch too). On startup the server runs `Ledger::verify_head` and refuses to
serve a HEAD that does not resolve in its configured store.

Schema-level guards (migration 0005): `immutable_objects`, `commit_index`, `commit_parents`
are write-once (UPDATE/DELETE raise); `refs` identity columns are immutable and only `head`
moves. The `graphs` trigger (0004) makes `graph_id`/`tenant_id` immutable on every UPDATE
path including `ON CONFLICT DO UPDATE`.

Objects are written before the ref. Files and their containing directories are synchronized after atomic rename before acknowledgement; crash leftovers may be unreachable but cannot make the ref invalid. The API never acknowledges an advanced ref before durable publication. Platform/filesystem durability assumptions still require deployment qualification and fault testing.
