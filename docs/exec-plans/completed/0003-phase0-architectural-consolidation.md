# Plan 0003: Phase 0 — architectural consolidation

## Goal
Remove ambiguity between the specification and the implementation, and freeze the
persistent-identity and atomicity decisions that every later phase depends on,
**before** any further persistent behaviour (branches, merge, projection, storage
migration) is built. This plan produces decisions, contracts, and reconciled
documentation — not new runtime features. It realises Phase 0 / priority P0 of
`product_development_plan.md` (§36, §41).

## Scope
- Reconcile stale documentation with the delivered state (PostgreSQL ref CAS from
  Plan 0002; async storage traits from ADR-0007), without overclaiming horizontal
  correctness while immutable objects remain node-local.
- Author the P0 architecture decision records:
  - ADR-0008 effective-delta patch applicability semantics (§3.2).
  - ADR-0009 `sculpin-cognitive-commit/v2` provenance envelope (§3.1).
  - ADR-0010 graph / tenant / ref identity model (§6).
  - ADR-0011 authenticated principal and temporal-field validation boundary (§3.3, §3.4).
  - ADR-0012 production immutable-store abstraction and default (§4).
  - ADR-0013 atomic acceptance transaction: ref + ref_event + decision + outbox +
    idempotency (§5, §21).
  - ADR-0014 two-phase semantic-validation protocol and contracts (§7–§9, §20).
- Define the coordination contracts as a design document: Proposal, ValidationRecord,
  DecisionRecord, and `SemanticExecutionContext`, grounded in what Sculpin actually
  exposes today (see Discoveries).

## Non-goals
No branches, no merge/diff, no DAG crate, no checkpoints. No implementation of commit
v2, the immutable-store abstraction, authentication, idempotency, or the acceptance
transaction — those are Phase 1 (P1). No Sculpin validation-adapter code — that is
Phase 2 (P2). No changes to v1 canonical bytes or existing golden vectors. This plan
changes documents and decisions only; the sole code-adjacent change permitted is the
documentation-reconciliation edits.

## Relevant invariants
Product invariants 1–14, especially 2 (content-derived identity), 7 (refs never point
to missing content), 9 (no silent rewrite), 12 (canonicalization is persistent
protocol), and 13/14 (no Fluree runtime, no query/reasoning engine). Any P0 decision
that affects persistent identity (commit v2, patch semantics, graph identity) or
atomicity (acceptance transaction) must land as an ADR before its P1 implementation.

## Assumptions
- `product_development_plan.md` is the authoritative direction for sequencing and for
  the recommended decisions; ADRs codify those recommendations with local reasoning.
- Sculpin integration targets a Python service surface (pySHACL + Python reasoning),
  not a JVM/Jena runtime; the ledger stays validator-agnostic and calls an external
  service. KB revision identity, agent/service principal typing, the Virtual A-Box
  contract, and a standalone validation endpoint do not exist in Sculpin yet and must
  be defined by contract.

## Work breakdown
- [x] Reconcile stale docs (product-spec, tech-debt, storage-boundaries, ARCHITECTURE).
- [x] ADR-0008 effective-delta patch applicability.
- [x] ADR-0009 commit v2 provenance envelope.
- [x] ADR-0010 graph/tenant/ref identity.
- [x] ADR-0011 principal + temporal validation boundary.
- [x] ADR-0012 production immutable storage.
- [x] ADR-0013 atomic acceptance transaction.
- [x] ADR-0014 + design doc: validation protocol and contracts.
- [x] Update AGENTS.md/CLAUDE.md/README/docs index and decisions index; align skills,
      specialist agents, and hooks with the phased roadmap and §42 discipline.
- [x] Independent architecture + invariant reviews of the P0 decision set; golden
      protocol review of the v2 envelope design.

## Dependency/order constraints
Doc reconciliation is independent and can proceed first. ADR-0008 (patch semantics) and
ADR-0009 (commit v2) are identity-bearing and should be reviewed together with a golden
protocol review. ADR-0012 (immutable storage) and ADR-0013 (acceptance transaction)
jointly determine the P1 persistence design and must be internally consistent. ADR-0014
depends on ADR-0009/0010 (candidate identity and graph identity) and on the Sculpin
contract findings. No implementation begins until the identity/atomicity ADRs are
accepted and reviewed.

## Current status
Complete and signed off (2026-09-26). Docs reconciled, ADR-0008…ADR-0014 accepted,
contracts written, top-level guidance and specialist agents updated, and independent
reviews passed with all raised items folded back into the ADRs. The owner's conditional
sign-off review was applied as the amendments below; Phase 1 (Plan 0004) is active.

## P0 sign-off amendments (2026-09-26)
The owner gave conditional sign-off with six tightening items, all applied before the
architecture was frozen and before P1 writes persistent data:
1. ADR-0010: `LedgerGraph` ↔ KB relaxed from 1:1 to many-graphs-per-KB; the graph is the
   authorization/privacy boundary, branches are not.
2. ADR-0009: `evidence_refs[]` canonicalized as a sorted unique set (order never affects
   `CommitId`); `correlation_id` removed from the envelope (tracing metadata lives on
   proposal/ref-event/decision records); `source_system` defined as caller-declared,
   bounded, non-authoritative — `actor` is the only trusted provenance. ADR-0011 and the
   validation design doc updated to match.
3. ADR-0013: per-operation lineage predicates inside the transaction (`new_head.graph_id
   == graph_id`; genesis no parent; advance `parents[0] == expected_head`; merge
   `parents[1]` = source HEAD; reset only as a distinct audited operation), with tests for
   backward, sideways, and cross-graph CAS.
