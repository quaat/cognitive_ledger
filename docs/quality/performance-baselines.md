# Performance baselines (Plan 0005 §10)

Reproduce with `scripts/bench.sh [depths] [samples]` (`ledger-stress bench`): one client, one
graph, a linear history built through the public API (one quad added per commit), and at each
listed chain depth the latency of `prepare`, `accept`, a ref read and a state read at the head,
measured 20 times each with no concurrent load, against the production-shaped compose stack
(owner migration, runtime identity, distroless image). No throughput target exists yet; these
numbers are the accepted baseline that later changes are gated against (`performance-testing.md`).

## 2026-09-27 baseline — RUN IN PROGRESS (depth 10,000 pending)

Hardware: 12 × Intel Core i7-4930K @ 3.40 GHz, 15 GiB RAM, Linux 5.10, PostgreSQL 17.2 in
Docker (`shared_buffers` 128 MB, default configuration), server pool 16 connections. Numbers
below are from the first run (aborted at depth 5,902 by client-token expiry, since fixed; the
depth-10,000 row is being produced by the rerun and will replace this section).

| depth | quads in state | prepare p50 / p95 ms | accept p50 / p95 ms | ref read p50 / p95 ms | state read p50 / p95 ms |
|---:|---:|---:|---:|---:|---:|
| 1 | 20 | 4.8 / 7.6 | 3.2 / 4.5 | 1.5 / 1.7 | 11.1 / 12.7 |
| 100 | 119 | 20.5 / 27.5 | 2.9 / 3.7 | 0.6 / 1.6 | 29.5 / 56.0 |
| 1,000 | 1,019 | 194.7 / 281.8 | 3.7 / 5.3 | 0.9 / 1.1 | 211.0 / 345.6 |
| 10,000 | — | pending | pending | pending | pending |

## Reading

- **Accept and ref reads are flat** (≈3–4 ms and ≈1 ms): the accept transaction touches the
  ref row, one event, one decision, one outbox row and the idempotency record; depth does
  not enter.
- **Prepare and state reads grow linearly with depth** (≈0.19 ms per ancestor commit on this
  hardware: 5 → 20 → 195 ms from depth 1 to 1,000). Both reconstruct the state by folding
  every patch from genesis (`WorkflowRepository::reconstruct` / `state_at_bounded`), because
  the ledger has no checkpoints yet. Extrapolated, depth 10,000 costs ≈2 s per prepare and
  per state read, and building a 10,000-deep history costs ≈2.6 h of serial API time; the
  development reconstruction limit (`max_depth` 10,000) is where the current design stops.
- **Under concurrency** (`docs/quality/evidence/stress-1000-writers-2026-09-26.md`) the same
  shape holds: successful prepare p99 was 157–327 ms at depths ≤ 300, admission control
  (12 expensive slots per replica) refuses the excess immediately.

## Checkpoint policy proposal (input to Phase 4/5; no implementation in Plan 0005)

Reconstruction depth dominates prepare latency well before depth 1,000, so a checkpoint
(materialized state snapshot) policy is required before histories of production length:

1. **Content-addressed state snapshots** stored as immutable objects (`sha256:` of the
   canonical N-Quads of the full state) and indexed by commit id in a new table
   `state_checkpoints(commit_id, snapshot_id, quads, bytes, created_at)`; never part of a
   commit's identity (protocol v2 stays frozen), so a checkpoint is a cache with a proof:
   `snapshot_id` must equal the digest of the fold from genesis, verifiable by
   `ledger-admin verify` on a sample.
2. **Policy:** checkpoint every *k* commits per ref (k ≈ 64–256, chosen so that a
   reconstruction folds at most k patches ≈ 15–50 ms on this hardware) and additionally
   whenever the fold exceeds a time budget; written by the accept path *after* COMMIT
   (asynchronously, idempotent, never blocking acceptance) or by a background sweeper under
   the runtime identity with `INSERT`-only privilege on the checkpoint table.
3. **Reads:** reconstruction starts from the nearest checkpoint at or below the target
   commit and folds forward; the `max_depth` limit then bounds the distance to the nearest
   checkpoint rather than the history length.
4. **Safety:** a missing or corrupt checkpoint degrades to the full fold (correctness never
   depends on the cache); the verifier gets a check "checkpoint digest equals fold digest"
   over a random sample; checkpoints are dropped, never edited.

Decision required before Phase 4 (branches make deep histories more common): an ADR for the
checkpoint table (persistent identity of snapshots, atomicity relative to acceptance).
