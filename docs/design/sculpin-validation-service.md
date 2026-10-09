# Sculpin semantic validation service — ledger-facing contract

Status: **contract defined by the ledger (Plan 0006 P2.4); not yet implemented by Sculpin.**
Sculpin today validates in-process (pySHACL + Python reasoning) through KB-scoped tools and has
no standalone endpoint (ADR-0014). This document is the interface the ledger calls; the Rust
types are `ledger_validation_protocol::{ValidationRequest, ValidatorResponse}` and the client
is `ledger_api::validator::HttpValidationClient`. Nothing here asks the ledger to understand
semantics: Sculpin owns the effective semantic state, SHACL, reasoning, ontology, base KB and
Virtual A-Box hydration; the ledger owns the candidate, the records and the acceptance policy.

## Operation
`POST <LEDGER_VALIDATOR_URL>` (one configured endpoint), `Content-Type: application/json`,
`Accept: application/json`, `Authorization: Bearer <credential>` when the ledger is
configured with `LEDGER_VALIDATOR_TOKEN_FILE` (workload identity token or API key; the
ledger never logs it), `Idempotency-Key: <invocation_id>` (the logical invocation identity,
see "Idempotency" below), `X-Correlation-Id` for tracing only. Synchronous; one candidate per
call.

### Request (`sculpin-validation-request/v1`)
```json
{
  "protocol": "sculpin-validation-request/v1",
  "invocation_id": "sha256:…sculpin-validation-invocation/v1 id; equals the Idempotency-Key header…",
  "candidate": {
    "graph_id": "0b0a2a1c-2e3d-4f50-8a61-72b384c5d6e7",
    "knowledge_base_id": "urn:exodus:kb:material-science",
    "commit": "sha256:…candidate commit id…",
    "state_digest": "sha256:…sculpin-rdf-state/v1 digest of the quads below…",
    "state_href": "/v1/graphs/0b0a…/commits/sha256:…/state",
    "quads": ["<urn:material:a> <urn:temperature> \"80\" .", "…"]
  },
  "requested": {
    "base_kb": {"kb_id": "urn:exodus:kb:material-science", "revision": "kbrev-7"},
    "ontology": {"id": "urn:sculpin:ontology:core", "version": "O2"},
    "shapes": {"id": "urn:sculpin:shapes:material", "version": "12+shapes_hash:4b7e"},
    "reasoning_profile": "owl-rl",
    "sources_revision": "urn:sculpin:source-catalog:rev-41"
  },
  "correlation_id": "…"
}
```
- `candidate.quads` is the complete reconstructed candidate dataset (canonical N-Quads,
  bytewise sorted; bounded by `LEDGER_LIMIT_VALIDATION_STATE_BYTES`). It is the cognitive
  overlay to evaluate on top of the base KB. `state_digest` lets Sculpin verify it
  (`sha256("sculpin-rdf-state/v1\n" + lines…)`, ADR-0018). `state_href` is the ledger's
  graph-scoped read path, relative to the ledger base URL, for a validator that prefers to
  fetch (it needs the `read` capability for the graph's tenant).
- `requested` are hints; every field is optional. An absent hint means "your current one".
  Sculpin may refuse a hint it cannot honour (4xx); a response that silently ignores a hint
  (other base KB, ontology, shapes, reasoning profile or sources revision) is refused as
  `VALIDATOR_ERROR`. There is no per-dataset version pin: external-source versions are
  selected only through `sources_revision`, so a run can never report the current revision
  while using older source versions. Hints are part of the ledger's request
  identity, so they are also what an idempotent retry reproduces.

### Response (`sculpin-validation-response/v1`)
```json
{
  "protocol": "sculpin-validation-response/v1",
  "candidate_commit": "sha256:…same as request…",
  "candidate_state_digest": "sha256:…same as request…",
  "context": {
    "base_kb": {"kb_id": "urn:exodus:kb:material-science", "revision": "kbrev-7"},
    "ontology": {"id": "urn:sculpin:ontology:core", "version": "O2"},
    "shapes": {"id": "urn:sculpin:shapes:material", "version": "12+shapes_hash:4b7e"},
    "reasoning": {"profile": "owl-rl", "implementation": "sculpin-python-reasoner", "version": "0.9.2"},
    "sources_revision": "urn:sculpin:source-catalog:rev-41",
    "virtual_contexts": [{
      "dataset_id": "urn:sculpin:datasource:lab", "source_version": "v41",
      "object_refs": ["s3://lab/run-9.parquet@v3"],
      "query_spec_digest": "sha256:…", "hydration_plan_digest": "sha256:…"
    }],
    "validator": {"service_version": "2026.09.1", "configuration_version": "cfg-2026-09-27"}
  },
  "outcome": {
    "kind": "violations",
    "violation_count": 3,
    "violations": [{"severity": "Violation", "code": "sh:MinCountConstraintComponent", "message": "…"}]
  },
  "report": {"digest": "sha256:…full report…", "reference": "urn:sculpin:validation-report:0f3a"}
}
```
- `context` is the **effective** context that was used (not the hints). `ontology`,
  `reasoning` are omitted when none applied. `sources_revision` is Sculpin's revision of
  the external-source catalog in force, declared per deployment (present whenever Sculpin
  has a catalog, even if this run hydrated nothing; required whenever `virtual_contexts` is
  non-empty) — it must change whenever the versions Sculpin would hydrate change; it is the candidate-independent freshness key for
  external data, while `virtual_contexts` record what this run actually hydrated. `virtual_contexts` identify external state
  only — never A-Box triples. The ledger adds `validator.service_id` from its own
  configuration (`LEDGER_VALIDATOR_SERVICE_ID`, the deployment's validator trust anchor);
  a response cannot claim a service identity.
- `outcome.kind` is Sculpin's verdict (`conforms` | `violations`). `violation_count` is the
  number of results reported (any severity; a conforming verdict may report warnings);
  `violations` is a bounded summary (severity ≤ 64 bytes, code ≤ 512 bytes). The ledger
  normalizes it deterministically before recording (control characters in messages become
  spaces, messages are cut to 1024 bytes, duplicates collapse, the first 64 entries in
  canonical order are kept) and never evaluates severities or codes. pySHACL mapping:
  `sh:conforms` → `kind` (with `allow_warnings`, a conforming verdict may carry Warning/Info
  results), `sh:resultSeverity` → severity, `sh:sourceConstraintComponent` → code,
  `sh:resultMessage` → message; `report.digest` is over a serialization Sculpin fixes (e.g.
  canonical N-Triples of the results graph).
- `report.digest` is the SHA-256 of the complete report Sculpin keeps; `reference` is an
  optional immutable reference to it (≤ 2048 bytes). The report is never sent inline.
- Unknown fields anywhere are refused (strict shape). The response must name the same
  `candidate_commit` and `candidate_state_digest` as the request.

## Identity the ledger derives (ADR-0018)
The ledger turns the response into a `sculpin-semantic-context/v1` (context id), a
`sculpin-semantic-environment/v1` (environment id) and a `sculpin-validation-record/v1`
(validation id, with a server-assigned `recorded_at`). The **environment** is the
candidate-independent part: base KB, ontology, shapes, reasoning, `sources_revision` and
the validator's two versions. Sculpin (or an orchestrator) can
compute the environment id of "what is current now" from the frozen layout without any
candidate, and acceptance names it (ADR-0019): a validation from another environment is
`VALIDATION_STALE`. The reference encoder is `scripts/golden/validation_v1_reference.py`
(`encode_environment`); vectors in `fixtures/golden/validation/environment-v1-*`.

## Integration prerequisites Sculpin must supply
1. **An aggregate, stable `base_kb.revision`** for the KB state validated against. Sculpin
   composes it (for example from `source_graph_hash`, `shapes_hash` and the ontology
   version); the ledger treats it as opaque and never guesses it. Without it two
   validations against different base states would be indistinguishable. The ordinary
   Sculpin KB is **not** migrated into the ledger to obtain it.
2. **Content-identifying `ontology.version` and `shapes.version`**: two different ontologies
   or shape sets must never share `(id, version)`; append a content digest (e.g.
   `12+shapes_hash:…`) where the version counter alone is not content-derived.
3. **An external-source catalog revision** (`sources_revision`) that changes whenever the
   source versions in force change, plus **Virtual A-Box identification**: stable
   `dataset_id`, `source_version`, object/version
   references (≤ 64; fold larger lists into `hydration_plan_digest`), and digests of the
   query specification and hydration plan.
4. **An endpoint implementing this contract**, authenticated by workload identity.
5. **Publishing the current environment** (its id, or its fields) to whoever accepts, so
   acceptance can require it.

## Timeouts, retries, idempotency
- The ledger's call is bounded by `LEDGER_LIMIT_VALIDATOR_SECONDS` (default 15 s; the
  start-up validation requires `validator + statement_timeout ≤ request_timeout`, and the call
  is additionally capped by the request's remaining budget — ADR-0026) and `LEDGER_LIMIT_VALIDATOR_RESPONSE_BYTES` (default 1 MiB, streamed and
  cut at the cap). Concurrent calls per replica are capped by
  `LEDGER_LIMIT_CONCURRENT_VALIDATIONS` (default 4; excess → `503 RESOURCE_LIMIT`); the
  reconstruction before the call takes one of the expensive-operation slots.
- Classification: connection failure, timeout, 5xx or 429 → `VALIDATOR_UNAVAILABLE` (503,
  retryable, nothing recorded); 3xx (never followed), other 4xx, a non-JSON content type,
  an oversized or malformed body, or a response naming another candidate/state →
  `VALIDATOR_ERROR` (502, nothing recorded).
- Once the ledger has recorded a validation, a client retry of `POST …/validations` with the
  same `Idempotency-Key` replays the record without calling the validator.

## Idempotency: at-least-once delivery, exactly-once logical validation
The ledger never holds a database transaction, connection or lock across the validator
call (ADR-0019), so the same logical ledger request can reach Sculpin more than once:
two identical requests racing under one `Idempotency-Key` both miss the replay lookup, and a
client retry after a lost response or a ledger crash between Sculpin's answer and the
ledger's record calls again. Every such delivery carries the same **invocation identity**:

- `invocation_id` in the body and the `Idempotency-Key` header carry one value, the
  `sculpin-validation-invocation/v1` id: `sha256:` + hex SHA-256 over the domain-separated
  encoding of the caller's authenticated idempotency scope (tenant, principal type and id,
  delegation, graph, operation `validate`, the ledger `Idempotency-Key`) and the canonical
  `sculpin-ledger-request/v2` digest of the validate request (candidate and hints).
  Correlation ids, clocks, arrival order and ledger instance never enter it. Layout in
  [`validation-protocol.md`](validation-protocol.md); reference encoder
  `scripts/golden/validation_v1_reference.py` (`encode_invocation`), vectors
  `fixtures/golden/validation/invocation-v1-*`.