4. ADR-0013/ADR-0014: idempotency covers prepare, because `recorded_at` is
   identity-bearing and a lost prepare response would otherwise mint a second candidate.
5. Sculpin-reality wording: `ARCHITECTURE.md`, `AGENTS.md`, `CLAUDE.md`, the product
   spec, `core-beliefs.md`, `tech-debt.md`, and the roadmap header now say "Sculpin
   semantic validation/reasoning layer" with pySHACL/Python as the current
   implementation and Jena as a possible implementation detail only.
6. `product_development_plan.md` confirmed tracked in Git (commit 04b08ab); the
   doc-link gate is green against the committed tree. The root-level `plan.md`,
   `background.md` (a byte-identical duplicate of `plan.md`), and `ledger.md` were moved
   non-destructively to `docs/reference/original/` as non-normative background material.
The effective-delta decision, commit versioning strategy, authenticated-principal
boundary, PostgreSQL immutable-store default, two-phase protocol, proposal/validation/
decision separation, and validator-agnosticism were confirmed without change.

## Decisions made
Captured as ADR-0008 … ADR-0014 (see `../decisions/README.md`). This plan does not
itself decide protocol; it sequences and gates the ADRs.

## Discoveries
- Documentation audit found stale "PostgreSQL CAS is future work" statements in
  `docs/product-specs/cognitive-ledger.md`, `docs/exec-plans/tech-debt.md`,
  `docs/design/storage-boundaries.md`, and `ARCHITECTURE.md`; these are being corrected.
- Root files `plan.md`, `background.md`, and `ledger.md` are orphaned/duplicative source
  material not linked from `docs/README.md`. Recommend consolidating into
  `product_development_plan.md` or archiving; left in place pending owner confirmation
  (no destructive deletion without sign-off).
- Sculpin (at `~/project/semanticmatter/sculpin`) is a Python monorepo: SHACL via
  pySHACL, reasoning in Python workers (no live Jena), Fuseki as the query store with
  `urn:exodus:kb:<id>` named graphs, Microsoft Entra OIDC on the agent API, a header-
  based `RequestContext(tenant_id,user_id,roles,correlation_id)`, versioned SHACL shape
  sets with `shapes_hash`, and reasoning `source_graph_hash` (a base-graph digest). The
  SHACL request/response shape is a strong template for `ValidationRecord`. Missing and
  therefore defined-by-contract here: agent/service principal typing, an aggregate KB
  revision identity, the Virtual A-Box (dataset id / source version / query spec), and a
  synchronous validation endpoint the ledger can call.

## Risks
- Over-specifying contracts against a Sculpin surface that will change: mitigate by
  keeping the ledger validator-agnostic and versioning `SemanticExecutionContext`.
- Freezing commit v2 prematurely: mitigate by requiring golden vectors as the P1 gate
  and keeping v1 readable forever.
- Documentation drift recurring: mitigate with the doc-link gate and by updating docs in
  the same change as behaviour (§42.6).

## Quality gates
Documentation consistency (`scripts/check-doc-links.py`, architecture dependency check,
`scripts/quality-gate.sh fast`), golden protocol review of the v2 envelope design, and
independent architecture + invariant reviews of the P0 decision set. No integration or
differential gate is applicable to a decisions-only phase; that is recorded honestly
rather than skipped.

## Test evidence
- `scripts/check-fast.sh sanity` (fmt + doc-links + architecture dependency check) passed
  on 2026-09-26, exit 0; doc-link scan valid across 40 files.
- Documentation reconciliation applied and verified by diff (product-spec, tech-debt,
  storage-boundaries, ARCHITECTURE) preserving the "ref CAS done / immutable objects
  node-local" split.
- Independent architecture review: no confirmed defects; cross-ADR consistency and
  boundary purity confirmed. Independent invariant (golden-protocol-style) review: no
  confirmed defects. Both raised "risk requiring decision" items, all resolved by folding
  them into the ADRs:
  - v2 golden vectors MUST pin absent-vs-empty encoding for optional fields and the
    ordering/dedup identity of list fields (`parents[]`, `evidence_refs[]`), with
    `recorded_at` pinned in the logical-commit vector (ADR-0009).
  - effective-delta identity MUST be proven equal via checkpoint and via full
    reconstruction (ADR-0008).
  - target existence MUST be an in-transaction predicate, with a fault-injection test for
    accepting a never-prepared candidate (ADR-0013).
  - graph model arrives as a new monotonic migration; 0001 is never rewritten (ADR-0010).
  - multi-replica / shared-ref PostgreSQL deployment is not permitted until the shared
    immutable store ships (ADR-0012).
A decisions-only phase adds no runtime tests; Plan 0004 owns the executable evidence
(golden vectors, migration/upgrade, multi-process correctness) for these decisions.
- Sign-off amendment pass (2026-09-26): `scripts/check-doc-links.py` exit 0 (44 files
  scanned, including the new `docs/reference/original/`), `scripts/check-architecture.py`
  exit 0, `.claude/settings.json` parses. `cargo fmt --check` was not executed (the
  toolchain is unavailable in the sandboxed session) and is not applicable: the amendment
  commit changes Markdown only.

## Deferred work
All implementation of the P0 decisions moves to Phase 1+ plans. Skolemization,
checkpoints, performance baselines, and security hardening remain in `tech-debt.md` and
their respective later phases.

## Completion criteria
Met when: docs no longer describe delivered work as future; ADR-0008…ADR-0014 are
Accepted; the Proposal/Validation/Decision/`SemanticExecutionContext` contract design is
written and reviewed; AGENTS.md/CLAUDE.md/README/skills/agents reflect the phased
roadmap; and independent architecture + invariant reviews plus the golden protocol
review of the v2 envelope raise no unresolved P0/P1 findings.
