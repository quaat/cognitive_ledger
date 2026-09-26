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
