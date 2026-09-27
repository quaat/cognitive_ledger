# Performance baselines (Plan 0005 §10)

Reproduce with `scripts/bench.sh [depths] [samples]` (`ledger-stress bench`): one client, one
graph, a linear history built through the public API (one quad added per commit), and at each
listed chain depth the latency of `prepare`, `accept`, a ref read and a state read at the head,
measured 20 times each with no concurrent load, against the production-shaped compose stack
(owner migration, runtime identity, distroless image). No throughput target exists yet; these
numbers are the accepted baseline that later changes are gated against (`performance-testing.md`).

## 2026-09-27 baseline (build `9b60d23` + the reviewed fixes, `scripts/bench.sh 1,100,1000,10000 20`)

Hardware: 12 × Intel Core i7-4930K @ 3.40 GHz, 15 GiB RAM, Linux 5.10, PostgreSQL 17.2 in
Docker (`shared_buffers` 128 MB, default configuration), server pool 16 connections,
distroless image, one server replica, client on the same host. Prepare/accept are measured on
the last 20 commits before each depth (1 at depth 1); ref and state reads 20 times at exactly
that depth. Building the 10,000-commit history took 10,156 s (2.8 h) of serial API time.

**Growing state** (one quad added per commit, so the state has `depth` quads):

| depth | quads in state | prepare p50 / p95 ms | accept p50 / p95 ms | ref read p50 / p95 ms | state read p50 / p95 ms |
|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 11.4 / 11.4 | 8.2 / 8.2 | 0.5 / 0.7 | 0.7 / 1.0 |
| 100 | 100 | 18.1 / 29.0 | 3.0 / 5.5 | 0.8 / 1.3 | 29.8 / 47.9 |
| 1,000 | 1,000 | 216.0 / 431.0 | 3.6 / 7.1 | 0.8 / 1.3 | 187.8 / 312.9 |
| 10,000 | 10,000 | 1,904.6 / 2,071.7 | 4.4 / 6.6 | 0.9 / 1.4 | 1,982.7 / 2,198.0 |

**Constant-state control** (`--constant-state`: each commit adds one quad and deletes the
previous one, so the state stays at one quad while the history deepens; 139 s to build):

| depth | quads in state | prepare p50 / p95 ms | accept p50 / p95 ms | ref read p50 / p95 ms | state read p50 / p95 ms |
|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 9.0 / 9.0 | 8.1 / 8.1 | 0.5 / 0.7 | 0.8 / 1.0 |
| 100 | 1 | 29.5 / 52.3 | 5.0 / 8.0 | 0.7 / 1.7 | 31.0 / 49.5 |
| 1,000 | 1 | 200.1 / 346.8 | 4.0 / 7.8 | 0.8 / 1.0 | 201.2 / 270.8 |

## Reading

Two experiments were run because one quad per commit makes state size equal to depth: the
growing-state run and the constant-state control. **The control reproduces the growth with a
one-quad state (200 ms at depth 1,000 vs 216 ms with 1,000 quads), so history depth — one
object fetch and hash per ancestor — dominates; state size contributes little at these sizes.**
Per ancestor the cost is ≈0.19–0.20 ms, linear up to depth 10,000 (1.9 s per prepare and per
state read at the development `max_depth`).

- **Accept and ref reads are flat** (≈3–4 ms and ≈1 ms): the accept transaction touches the
  ref row, one event, one decision, one outbox row and the idempotency record; depth does not
  enter.
- **Prepare and state reads grow linearly with depth** (11 → 18 → 216 → 1,905 ms p50 from
  depth 1 to 10,000). Both fold every patch from genesis (`WorkflowRepository::reconstruct` /
  `state_at_bounded`): one object fetch per ancestor (two sequential `SELECT`s plus a
  SHA-256 re-hash each) and a set insert per quad; the control shows the per-ancestor part
  is what grows.
- **Cheaper remedies to measure before any persistent cache:** batching the ancestor fetch
  into one query (or one round trip per k ancestors) and an in-process cache of verified
  content-addressed objects remove round trips without adding persistent state; since the
  control run shows the per-ancestor cost dominates, those come first and are expected to
  cut the constant by a large factor, but not the linear growth itself.
- The stress-run p99 figures (`docs/quality/evidence/stress-1000-writers-2026-09-26.md`) include
  queueing and contention and are not comparable to these single-client numbers.

## Checkpoint policy proposal (input to Phase 4/5; no implementation in Plan 0005)

Reconstruction cost is proportional to history length; the cheaper remedies lower the constant
but not the growth, so a checkpoint (materialized state snapshot) policy is needed before
production-length histories (a 10,000-commit ref already costs ≈2 s per prepare).
Constraints the design must respect (from ADR-0008, ADR-0012, ADR-0013, ADR-0016 and the
product specification, which requires that a corrupt checkpoint is detected and cannot
redefine commit state):

1. **A checkpoint never decides identity.** Under ADR-0008 the effective delta, hence the
   `PatchId`/`CommitId`, depends on the resolved base state. `prepare` therefore may use a
   checkpoint only if that checkpoint is *verified*: either written by a trusted writer
   that folded from genesis (the owner identity or a verifier job, never the runtime, whose
   compromise is an accepted residual risk under ADR-0016), or re-verified by the reader
   against the fold before use. Reads (`state`) may use unverified checkpoints only if they
   are labelled as such; the default is verified-only.
2. **Snapshot format is protocol.** A content-addressed snapshot (`sha256:` of a canonical
   N-Quads serialization of the full state) is a new hashed canonical format: it needs an
   ADR and golden vectors, not only a table. The content address proves the bytes are the
   ones written, not that they are the correct state for `commit_id`; only the fold proves
   that, so "verified" means "the writer folded, or a verifier re-folded, and recorded the
   digest match" — checked on every checkpoint before it is trusted, not on a sample.
3. **Placement:** a checkpoint row `state_checkpoints(commit_id, snapshot_id, quads, bytes,
   verified_by, created_at)` every *k* commits per ref (k sized from the control run so a
   fold from the nearest checkpoint stays within tens of milliseconds), written outside the
   acceptance transaction (after COMMIT, idempotent, never blocking acceptance) by a
   dedicated checkpointer identity — not the serving runtime and not a long-lived owner
   credential (ADR-0016 keeps the owner on the operator host): a role with `INSERT` on the
   checkpoint table and `SELECT` elsewhere, excluded from `ledger_grant_runtime`, and the
   server's forbidden-privilege startup check extended to that table.
4. **Never dropped, never edited.** Snapshot objects live in `immutable_objects` (write-once
   guard, ADR-0012 leaves large snapshots to object storage later) and index rows are
   append-only; the `max_depth` limit keeps its ADR-0013 meaning (an accepted head stays
   readable under the limits that accepted it) because a checkpoint only shortens the fold,
   its absence never lengthens it beyond the history length that was accepted.
5. **Verification:** every checkpoint is verified (digest equals fold digest) before it is
   ever used; `ledger-admin verify` re-checks a bounded random subset per run, covering the
   full set over a schedule, and asserts "no checkpoint referenced by a prepare was
   unverified".

Decision required before Phase 4 (branches make deep histories more common): the ADR for the
snapshot format and the checkpoint table (persistent identity of snapshots, who writes them,
atomicity relative to acceptance, verification before use).
