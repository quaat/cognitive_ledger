# Async storage traits and pluggable ref store

## Status
Accepted

## Context
Milestone 0002 introduces a PostgreSQL-backed compare-and-set ref store so mutable
`main` coordination is horizontally safe rather than single-host. The chosen client
(`sqlx`) is async-only. The existing `ObjectStore`, `CommitStore`, and `RefStore`
traits and the `Ledger` application service were synchronous and invoked with blocking
I/O directly from `async` Axum handlers. Adding a synchronous bridge around an async
database client would block a Tokio worker per acknowledgement and misrepresent the
long-term shape of the service.

Two further facts forced an interface decision now rather than later:

1. The filesystem `RefStore` previously validated ref *target existence* inside
   `compare_and_set` because `FileStore` was simultaneously the `CommitStore`. A
   PostgreSQL ref store holds only mutable refs and cannot see immutable filesystem
   commits, so that invariant cannot live inside the ref adapter.
2. The deployed server must select the ref backend at runtime (filesystem for local
   and tests, PostgreSQL in Compose/production) while immutable objects and commits
   stay on the filesystem in both cases.

## Decision
`ObjectStore`, `CommitStore`, and `RefStore` become `async` traits (via `async-trait`
so they remain object-safe for runtime backend selection). `FileStore` implements the
async methods by running the existing, unchanged synchronous filesystem logic inside
`tokio::task::spawn_blocking`, preserving its durability protocol (temp file + fsync,
atomic rename, containing-directory fsync, OS advisory lock). `Ledger` becomes async
and holds `Arc<FileStore>` for immutable object/commit access plus a separate
`Arc<dyn RefStore>` for the pluggable ref backend; `Ledger::open` selects the
filesystem ref store, and a new constructor accepts an injected ref store.

`RefStore::compare_and_set` is redefined as a *pure atomic ref primitive*: it swaps the
ref if and only if the stored value equals the caller's expected value, and knows
nothing about commit contents. The ref-target-existence invariant moves up to the
`Ledger` application service, which verifies the target commit exists in its commit
store before advancing any ref, regardless of ref backend. This keeps the invariant
enforced uniformly for both the filesystem and PostgreSQL adapters.

`async-trait` is a proc-macro utility, not an infrastructure client, so it is permitted
in `ledger-core`; the architecture gate continues to forbid `axum`/`sqlx`/database
clients there.

## Alternatives considered
- **Synchronous traits with an internal runtime bridge in the PostgreSQL adapter.**
  Lower immediate churn but blocks a Tokio worker per CAS and entrenches a shape we
  would later reverse.
- **Native `async fn` in traits with generic `Ledger<R>` monomorphization.** Avoids the
  `async-trait` allocation but reintroduces the Send-bound problem for futures used in
  Axum handlers and complicates runtime backend selection; the per-call boxing cost is
  negligible relative to disk/network I/O.
- **Leaving target-existence inside the ref adapter.** Impossible for a PostgreSQL ref
  store that cannot read filesystem commits, and duplicating the check per backend
  invites drift.

## Consequences
- The change is internal architecture, not persistent protocol: canonical bytes,
  hashes, and golden vectors are untouched and remain stable. No golden-vector review
  is required.
- `Ledger` and the storage traits are now `async`; call sites (`ledger-api`,
  `ledger-testkit`, `ledger-server`, and unit tests) use `.await`. `FileStore::open`
  stays a synchronous startup constructor.
- The ref-target-existence invariant is now an application-layer guarantee with its own
  test, applied identically to every ref backend.
- The seam for a PostgreSQL `RefStore` exists without changing immutable-object storage,
  keeping ADR-0004's storage split intact.
