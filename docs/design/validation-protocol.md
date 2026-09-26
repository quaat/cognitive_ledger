# Two-phase semantic-validation protocol and coordination contracts

This document defines the contracts by which the Cognitive Ledger coordinates semantic
acceptance with Sculpin **without embedding any semantics**. The ledger owns immutable
candidates, refs, decisions, and ref events; Sculpin owns SHACL, reasoning, ontology, and
domain rules. It realises plan §7–§9 and §20 and is the design basis for ADR-0014. It is
a contract design for later phases (P2), not a P0 implementation.

## Why two phases
A synchronous `ledger → Sculpin → ledger` call chain would make the ledger structurally
depend on the validator at write time. Splitting *prepare* (create an immutable candidate)
from *accept* (move a protected ref after an immutable validation) keeps Sculpin in
control of semantic composition and lets the same candidate be revalidated later against a
different context. A convenience endpoint may orchestrate both phases synchronously later,
but the underlying protocol stays separable.

### Phase A — prepare candidate
```
authenticate (ADR-0011)
check Idempotency-Key; replay the stored candidate on a same-digest retry (ADR-0013)
resolve graph/ref (ADR-0010)
verify expected HEAD
parse + normalize RDF
validate patch against base and compute effective delta (ADR-0008)
persist effective patch
persist candidate commit (v2, ADR-0009)
return candidate identity
```
No accepted ref moves. A failed CAS later cannot lose the candidate.

### Phase B — validate and accept
Sculpin fetches the candidate state, builds a `SemanticExecutionContext`, runs its own
SHACL/OWL/domain/external checks (today pySHACL/Python reasoning; the implementation is
opaque to the ledger), and returns an
immutable `ValidationRecord`. Then:
```
accept(candidate, validation_id, expected_head)
```
runs the atomic acceptance transaction (ADR-0013), including the target-existence and
lineage predicates (`new_head.graph_id == graph_id`, `new_head.parents[0] == expected_head`
for a normal advance). Rejection records a decision and moves no ref.

## Contracts

### Proposal vs Branch
Not every feedback event needs a branch. A **Proposal** is a candidate immutable commit
proposed against an expected head, with evidence, validation records, and a decision. A
**Branch** is a mutable ref for a multi-step line of cognitive work. One-shot corrections
use a proposal without creating `feedback/<n>`; multi-step agent work uses a branch.
Rejected candidates remain immutable/auditable per retention policy though no accepted ref
reaches them.

```
ProposalRecord
    candidate_commit
    graph_id, target_ref, expected_head
    evidence_refs[]
    validation_ids[]
    decision_id?
    correlation_id?      (tracing metadata — here, never in the commit; ADR-0009)
```

### SemanticExecutionContext
The reproducibility contract. Content-addressable or carrying a deterministic digest.
```
candidate_commit
candidate_state_digest
base_kb:      kb_id, revision_or_digest
ontology:     id, version_or_digest
shapes:       id, version_or_digest
reasoning:    profile, implementation/version
virtual_contexts[]:
    dataset_id, source_version, object/version ids,
    query_spec_digest, hydration_plan_digest
validator:    service_version, configuration_version
```
Grounding in what Sculpin exposes today: `base_kb.revision_or_digest` ← Sculpin reasoning
`source_graph_hash`; `shapes` ← SHACL shape-set `id` + integer `version` + `shapes_hash`;
`validator` ← SHACL report `extra{backend, backend_version}`. Sculpin must provide a stable
KB revision / base-graph digest (it can compose one from `source_graph_hash`, `shapes_hash`
and an ontology version); the ledger only references it. External A-box triples are never
committed — only their identifying references.

### ValidationRecord
Immutable; referenced by proposals/decisions; **never** inserted into hashed commit bytes.
```
candidate_commit, candidate_state_digest
semantic_execution_context_id (or digest)
validator_identity, validator_version
outcome (conforms | violations[])
recorded_time
report_reference_or_digest
```

### DecisionRecord
Validation and decision are distinct: a candidate can be valid yet rejected by a reviewer.
```
candidate, branch/target_ref
decision = accepted | rejected | superseded
principal (ADR-0011)
reason / reason_ref
validation_ids[]
decided_at
```
Acceptance creates the ref movement (ADR-0013); rejection does not.

## Boundary
The ledger is validator-agnostic: it calls a validation service and stores its immutable
record, and never embeds SHACL/OWL/reasoning (invariants 13–14). The context and record
formats are versioned so Sculpin's surface can evolve.

## Required test scenarios (P2)
valid SHACL candidate; invalid SHACL candidate; reasoning-derived violation; virtual
A-box-dependent success; virtual A-box-dependent rejection; external source changed after
validation; ontology changed after validation; validator unavailable. Validation results
must never mutate already-hashed commit bytes.
