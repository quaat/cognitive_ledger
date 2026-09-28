# Plan 0007: Phase 3 — accepted-state projection

Status: **in progress** (started 2026-09-27). Branch `claude/p3-accepted-state-projection` from
`main` at `0a56092484ba890df3cf43f297e690db1132cc4b` (the PR #7 merge; Phase 2 complete, see
[Plan 0006](../completed/0006-phase2-semantic-validation.md)). No gate is reported as passed
until it is executable and has run.

## Goal
Reliably project the ledger's **accepted** cognitive state into Sculpin's query environment
(Fuseki) while the ledger stays authoritative, and make projection lag, failure and recovery
explicit. Not general event streaming: a narrow projection subsystem consuming the existing
`projection_outbox`.

## Scope (slice 1, then slice 2)
1. ADR-0020 (projection protocol: cognitive graph IRI, marker, conditional full-state write,
   decision table, error classes) and ADR-0021 (projection state, stream leases, projector
   identity).
2. `crates/ledger-projection` (infrastructure-free): target graph identity, marker model and
   parsing, the ADR-0020 decision function, `ProjectionClient` trait, error taxonomy, frozen
   vectors.
3. Migration 0011: `projection_state`, guard triggers, `ref_events` FK target,
   `ledger_grant_projector`, runtime grant re-issued with SELECT on `projection_state`; schema
   verifier (tables, constraints, CHECKs, NOT NULL, triggers, functions) and a projector
   identity model; PostgreSQL 15 + 17.
4. `ProjectionRepository` (`ledger-store`): enable/disable (owner), claim with lease,
   acknowledge / fail under lease fencing, status and lag.
5. `crates/ledger-projection-fuseki`: hardened HTTP adapter (configured endpoints only,
   https in production, no redirects, no proxy, timeouts, bounded bodies, credentials from a
   file and never logged, error classification), SPARQL builders for the conditional write,
   rebuild, marker read and the transactional probe.
6. `apps/ledger-projector`: startup verification (schema, projector identity, target
   transactional probe), bounded worker concurrency, claim loop, backoff, graceful shutdown,
   `/health`, `/ready`, `/metrics`; `rebuild` and `verify` subcommands; periodic
   reconciliation of idle streams and a periodic transactional probe.
7. `ledger-admin projection enable | disable | status [--json]` (owner; status is the one
   status surface).
8. Tests: unit (IRI vectors, marker parsing, decision table, request shapes), PostgreSQL
   (claim race, lease expiry, fencing, ordering, least privilege, verifier drift), real
   Fuseki (genesis, advance, duplicate, outage and catch-up, crash before/after target commit
   with failpoints, lost/corrupt/foreign/ahead markers → rebuild or recovery, feed switch
   across tenants with a held write, two workers, multiple graphs of two tenants).
9. Slice 2: upgrade 0010 → 0011 harness from the Phase-2 release with a populated backlog;
   compose integration with Fuseki; reviews.

## Non-goals
General branches, branch APIs, merge / merge preview / conflicts, checkpoints, S3, GC,
incremental RDF-patch projection, projecting anything other than accepted state, a public
projection HTTP API (status is CLI + metrics first), live Sculpin integration.

## Invariants
All Phase 0–2 invariants unchanged; existing canonical goldens unchanged. Added: (a) projection
never changes ledger history and acceptance never waits for projection; (b) the target marker
never moves backwards and a version is marked delivered only when the target represents it or
a later version; (c) a stream projects only into its own cognitive graph, and no two streams
share one; (d) nothing but accepted state at the stream's ref is written; (e) the ledger never
reads *state* back from Fuseki into history — the projector reads only the marker, the
target's own triple count and containment answers, to decide what to write (ADR-0020: the
marker is trusted as far as the target is).

## Persistent data changes
Migration 0011 only (ADR-0021). No content migration; existing outbox rows stay pending until
a stream is enabled.

## Security boundary
Projection targets are deployment configuration (endpoint, dataset, credential file), never
request data. Distinct `ledger_projector` database role (ADR-0021). The HTTP server gains no
projection write privilege.

## Failure semantics
Target unavailable → retry with backoff; the ledger keeps accepting, the backlog grows and
is observable. Permanent target errors → stream `blocked` with a stable code. Marker ahead of
the ledger → `rebuild_required`. Crash anywhere → lease expiry and idempotent re-run.

