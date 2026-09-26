# Authenticated principal and temporal-field validation boundary

## Status
Accepted (design); enforced in Phase 1.

## Context
The bootstrap API accepts `author` as a free-form string in the request body and
`event_time` as an arbitrary string. Neither is acceptable for production: actor identity
must be derived from authenticated context (invariant/spec: authenticated actor
provenance must not be inferred from client text), and temporal fields must be parsed and
normalized because they are identity-bearing in commit v2 (ADR-0009). Sculpin already has
Microsoft Entra OIDC on its agent API and a header-based
`RequestContext(tenant_id, user_id, roles, correlation_id)`; the ledger must compose with
that trust model rather than invent a conflicting one.

## Decision
Authentication happens at the service boundary and yields an `AuthenticatedPrincipal`:

```
principal_id
principal_type   = human | agent | service
tenant_id
on_behalf_of?
```

- The commit v2 `actor` (ADR-0009) and `tenant_id`/`graph_id` scoping (ADR-0010) are
  populated **only** from the authenticated principal. The HTTP body cannot set or
  override actor, tenant, or `on_behalf_of`.
- Trust sources: OIDC (Sculpin's Entra) for humans and trusted workload/service identity
  for agents and services. `principal_type` is derived from the verified identity, never
  claimed by the caller. `correlation_id` is propagated for tracing.
- Temporal fields: `event_time` is parsed as an RFC 3339 date-time, normalized to a single
  serialization (UTC, fixed precision) before hashing, and may be optional per policy;
  malformed values are rejected. `recorded_at` is always server-assigned.
- Bounds: cap `message` length, `evidence_refs` count, and identifier lengths; reject
  malformed Unicode/IRIs early. The identity-bearing caps and the normalized `event_time`
  form are frozen together with the v2 envelope; the broader request/resource limits live
  in the resource-limits work (plan §22).

## Alternatives considered
- **Keep client-supplied `author`.** Trivially spoofable; unacceptable for audit.
- **Accept arbitrary `event_time`.** Unsortable/ambiguous and would make normalized,
  reproducible commit bytes impossible.

## Consequences
- `ledger-api` gains an authentication extractor producing `AuthenticatedPrincipal`;
  `CommitBody` drops `author`/tenant fields (`deny_unknown_fields` already blocks
  `recorded_time`); `event_time` becomes a typed, validated field.
- Because actor fields and the normalized `event_time` are part of v2 identity, they are
  covered by the ADR-0009 golden vectors.
- Integration: behind a trusted gateway the ledger may consume `RequestContext` headers;
  otherwise it validates Entra tokens directly. Either way `principal_type` comes from the
  verified identity.