- Another ledger key, principal, graph, candidate or hint set is another invocation. The
  same key with another body is refused by the ledger (`IDEMPOTENCY_CONFLICT`); at most one of
  two such racing requests is ever recorded.
- **Contract (required):** repeated or concurrent validation calls carrying the same
  invocation identity represent the same logical validation operation and must resolve to
  the same logical effective semantic context and validation result. Concretely:
  - *Results.* Sculpin keeps the answer it returned for an invocation (HTTP 2xx) and answers
    every later delivery of that id with it. The store is shared by all Sculpin replicas.
  - *Single flight.* A delivery arriving while the first is still running waits for it
    rather than validating again in whatever environment is then current. If the first
    worker dies, another may take the invocation over (a lease); a waiter that outlives
    the ledger's timeout is abandoned by the ledger, and the ledger's next retry must find
    the finished result.
  - *Binding.* Sculpin cannot recompute the id (it never sees tenant, principal or key), so
    it stores `(candidate.commit, candidate.state_digest, requested)` with the id and refuses
    (4xx) a delivery whose body differs. Stored results are scoped by the authenticated
    caller as well as by id; the id is not a secret.
  - *Failure* is judged from Sculpin's side: only an invocation Sculpin itself answered with
    a non-2xx status may be computed afresh on retry. A 2xx answer the ledger refused as
    `VALIDATOR_ERROR` (ignored hint, other candidate/state, malformed or oversized body)
    stays bound to its invocation, so a retry under the same key fails the same way; the
    client must use a new `Idempotency-Key`.
  - *Retention.* The ledger's retry horizon is unbounded (idempotency results never expire,
    and a request that failed recorded nothing). Sculpin declares a retention period `R`
    for invocation results and keeps them at least that long; exactly-once logical
    validation holds within `R`. After `R` a redelivery may be validated afresh in the then
    current environment: the ledger records that honestly and acceptance still requires the
    environment the reviewer names, so expiry (or no deduplication at all) changes which
    environment a record names, never whether acceptance is safe. Orchestrators retry a
    failed validation within `R` or under a new key.
- A same-key retry repeats the original logical validation, even where an absent hint
  means "current". To validate against the environment that is current now, use a new
  `Idempotency-Key` (ADR-0019).
- Because the ledger holds no lock across the call, two requests racing under one key with
  *different* bodies both reach Sculpin (under different invocation ids); the ledger records
  at most one of them and answers the other `IDEMPOTENCY_CONFLICT`, so Sculpin may keep an
  invocation result the ledger never recorded.
- The id is opaque to Sculpin: it must not be parsed, and it carries no semantic meaning. It
  is not part of any context, environment or validation identity, and it is not stored by
  the ledger.

Without this, the environment a durable record names would depend on timing: whichever of
two deliveries recorded first would win, possibly under a newer base KB, ontology, shapes,
reasoning configuration or source-catalog revision. The ledger's fake validator in
`crates/ledger-api/tests/pg_validation_api.rs` implements the contract for the ledger's own
tests; that proves the ledger's side (same identity on every delivery, one record, one
idempotency result), not Sculpin's.

## Transport security
https is required whenever the ledger runs with production authentication; plain http is
accepted only to a loopback host in development. No redirects are followed, no proxy is
taken from the environment, credentials never appear in URLs, errors never echo the
endpoint, the credential or the response body.
