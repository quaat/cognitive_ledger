# Canonical identity of `SemanticExecutionContext` v1, `SemanticEnvironment` v1, `ValidationRecord` v1 and the candidate state digest

## Status
Accepted (2026-09-27, Plan 0006 / Phase 2 slice P2.1). Persistent protocol: the layouts below
are frozen by golden vectors under `fixtures/golden/validation/` and `fixtures/golden/states/`
and by the independent Python reference encoders in `scripts/golden/`. Any change is a new
version with its own vectors; `CommitId`, `PatchId` and request-v1 bytes are untouched.

## Context
ADR-0014 makes `SemanticExecutionContext` and `ValidationRecord` versioned contracts that are
"content-addressable or carrying a deterministic digest", referenced by candidates and
decisions and never embedded in hashed commit bytes, but it does not settle their canonical
representation. Phase 2 persists both, binds acceptance to them (ADR-0019) and lets Sculpin
compute the same identities independently (a reviewer must be able to name "the context
Sculpin currently declares" without a ledger round-trip). The ledger also has to prove *which
exact RDF state* a validation examined: `CommitId` identifies a commit, not a dataset, and
two commits with different histories may reconstruct to the same state.

## Decision

### Encoding family
All three encodings reuse the deterministic binary layout of commit v2 (ADR-0009) and request
v1 (ADR-0015): a NUL-terminated header, big-endian integers, `field` = `u32` length + UTF-8
bytes, `opt` = `0x00` (absent) or `0x01` + non-empty `field` (present-but-empty is invalid),
counted lists with a fixed cap. Identifiers are opaque bounded tokens: non-empty, at most 512
bytes, no Unicode control characters, no normalization (`ledger-core::validate_token`).
Digests are strict `sha256:<64 lowercase hex>` (`ContentId`). Timestamps are canonical
`LedgerTimestamp`s (ADR-0011). Decoders are strict: they re-check every rule the encoder
applies, so bytes that are not the canonical form of some logical value are rejected.
Identity is `sha256:` over the bytes (`ContentId`).

### `sculpin-rdf-state/v1` — candidate state digest
```
"sculpin-rdf-state/v1\n"
one canonical N-Quads line per quad, LF-terminated, in bytewise ascending order
```
The quads are exactly the reconstructed dataset (`BTreeSet<Quad>`, ADR-0003 canonical lexical
form; named-graph quads carry their graph term, default-graph quads carry none). An empty
dataset is the header alone. The digest is derived metadata computed through the bounded
reconstruction interface (`ReconstructionLimits` apply); it never enters `CommitId`, and a
commit's state digest can be recomputed and re-verified from immutable history at any time.
Checkpoints are out of scope; when they exist they must key on this digest (spec
"Checkpoints and reconstruction").

### `sculpin-semantic-context/v1`
```
"sculpin-semantic-context-v1\0"
field  graph_id                         ADR-0010 token
field  candidate_commit                 CommitId
field  candidate_state_digest           sculpin-rdf-state/v1 digest
field  base_kb.kb_id                    token
field  base_kb.revision                 token (opaque: Sculpin composes it)
u8     ontology tag   0x00 absent | 0x01 then field ontology.id, field ontology.version
field  shapes.id                        token
field  shapes.version                   token
u8     reasoning tag  0x00 absent | 0x01 then field profile, field implementation, field version
opt    sources_revision                 token: Sculpin's external-source catalog revision in force
u32    virtual_context_count (<= 64)    then per element, in bytewise ascending order of the
                                        element's own encoding, unique (a set):
         field dataset_id
         field source_version
         u32   object_ref_count (<= 64) then `field` × n, ascending by the *field encoding*
                                        (u32 length prefix first, so shorter refs sort first),
                                        unique (a set)
         field query_spec_digest        ContentId
         field hydration_plan_digest    ContentId
field  validator.service_id             token (deployment-configured validator identity)
field  validator.service_version        token
field  validator.configuration_version  token
```
Rules: the ontology and the reasoning group are optional (SHACL-only validation without an
ontology or without reasoning is legitimate; absence is the one spelling of "none");
everything else is required and non-empty. `ontology.version` and `shapes.version` MUST
identify immutable content (e.g. Sculpin's shape-set version combined with its
`shapes_hash`): two different ontologies or shape sets never share `(id, version)`.
**Every set in every layout is ordered by the bytes of its elements' own encodings** — the
encoder, the strict decoder and the reference implementation apply the same rule.
Virtual contexts and object references are sets: caller order and duplicates never reach the
identity. Two otherwise identical runs against different `source_version`s therefore have
distinct context identities (the Virtual A-Box requirement of ADR-0014). Only identifying
provenance of external state is recorded — never A-Box triples.

### `sculpin-semantic-environment/v1` — the candidate-independent environment
```
"sculpin-semantic-environment-v1\0"
field  base_kb.kb_id · field base_kb.revision
u8     ontology tag (as in the context)
field  shapes.id · field shapes.version
u8     reasoning tag (as in the context)
opt    sources_revision
field  validator.service_version · field validator.configuration_version
```
The environment of a context is its projection: base KB, ontology, shapes, reasoning,
`sources_revision` and the validator's versions. It omits everything that depends on the
candidate — graph, commit, state digest, and the virtual contexts a run actually hydrated
(which datasets, object refs, query and hydration digests depend on the candidate's
content) — and the ledger-side `validator.service_id`, so Sculpin can compute and publish
the id of its *current* environment before any candidate exists. External-source drift is
carried by `sources_revision`: Sculpin revises it whenever the source versions it would
hydrate change. It is **deployment-declared**, not per run: present whenever Sculpin has a
source catalog, whether or not this run hydrated anything, and **required** whenever a
context carries virtual contexts (encoder, decoder and reference refuse hydration without a
revision, which would make source drift invisible to freshness). ADR-0019 binds acceptance
to the environment.

### `sculpin-validation-record/v1`
```
"sculpin-validation-record-v1\0"
field  graph_id
field  candidate_commit
field  candidate_state_digest
field  semantic_execution_context_id    ContentId of the context above
field  validator.service_id
field  validator.service_version
field  validator.configuration_version
u8     outcome                          0 conforms | 1 violations (the validator's verdict)
u32    violation_count                  results reported (any severity); >= 1 for violations;
                                        a conforming verdict may report non-blocking results
u32    summary_count (<= 64, <= violation_count)   then per entry, bytewise ascending on the
                                        entry's encoding, unique (a set):
         field severity                 token <= 64 bytes
         field code                     token
         field message                  <= 1024 bytes, may be empty (not optional)
field  recorded_at                      canonical LedgerTimestamp, server-assigned
field  report_digest                    ContentId of the validator's full report
opt    report_reference                 token <= 2048 bytes (immutable reference, opaque)
```
The full semantic report is never stored inline: the record carries a bounded summary plus
the report's digest and an optional immutable reference. `recorded_at` is assigned by the
ledger when it records the response, so a `ValidationRecord` id is re-verifiable but not
derivable from the validator's output alone (as with commit v2). The same candidate may have
any number of records (different times, contexts, validator versions); none embeds the
context, each references it by id, and the context's `(graph, candidate, state digest,
validator)` is repeated in the record so a database foreign key can enforce agreement
(migration 0010).

### What the ledger checks and what it does not
The ledger checks structure: bounds, strictness, that the context's candidate and state
digest are the ones it computed, that the record references the stored context, and that
the identities hash correctly. It never inspects ontology, shapes, base-KB or Virtual A-Box
identifiers beyond bounding them, and never evaluates `severity`/`code`/`message`.

## Revision (review round 1, 2026-09-27, before any persisted data)
Independent review of the first draft (commit 562f468) found that the encoder sorted object
references by raw string while the decoder and the reference sorted by field encoding (mixed-
length references failed to round-trip and produced another id in Sculpin), and that the
context id could not serve as the freshness key because it hashes candidate-specific
provenance. The layouts above supersede that draft: encoding-order sets everywhere, the
environment layout, an optional reasoning group, conforming verdicts with non-blocking
results, and the validator bound in the record→context foreign key. No data or release used
the draft; its vectors were replaced. A second review round found that per-run source pins
still made the environment candidate-dependent (a candidate hydrating fewer datasets got
another id); the pins were replaced by the Sculpin-declared `sources_revision` before any
persistence, again with regenerated vectors.

## Alternatives considered
- **JSON/JCS canonicalization.** Rejected for the same reasons as ADR-0009: the binary family
  already exists with two independent implementations and a fuzz corpus.
- **Store contexts/records as `immutable_objects`.** Would need the commit-family header
  classifier and the invariant verifier to learn new object kinds; rejected in favour of
  dedicated tables with their own content-address CHECKs (same guarantee, no cross-talk).
- **Validator-assigned `recorded_at`.** Would let a client alias records and make identity
  depend on an untrusted clock; rejected.
- **Only a count, no violation summary.** Rejected: rejected candidates must stay auditable
  in the ledger without dereferencing an external report.

## Consequences
- Phase 2 adds `ledger-validation-protocol` (types, encoders, strict decoders),
  `ledger-rdf::state_digest`, golden vectors and Python references; `scripts/check-fast.sh`
  runs the references. Sculpin can compute context ids itself from the documented layout.
- A validator's `base_kb.revision` is an integration prerequisite Sculpin must supply
  (ADR-0014, plan §10); the ledger treats it as opaque.
- Depends on ADR-0009/0011 (token and timestamp rules), ADR-0010 (graph identity),
  ADR-0014 (contract roles); feeds ADR-0019 (acceptance binding).
