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

- A `LedgerGraph` maps 1:1 to one Sculpin KB cognitive overlay, referencing the KB via
  `knowledge_base_id` (the KB's `urn:exodus:kb:<id>`); the ledger owns its own stable
  `graph_id` so identity survives KB renames.
- `tenant_id` scopes every entity and every authorization check (ADR-0011).
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

## Consequences
- Phase 1 adds `graphs`, `refs`, and `projections` tables and scopes the API and authz by
  `(tenant_id, graph_id)`.
- Commit v2 (ADR-0009) binds `graph_id`; the acceptance transaction (ADR-0013) operates
  on `(graph_id, ref name)` and bumps `Ref.version`; validation context (ADR-0014)
  identifies the graph and target ref.
- The delivered `migrations/0001_create_refs.sql` defaults `graph_id='default'` (single
  graph). The real graph model MUST arrive as a new, monotonic migration; released
  migrations are immutable and are never rewritten (`.claude/rules/migrations.md`).
