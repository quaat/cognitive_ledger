# Graph, tenant, and ref identity model

## Status
Accepted (design); schema and enforcement implemented in Phase 1.

## Context
The current runtime effectively serves one logical graph and one `main` ref. Before
branches, multi-tenant deployment, or Sculpin integration, the ledger needs an explicit
ownership model. Sculpin already scopes every domain entity by `tenant_id` and names
knowledge-base graphs deterministically as `urn:exodus:kb:<id>`, so ledger identity must
compose cleanly with that without importing Sculpin's schema.

## Decision
Introduce three first-class entities:

```
LedgerGraph   id, tenant_id, knowledge_base_id, purpose, created_at, status
Ref           graph_id, name, head, ref_type, protection_policy, version
Projection    graph_id, ref, target, projection_head
```

- A `LedgerGraph` belongs to exactly one tenant and may reference one Sculpin KB via
  `knowledge_base_id` (the KB's `urn:exodus:kb:<id>`). The relationship is **many
  ledger graphs → one KB**, not 1:1: several graphs (for example user-specific,
  team-specific, or differently governed cognitive states) may reference the same KB.
  The ledger owns its own stable `graph_id` so identity survives KB renames. The
  `LedgerGraph` is the authorization and privacy boundary; branches are for alternative
  evolution of the *same* cognitive state and are never the security boundary between
  distinct cognitive models. Amended at P0 sign-off (2026-09-26) from the original 1:1
  wording, before any schema or API crystallised.
- `tenant_id` scopes every entity and every authorization check (ADR-0011). It is not
  part of the commit envelope: a graph belongs to exactly one tenant, so `graph_id`
  determines it.
- Representation (frozen with commit v2 in Phase 1): `graph_id` is an opaque,
  ledger-generated ASCII token matching `[A-Za-z0-9._:-]{1,128}` (a UUID or URN-like
  token both fit). It is validated, never parsed; nothing derives meaning from its shape.
- **`graph_id` is globally unique**, not unique-per-tenant: `graphs` is keyed by
  `graph_id` alone. A commit embeds only `graph_id` (ADR-0009), so global uniqueness is
  what makes commit → graph → tenant resolution unambiguous and cross-tenant collisions
  impossible. **The `tenant_id` binding of a graph is immutable**: it is never updated;
  re-homing a graph to another tenant is a new graph plus an explicit, audited import,
  never a mutation of the existing row. (P0-bridging amendment, 2026-09-26.)
- `graph_id` is identity-bearing in the commit v2 envelope (ADR-0009): a commit belongs
  to exactly one graph.
- `Ref` carries a `protection_policy` (`main` defaults to protected) and a monotonic
  `version` for optimistic concurrency and ref-event ordering (ADR-0013).
- `Projection` records which commit a downstream Fuseki graph currently represents.

## Alternatives considered
- **Single global graph / single `main`.** Cannot support multiple KBs, tenants, or
  branches; blocks the whole roadmap.
- **Derive graph identity from the KB URN alone.** Couples ledger identity to an external
  naming scheme and breaks if a KB is renamed or re-homed.
- **Put branch name in the commit.** Rejected in ADR-0009; refs are mutable and separate.
- **One `LedgerGraph` per KB, with branches as the isolation boundary.** Authorization
  is graph-level, not triple-level, so distinct cognitive models that must not see each
  other need distinct graphs; forcing them into branches of one graph would make the
  branch namespace a de-facto security boundary it was never designed to be.

## Consequences
- Phase 1 adds `graphs`, `refs`, and `projections` tables and scopes the API and authz by
  `(tenant_id, graph_id)`. `graphs.knowledge_base_id` is nullable and non-unique (a
  plain index, not a uniqueness constraint), and the API never assumes it can resolve a
  KB to a single graph.
- Commit v2 (ADR-0009) binds `graph_id`; the acceptance transaction (ADR-0013) operates
  on `(graph_id, ref name)` and bumps `Ref.version`; validation context (ADR-0014)
  identifies the graph and target ref.
- The delivered `migrations/0001_create_refs.sql` defaults `graph_id='default'` (single
  graph). The real graph model MUST arrive as a new, monotonic migration; released
  migrations are immutable and are never rewritten (`.claude/rules/migrations.md`).
- The `graphs` migration (Phase 1) MUST ship with these executable tests against a real
  PostgreSQL (run by `scripts/test-integration.sh`):
  1. inserting two graphs with the same `graph_id` under *different* tenants fails with a
     uniqueness violation (global uniqueness);
  2. `UPDATE graphs SET tenant_id = …` fails (a trigger raises; the binding is immutable),
     while updating `status`/`purpose` succeeds;
  3. two graphs under one tenant referencing the same `knowledge_base_id` both succeed
     (many graphs per KB);
  4. the upgrade path from 0001: existing `refs` rows with `graph_id='default'` obtain a
     backfilled `graphs` row (`status='bootstrap'`) so the later `refs.graph_id` FK holds,
     and a clean install produces the same schema as the upgrade.

## Production v1 graph-binding policy
v1 envelopes carry no `graph_id`; their graph membership is a deployment policy, never
commit content. The policy, executable in `PostgresImmutableStore` (ADR-0012):

- `commit_index.graph_id` is NOT NULL and one commit id binds to exactly one graph.
  Because ids are content-derived and globally unique, the same v1 bytes can never be
  indexed under two graphs.
- A v2 commit is indexed under its own embedded `graph_id`, verified from the bytes.
- A v1 commit is indexed only under a binding the store was configured with
  (`V1Binding::BindTo(graph_id)`). Production deployments configure
  `V1Binding::Reject`: writing a v1 commit to a production store fails closed
  (`InvalidCommit`). The only supported `BindTo` target is the bootstrap `default` graph
  used by the pre-v2 write path in dev/single-host deployments and by an explicit,
  audited import of pre-production history.
- A v1 commit remains readable everywhere (dual read); the policy governs *writing and
  indexing*, not reading.
- When the v2 write path lands (P1.3/P1.4) the server's default becomes `Reject`; until
  then the bootstrap topology (`BindTo("default")`) is the only supported one, and it is
  not a multi-tenant deployment.
