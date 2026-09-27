# Stress run `6ab837ac` — PASS

Produced by `scripts/stress.sh 1000 60 100 3` on branch `claude/p1.5-production-qualification` (image `sha256:24653c2154da…`, both replicas as uid 65532, `LEDGER_AUTH_MODE=dev-hs256`, server pool 16 connections and `max_concurrent_expensive` 12 per replica; run directory `target/stress/20260926T212049Z/`, raw JSON not committed). Reading guide: `refused` rows are `503 RESOURCE_LIMIT`, the expensive-operation admission control refusing immediately under 1,000 concurrent clients on two replicas — intended backpressure, retried by the clients with backoff, not a failure class; only the `ok` rows are held to the p99 budget. The contended phase is an adversarial worst case (every writer targets the same ref; only one accept per round can win, the rest are `HEAD_CHANGED` conflicts). Duplicated-request pairs count as *compared* only when both replicas answered; pairs with one side refused by admission control are listed separately and prove nothing about replay. Every client-observed landing was checked against `ref_events` (same version and head), every non-transient error class fails the run, and the deadlock counter was read after PostgreSQL's statistics flush interval.

Hardware: 12 × Intel(R) Core(TM) i7-4930K CPU @ 3.40GHz, 15 GiB RAM, kernel 5.10.0-44-amd64. PostgreSQL: PostgreSQL 17.2 (Debian 17.2-1.pgdg120+1) on x86_64-pc-linux-gnu, compiled by gcc (Debian 12.2.0-14) 12.2.0, 64-bit (`max_connections` 100, `shared_buffers` 128MB). Replicas: http://127.0.0.1:8080, http://127.0.0.1:8081.

## contended (one graph, time-boxed) — PASS

1000 writers over 1 graph(s), 60.6 s wall, 320066 requests (5278 req/s), 282 commits landed (4.6 commits/s), 13686 conflicts (HEAD_CHANGED/LINEAGE_MISMATCH), writers incomplete 1000, gave up 0.

Latency per operation and outcome (successful operations are the ones held to the p99 budget; `refused` = `503 RESOURCE_LIMIT` admission control):

| op | outcome | count | p50 ms | p95 ms | p99 ms | max ms | mean ms |
|---|---|---:|---:|---:|---:|---:|---:|
| ref_read | ok | 146182 | 36.3 | 63.3 | 98.6 | 1369.6 | 39.6 |
| prepare | ok | 8945 | 166.9 | 296.8 | 326.6 | 381.7 | 171.7 |
| prepare | conflict | 3484 | 43.0 | 69.4 | 104.2 | 1117.3 | 46.3 |
| prepare | refused | 150895 | 18.6 | 34.3 | 44.7 | 1131.5 | 19.7 |
| accept | ok | 358 | 44.3 | 68.0 | 112.0 | 128.8 | 45.4 |
| accept | conflict | 10202 | 39.4 | 64.6 | 82.3 | 168.0 | 40.0 |

Successful p99 budget 5000 ms: accept 112.0 ms, prepare 326.6 ms, ref_read 98.6 ms.

Error classes: `503 RESOURCE_LIMIT` ×150895. Unexpected classes (fail the run): none.

Duplicated requests (same key, two replicas, concurrent): 188 pairs compared with both answers (exactly one original execution each, identical durable fields), 1686 pairs both conflicting identically, 1615 pairs with one side refused by admission control or lost, 15380 pairs with neither side answering; **0 disagreement(s)**.

Runtime-role sessions (sampled every 250 ms, 238 samples): max total 32, max active 12, mean active 2.6, max waiting on locks 6. Deadlocks during phase: 0.

Invariants over 1 graph(s): Σ refs.version = 282 = Σ ref_events = 282 = Σ accepted decisions = 282 = Σ outbox rows = 282; client-observed landings = 282 with 282 distinct (graph, version) pairs, 282 of them found as ref events with the same version and head; 0 graph(s) violate.

## independent (many graphs, fixed commits per writer) — PASS

1000 writers over 100 graph(s), 8.7 s wall, 47116 requests (5436 req/s), 3000 commits landed (346.1 commits/s), 1627 conflicts (HEAD_CHANGED/LINEAGE_MISMATCH), writers incomplete 0, gave up 0.

Latency per operation and outcome (successful operations are the ones held to the p99 budget; `refused` = `503 RESOURCE_LIMIT` admission control):

| op | outcome | count | p50 ms | p95 ms | p99 ms | max ms | mean ms |
|---|---|---:|---:|---:|---:|---:|---:|
| ref_read | ok | 20398 | 42.0 | 110.2 | 129.9 | 156.7 | 50.6 |
| prepare | ok | 4085 | 58.6 | 114.5 | 156.9 | 188.4 | 62.6 |
| prepare | conflict | 476 | 42.5 | 113.0 | 147.9 | 160.2 | 49.0 |
| prepare | refused | 17706 | 24.2 | 68.9 | 90.5 | 125.8 | 31.0 |
| accept | ok | 3300 | 41.6 | 97.6 | 132.5 | 168.8 | 44.6 |
| accept | conflict | 1151 | 38.2 | 90.9 | 140.7 | 154.0 | 42.4 |

Successful p99 budget 5000 ms: accept 132.5 ms, prepare 156.9 ms, ref_read 129.9 ms.

Error classes: `503 RESOURCE_LIMIT` ×17706. Unexpected classes (fail the run): none.

Duplicated requests (same key, two replicas, concurrent): 371 pairs compared with both answers (exactly one original execution each, identical durable fields), 147 pairs both conflicting identically, 366 pairs with one side refused by admission control or lost, 1422 pairs with neither side answering; **0 disagreement(s)**.

Runtime-role sessions (sampled every 250 ms, 35 samples): max total 32, max active 9, mean active 3.4, max waiting on locks 5. Deadlocks during phase: 0.

Invariants over 100 graph(s): Σ refs.version = 3000 = Σ ref_events = 3000 = Σ accepted decisions = 3000 = Σ outbox rows = 3000; client-observed landings = 3000 with 3000 distinct (graph, version) pairs, 3000 of them found as ref events with the same version and head; 0 graph(s) violate.

## Verifier

`ledger_store::verify` over the whole database: clean (0 violating check(s)).
