# Production immutable-store abstraction and default

## Status
Accepted (design); implemented in Phase 1.

## Context
Plan 0002 made mutable `HEAD` coordination safe across processes with PostgreSQL CAS, but
immutable commits and patches still live on the ledger container's local filesystem. Two
replicas sharing one PostgreSQL therefore do **not** form a valid horizontally scaled
service:

```
Replica A  writes commit C42 to local filesystem; CAS main -> C42 in shared PostgreSQL
Replica B  reads main = C42 but does not possess C42 locally
```

This is the most important implementation-level architecture gap and must be resolved
before multi-replica production deployment. ADR-0004 anticipated PostgreSQL for refs; this
decision extends the storage split to immutable content.

## Decision
Refactor `Ledger` so it no longer concretely holds `Arc<FileStore>` for immutable
content. Depend instead on an infrastructure-free abstraction in `ledger-core`:

```
ImmutableStore
    put_content / get_content
    put_commit  / get_commit
    exists
```

Provide implementations:

```
FilesystemImmutableStore   (dev / single host)
PostgresImmutableStore     (production default)
S3ImmutableStore           (large checkpoints / large artifacts)
```

Production default: store the relatively small immutable commit/patch objects in
**PostgreSQL**, giving a single HA persistence system for refs plus content — simpler
backup/restore, simpler multi-replica correctness, transactionally indexable content, and
fewer distributed failure modes. Retain the object-store interface so S3-backed immutable
objects remain available for large artifacts and if scale measurements later justify it.
Digest verification and idempotent writes are preserved for every backend.

Delivery phasing: `FilesystemImmutableStore` exists today; `PostgresImmutableStore` lands
in Phase 1; `S3ImmutableStore` is later and measurement-driven. The list is a target
design, not current delivery.

Hard rule: a shared ref MUST NOT reference node-local immutable content. Deployment
qualification forbids it. If "S3 for all immutable objects" is chosen instead, the bucket
must be shared by every replica and its consistency/durability guarantees become part of
deployment qualification.

Operational constraint until Phase 1 lands the shared immutable store: because `PgRefStore`
(mutable ref CAS) is already usable while immutable commit/patch objects remain node-local,
a multi-replica / shared-ref PostgreSQL deployment is NOT permitted yet — the current
supported topology is single-writer / single-host. Multi-replica deployment is gated on the
`PostgresImmutableStore` (or a shared object store) shipping.

## P1.2 PostgreSQL foundation: `immutable_objects` + verified `commit_index`
(P0-bridging amendment, 2026-09-26.) The PostgreSQL backend stores content and a
*derived, verified* commit index so that P1.3 can enforce graph and parent predicates
transactionally rather than by decoding bytes inside the acceptance transaction:

```
immutable_objects   id TEXT PK ('sha256:'+64 hex, CHECK), bytes BYTEA, created_at
                    write-once; INSERT … ON CONFLICT DO NOTHING is the whole publish step

commit_index        id        TEXT PK  REFERENCES immutable_objects(id)
                    graph_id  TEXT NOT NULL          (v2: from bytes; v1: configured binding)
                    version   SMALLINT NOT NULL CHECK (version IN (1, 2))
                    patch_id  TEXT NOT NULL REFERENCES immutable_objects(id)
                    parent_count SMALLINT NOT NULL CHECK (0..2)
                    indexed_at
                    UNIQUE (graph_id, id)             (target of the P1.3 refs composite FK)

commit_parents      commit_id TEXT REFERENCES commit_index(id)
                    position  SMALLINT CHECK (0 or 1)
                    parent_id TEXT REFERENCES commit_index(id)
                    PK (commit_id, position), UNIQUE (commit_id, parent_id)
```

Rules:
- Only `PostgresImmutableStore::put_commit` writes `commit_index`/`commit_parents`, in the
  **same transaction** as the object bytes, from values it derived by decoding the bytes
  and checking `id == sha256(bytes)`. Parents are checked against `commit_index` (typed:
  a parent must be an indexed commit, not merely an object) and the patch against
  `immutable_objects`, inside that transaction. Rows are write-once; the application
  never `UPDATE`s or `DELETE`s them. The index is therefore *verified*: it can be
  re-derived from bytes at any time, and a re-derivation check is part of the gate.
- `get_commit` decodes bytes and verifies the digest; it never trusts the index for
  content. `exists` may probe either table.
- P1.3 builds on this: `refs(graph_id, head)` gains a composite FK to
  `commit_index(graph_id, id)`, which makes "a ref never points to missing content" and
  "`new_head.graph_id == graph_id`" database predicates; the advance rule
  `new_head.parents[0] == expected_head` is a join on `commit_parents` where
  `position = 0`; genesis is `parent_count = 0`.
- Filesystem and PostgreSQL backends are both version-neutral over `AnyCommit`
  (ADR-0009 dual read); the bootstrap `FileStore` keeps no index.

## Alternatives considered
- **Keep filesystem-only.** Not horizontally correct; the failure above is unavoidable.
- **S3/MinIO for all immutable objects now.** More distributed failure modes and a
  consistency-qualification burden; retained as an interface-level option, not the first
  default.

## Consequences
- `ledger-core` defines the `ImmutableStore` trait; `ledger-store` implements filesystem
  and PostgreSQL backends; `Ledger` composes `ImmutableStore` + `RefStore`/acceptance
  repository (ADR-0013).
- Phase 1 needs a migration path from filesystem content to the PostgreSQL content store,
  and a multi-process correctness test: two replicas on one shared store, one writes, the
  other reads and reconstructs.
- The acceptance transaction (ADR-0013) may assume the candidate commit is durable in the
  shared immutable store before a shared ref advances.

## Gate
With two replicas sharing one production store, a commit written by replica A is
reconstructable by replica B; no shared ref resolves to node-local content. The
`commit_index` re-derives byte-for-byte from `immutable_objects`; a v1 write against a
`Reject`-configured store fails closed; identical concurrent writes are idempotent.
