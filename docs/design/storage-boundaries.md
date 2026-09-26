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

## Runtime configuration (`ledger-server`)
| Variable | Meaning |
|---|---|
| `LEDGER_ADDR` | Listen address. Default `127.0.0.1:8080` (loopback until the authenticated API exists, P1.4); containers and operators set `0.0.0.0:8080` explicitly — the Dockerfile does. |
| `LEDGER_DATA_DIR` | Filesystem root for the filesystem backend (default `./data`). |
| `LEDGER_DATABASE_URL` | When set: PostgreSQL holds the ref head **and, by default, the immutable objects**. Unset: filesystem-only development mode. Set but empty, or not valid Unicode: startup error (no silent filesystem fallback). Carries credentials; never logged. |
| `LEDGER_IMMUTABLE_BACKEND` | `postgres` (default when a database URL is set) or `filesystem`. `filesystem` is single-host only and logs a conspicuous warning: refs in the shared database would point at node-local content. `postgres` without a database URL is a configuration error (no silent fallback to node-local objects). Any other value refuses to start. |
| (shutdown) | Graceful shutdown on SIGINT and, on Unix, SIGTERM: stop accepting, drain open connections, exit after at most 30 s regardless (PostgreSQL rolls back any unfinished publication). |
| (startup) | With any backend the server verifies that the current HEAD resolves in the configured immutable store and refuses to start otherwise. |

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