## Migration impact
Stop projectors (none exist before) → owner `ledger-admin migrate --runtime-role …
--projector-role …` → new server and projector builds. A 0010 server refuses 0011 (`ahead`);
a 0011 server refuses 0010 (`behind`); a 0011 projector started before the migration cannot
read the schema level of an ungranted 0010 database and refuses with the grant instruction
(`--projector-role`), and with only the metadata readable it refuses as `behind`
(`scripts/upgrade-p3.sh` asserts all four).

## Affected crates
new `ledger-projection`, `ledger-projection-fuseki`, `apps/ledger-projector`; `ledger-store`
(migration, verifier, repository); `apps/ledger-server` (`ledger-admin projection`);
`scripts/check-architecture.py`; docs.

## Quality gates
`check-fast`, `check-supply-chain`, PostgreSQL 15 + 17 suites, real Fuseki integration,
projection fault suite, compose integration, upgrade 0010 → 0011; Phase-2 suites stay green.

## Discoveries
- 2026-09-27, pinned `stain/jena-fuseki:5.1.0`, TDB2 dataset from an explicit assembler
  (`fuseki-server --config`): a multi-operation SPARQL Update whose last operation fails
  (`LOAD <urn:…>`) returns 500 and leaves no trace of its earlier `INSERT` (one transaction);
  the ADR-0020 guarded replace moved v1 → v2, and a stale v1 write and a duplicate v2 write
  were no-ops. Anonymous update on the named endpoint → 401; update on the dataset root with
  only named endpoints → 400; anonymous query → 200.
- The image's entrypoint creates TDB1 datasets and the default dataset templates add
  unnamed update/GSP endpoints on the dataset root; the ledger's compose/test Fuseki
  therefore uses its own assembler (TDB2, named `query` and `update` endpoints only).
- `/fuseki/configuration` must be writable in the webapp build; the explicit `--config`
  file avoids it.
- 2026-09-28, same image: TDB2 stores literals by value — `"01"^^xsd:integer` reads back as
  `"1"` and merges with a distinct ledger triple `"1"^^xsd:integer`; `"1.50"^^xsd:decimal` →
  `"1.5"`; `@EN` → `@en`; `"1"^^xsd:boolean` → `"true"`. A ledger-computed triple count and
  byte-exact comparison would therefore rebuild a correct projection forever; ADR-0020 now
  has the target compute `lp:tripleCount` in the write transaction and verifies rebuilds by
  `ASK` containment (target term equality). Pinned by
  `fuseki_projection::the_targets_literal_canonicalization_never_loops_or_blocks`.
- 2026-09-28: every TDB2 update commit costs ~0.5–1.1 s on the qualification host (even a
  one-triple `INSERT DATA`; queries take milliseconds). Twelve Fuseki tests sharing one
  dataset in parallel queued past the 10 s client timeout (correct, retried
  `TARGET_TIMEOUT`, but not what each test asserts); the suite runs `--test-threads=1`.
- 2026-09-28: the upgrade workload of Phase 2 puts named-graph quads on `main`; such streams
  block visibly (`NAMED_GRAPH_UNSUPPORTED`) in v1. The 0010 → 0011 harness therefore adds
  default-graph-only graphs to prove the backlog projection, and asserts the blocking of
  the others.
- Review round 1 (seven read-only reviewers on `898b122`) found, and this round fixed: an
  unguarded rebuild that a stalled worker could apply over a newer projection (now the
  ceiling-guarded replace); `DependencyTimeout` classified permanent (now retryable); an idle
  target that lost its data stayed empty until the next acceptance (now reconciliation +
  `TARGET_LOST`); two deployments or two KB feeds sharing one dataset/graph could overwrite
  each other (now `TARGET_CONFLICT` + dataset binding); literal canonicalization (above);
  projector role able to enable/disable streams (now owner-only by trigger); a full UNIQUE
  preventing re-pointing a KB after disable (now partial); non-`main` refs and bootstrap
  heads enable-able but never projectable (now refused); `work_for` reading head and event in
  two snapshots (now one query); language-tagged marker values accepted; probe accepting any
  failure (now HTTP 500 naming `LOAD`); secret files not checked as regular bounded files;
  `MAINTAIN` (PostgreSQL 17) not in the privilege model.
