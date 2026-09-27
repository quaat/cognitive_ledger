# Canonical HTTP request identity (`sculpin-ledger-request/v1`)

## Status
Accepted (2026-09-26, P1.4). Persistent protocol: the layout is frozen by golden vectors in
`fixtures/golden/requests/` and the independent reference encoder
`scripts/golden/request_v1_reference.py`; any change is a new version with its own vectors.

## Context
ADR-0013 scopes idempotency by tenant, actor, graph, operation and `Idempotency-Key`, and
stores a `request_digest` so that a retry with the same key but a different request is
`IDEMPOTENCY_CONFLICT` rather than a silent replay. The digest is compared on every replay,
so it is persistent identity: if its definition drifted between releases, every legitimate
retry across the upgrade would conflict. P1.3 left the digest to the API layer without
defining it; a client-supplied digest would let a client alias two different requests.

## Decision
- The **server** computes the digest from the parsed, normalized request. Clients never
  supply it, and it excludes everything that is not the request's meaning: the
  `Idempotency-Key`, correlation id, `recorded_at`, the authenticated actor (already part
  of the idempotency scope), transport headers and JSON key order.
- Layout: header `sculpin-ledger-request/v1\0`, then typed fields in a fixed order, each a
  u32 big-endian length prefix plus UTF-8 bytes (`field`); optional fields are `0x00`
  (absent) or `0x01` + field (present, non-empty; an empty string is normalized to absent
  before encoding). `prepare`: operation, graph, branch, opt expected head, requested
  `PatchId` (the canonical patch, so client operation order and duplicates do not matter),
  activity, opt canonical `event_time`, evidence refs as a bytewise-sorted **set** (count
  then fields), opt source system, message. `accept`: operation, graph, branch, opt
  expected head, candidate, opt reason, validation policy. `reject`: operation, graph,
  branch, candidate, reason. Digest = `sha256:` over the bytes (`ContentId`).
- The `validation_policy` field names the policy the client requests. In Phase 1 the only
  requestable value is `no-validation`; whether a deployment permits acceptance under it is
  server configuration (`ValidationPolicy::Required` vs `NoValidation`), deliberately **not**
  request identity, so a retry after a configuration change replays instead of conflicting.
  Phase 2 introduces client-visible policy names through a new request version if needed.
- Six golden vectors (prepare genesis/advance/advance-reordered, accept, accept without
  reason, reject) are frozen; `request-prepare-advance-reordered` differs textually from
  `request-prepare-advance` (JSON order, evidence order and duplicates, operation order,
  timestamp offset) and must produce identical bytes. Rust (`crates/ledger-api/tests/
  request_goldens.rs`, via the handlers' own `canonical_*` builders) and Python both check
  every vector; `scripts/check-fast.sh` runs the Python check.

## Alternatives considered
- **Client-supplied digest header.** Trivially aliased; rejected.
- **Hash of the raw JSON body.** Key order, whitespace and evidence order would turn honest
  retries into conflicts; rejected.
- **Include the server's acceptance policy in the digest.** Would make retries across a
  configuration change conflict; rejected (see decision).

## Consequences
- `ledger-api::request_identity` owns the encoder; the repository trusts
  `RequestScope.request_digest` from this single producer (tech-debt: move the encoder into
  `ledger-store` if a second producer appears).
- Adding a request field or operation is a protocol change: new version string, new vectors,
  ADR amendment and golden review.

## Amendment: `sculpin-ledger-request/v2` for validation-aware operations (2026-09-27, Plan 0006)
Phase 2 adds request fields (`validation_id`, `semantic_context_id`, requested-context hints)
and the operation `validate`. Per this ADR that is a new version, not an extension. To keep
every P1.x retry replaying across the upgrade, **the version is selected by the request
shape**, deterministically:

- `prepare` — unchanged, always v1.
- `accept` / `reject` **without** `validation_id` — unchanged, v1 bytes (development
  `no-validation` acceptance and legacy rejections keep their digests).
- `accept` / `reject` **with** `validation_id`, and `validate` — v2.

```
"sculpin-ledger-request/v2\0"
field  operation             accept | reject | validate
field  graph_id
-- accept --
field  branch
opt    expected_head
field  candidate
opt    reason
field  validation_policy     "validated"
field  validation_id         ContentId (sculpin-validation-record/v1)
field  semantic_context_id   ContentId (sculpin-semantic-context/v1)
-- reject --
field  branch
field  candidate
field  reason
field  validation_id
-- validate (no branch: a validation is of a candidate, not of a ref) --
field  candidate
u8     base_kb tag    0x00 | 0x01 field kb_id, field revision
u8     ontology tag   0x00 | 0x01 field id, field version
u8     shapes tag     0x00 | 0x01 field id, field version
opt    reasoning_profile
u32    virtual_context_count, then VirtualContextRef elements exactly as in
       sculpin-semantic-context/v1 (bytewise ascending on the element encoding, unique)
```
Vectors: `fixtures/golden/requests/request-v2-*.{input,hex,sha256}`, checked by
`scripts/golden/request_v1_reference.py` (which now also implements v2) and
`crates/ledger-api/tests/request_goldens.rs`. The six v1 vectors are unchanged.
