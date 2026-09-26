# sculpin-cognitive-commit/v2 provenance envelope

## Status
Accepted (design); golden vectors and dual-read implemented in Phase 1.

## Context
The v1 commit envelope is deliberately minimal: ordered parents, patch id, a free-form
`author` string, `message`, client `event_time`, and server `recorded_time`. The
production architecture makes actor identity, activity, evidence, and correlation
first-class. Because no persistent production history exists yet, we must define the
long-term external envelope now rather than release v1 as the durable protocol.
Invariant 12 forbids silently changing canonicalization, and invariant 2 makes the
envelope identity-bearing, so a new version — not an in-place extension — is required.

## Decision
Define `sculpin-cognitive-commit/v2` with a frozen, versioned, deterministic canonical
encoding carrying:

```
format            (version tag)
graph_id          (ADR-0010; identity-bearing)
parents[]         (ordered; parent zero is the reconstruction parent)
patch_id          (effective patch, ADR-0008)
actor:            principal_id, principal_type (human|agent|service),
                  on_behalf_of?          (ADR-0011; from authenticated context only)
activity
event_time        (RFC3339, normalized — ADR-0011)
recorded_at       (server-assigned)
evidence_refs[]   (durable references only; a SET — canonicalized sorted + unique)
source_system     (caller-declared, bounded; informational, NOT trusted provenance)
message           (bounded length — ADR-0011)
```

Amended at P0 sign-off (2026-09-26): `correlation_id` was removed from the envelope and
`evidence_refs[]` changed from "caller order preserved + deduplicated" to a sorted, unique
set. Neither change affects released bytes — no v2 history exists yet.

Explicit exclusions from the commit envelope:

- **branch name** — refs are mutable and separate (ADR-0010); a name in an immutable
  commit would be a lie after a rename.
- **SHACL results, ontology/shapes versions, Virtual A-Box snapshots** — these belong to
  immutable `ValidationRecord`s (ADR-0014), because the same candidate commit may be
  revalidated later against a different semantic context.
- **raw JWTs/access tokens, arbitrary PII, full feedback/conversation text** — store
  durable evidence references instead (privacy/retention, plan §30).
- **`correlation_id`** — operational tracing metadata, not knowledge-history identity. It
  lives on the proposal, ref event, decision, and traces (ADR-0011, ADR-0013). Including
  it would make two otherwise identical commits differ by request plumbing.

### Trusted versus declared provenance
`actor` is the **only** trusted provenance in the envelope: it is populated exclusively
from the authenticated principal (ADR-0011). `source_system` is caller-declared: it is
bounded (ADR-0011 caps), stored and hashed as given, and is informational only. Consumers,
audit tooling, and the ledger itself MUST NOT treat `source_system` as authenticated
origin, and the API MUST NOT let it masquerade as such (no default derived from
credentials, no authorization decision keyed on it). If an authenticated origin is ever
needed it becomes a new principal attribute under ADR-0011, not a reinterpretation of this
field.

Retain the existing deterministic binary canonicalization approach; there is no need to
switch to JSON/JCS. v1 remains readable forever (dual-read); unknown envelope versions
fail closed.

## Alternatives considered
- **Extend v1 in place.** Violates invariant 12 and breaks the ability to read old
  fixtures deterministically.
- **JSON/JCS canonicalization.** Unnecessary churn; the binary form already meets the
  determinism requirement.
- **Embed validation/semantic context in the commit.** Bloats identity and prevents
  revalidation of an unchanged candidate against a new context.

## Consequences
- Phase 1 adds a v2 type in `ledger-core`, a dual-read decoder, compatibility tests, and
  checked-in golden vectors; the canonicalization design doc gains a v2 section.
- Depends on ADR-0010 (graph identity), ADR-0011 (actor/temporal validation), and feeds
  ADR-0014 (validation references the candidate, not vice versa).

## Encoding rules the golden vectors MUST pin (Phase 1)
Independent invariant review flagged these as identity-determinism hazards that cannot be
retrofitted once v2 history exists:

- **Optional fields** (`on_behalf_of`, and `event_time` where policy allows omission):
  the canonical encoding MUST distinguish *absent* from *empty*, and a caller MUST NOT be
  able to forge "absent" by sending an empty value. Pin both cases in golden vectors.
- **List fields**: `parents[]` is ordered and identity-bearing (parent zero is the
  reconstruction parent, ADR-0006). `evidence_refs[]` is a *set*: the canonical encoding
  sorts the references bytewise on their canonical UTF-8 form and removes duplicates, so
  the order a caller supplies never influences `CommitId`. Identical evidence therefore
  yields identical identity regardless of agent ordering. If evidence priority or ranking
  ever matters it MUST be modelled explicitly (a new field in a new envelope version),
  never inferred from position. Pin a vector where the same references are supplied in
  two orders and once with a duplicate, all yielding one `CommitId`.
- **`recorded_at`** is server-assigned and identity-bearing, so a `CommitId` is
  re-verifiable but not derivable from logical inputs alone; the golden "logical commit"
  includes a pinned `recorded_at`.

## Gate
Same v2 logical commit → same canonical bytes → same `CommitId` across builds and
platforms, proven by repository-owned golden vectors (including the absent-vs-empty,
`parents[]` ordering, and `evidence_refs[]` order-independence/dedup cases above), before
v2 is used for any persistent history.