- Review round 2 (five read-only Opus reviewers on `d2daf55`: projection correctness,
  storage/concurrency, security, tests, Sculpin boundary). Consensus P1: version-number
  guards order nothing across streams, so a write in flight when an operator switched a KB's
  feed (disable, enable another tenant's graph, rebuild) could land over the new feed; a
  ceiling taken from garbage/ahead values admitted a queued stale replacement; an i64-
  overflowing numeric `refVersion` made recovery impossible. Fixed in `e67790b` by a
  compare-and-swap on the exact observed marker terms (transaction-local write token); a
  malformed marker whose parseable version exceeds the head is `MARKER_AHEAD`. Also fixed:
  reconciliation ignored backoff (tight loop on a failing idle stream) and a permanent
  ledger-state error was retryable; projector readiness skipped fingerprints and identity;
  0011 guard functions did not pin `search_path` and identities could hold CREATE outside
  `public`; re-enable after a KB change reported the wrong graph; union default graph not
  detected; architecture check bypassable by renamed/transitive dependencies (now on
  `cargo metadata`); contract overstated reconciliation and had an unsound freshness rule;
  upgrade harness failed its projector-skew check for the wrong reason (misleading identity
  message fixed); tests: missing real-target coverage for `MARKER_COMMIT_MISMATCH`, read-back
  and containment failures, probe pause, auth refusal; timing-dependent `attempts == 1`;
  serialization only in the script. Recorded, not fixed (tech debt): binding keyed on target
  id only, private CA, `_FILE` DB URL, response caps/copies, unauthenticated metrics DB work,
  shared `delivered_at`, cross-tenant KB re-point without confirmation.
- Codex (`codex exec -s read-only`) on `e78b1bd`: **P0** — the compare-and-swap was not
  ABA-free across feeds (B's stalled rebuild observed A's marker; B disabled, A re-enabled
  with an unchanged marker; B's late write would land in A's graph; not covered by the
  feed-switch test, where the marker changed). Fixed: a fresh `lp:writeId` per write and a
  two-phase disable that fences the target before the graph is freed (ADR-0020/0021);
  `fuseki_projection::a_stalled_rebuild_of_a_disabled_feed_never_lands_after_the_old_feed_returns`
  reproduces the sequence, and skipping the fence turns it red. P2s fixed: zero reconcile/
  probe intervals accepted (now minimums), the status server bound after the workers started
  (now before), CREATE reachable through a settable parent role not refused (now refused,
  tested).
- Codex round 2 on `212433c` (after the write-id fix): the write-id fence closes
  the ABA sequence; **P1** — an operator rebuild whose claim reply was delayed could observe
  another worker's newer projection and replace it with its older head (Replace has no
  version rule) → a forced rebuild now stops as superseded when the target holds a newer
  genuine state of the stream (`a_delayed_operator_rebuild_never_regresses_a_newer_projection`;
  a mutation removing the check turns it red); **P2** — the runtime identity model was not
  exhaustive (privileges on unlisted tables passed) → exhaustive for both identities
  (`weakened_projection_controls…` asserts the refusal for both).
- Codex round 3 on `bffa6e0`: **P0** — a rebuild whose claim reply was delayed past its
  lease could observe *after* its stream was fenced and another feed took the graph; its
  compare-and-swap against that current marker would pass. Fixed by checking, after the
  observation and before every write and fence, that the worker still holds its lease
  (ADR-0020 "Authority after observation");
  `a_rebuild_that_observes_after_losing_its_lease_writes_nothing` reproduces the schedule
  (a mutation removing the check turns it red).
- Codex round 4 on `0634a27`: no further target-write defect; **P1** — re-enabling a stream
  kept its recorded progress as current although another feed may have written the graph
  meanwhile (status "active, lag 0" until a later reconciliation) → re-enabling clears the
  stream's last check so reconciliation re-observes it at once (and reports
  `TARGET_CONFLICT` if another feed's marker is there); **P2** — column-level `REFERENCES`
  grants passed the identity check → refused (tested).

## Evidence
(filled as slices land)

## Sub-agent decomposition (§42)
Main session owns the protocol crate, the migration, the verifier and the repository.
Bounded read-only reviewers per slice (architecture, projection correctness,
storage/concurrency, security, fault recovery, tests, Sculpin boundary).
