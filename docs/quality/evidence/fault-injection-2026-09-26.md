# Fault-injection run `f6ab83f06` — PASS

Produced by `scripts/fault.sh 8 3 200 20` on branch `claude/p1.5-production-qualification` (run directory `target/fault/20260926T215212Z/`; raw JSON, the per-request in-doubt list and container logs are not committed). Eight `docker kill -s KILL` of alternating server replicas and three of the PostgreSQL container while 200 writers sustained prepare/accept traffic over 20 graphs. Kill moments followed observed progress (≥30 new commits since the previous recovery); recovery was detected by `/ready` polling; `ledger-admin verify` ran after every recovery (11 × `VERIFY OK`); the replicas' `StartedAt` was asserted unchanged across the PostgreSQL kills (pools reconnected, no restart). In every quiet window the writers were paused, in-flight requests drained, and every in-doubt response so far replayed verbatim (same key, body and actor) and checked against the database; `in-flight` in-doubt responses are the ones interrupted while being served, `never-sent` are connection refusals while a replica was down. `503 RESOURCE_LIMIT` is the expensive-operation admission control under 200 concurrent clients (intended backpressure). The three accepts and one prepare that had committed before their response was lost replayed identically with exactly one ref event each — observed by chance (the COMMIT-to-response window is sub-millisecond; an earlier 30-kill run observed none), so the deterministic proof of that case remains the `FailPoint` unit test (`pg_workflow`: lost response after COMMIT replays); every refused replay had zero ref events for its candidate; none was inconsistent. The predicates are the ones the `FailPoint` unit tests assert after each injected failure (ref unchanged, no event/decision/outbox/idempotency row from an aborted attempt, retry succeeds exactly once), so the unit and real-crash evidence are compared on the same terms.

Hardware: 12 × Intel(R) Core(TM) i7-4930K CPU @ 3.40GHz, 15 GiB RAM, kernel 5.10.0-44-amd64. PostgreSQL: PostgreSQL 17.2 (Debian 17.2-1.pgdg120+1) on x86_64-pc-linux-gnu, compiled by gcc (Debian 12.2.0-14) 12.2.0, 64-bit. Replicas: http://127.0.0.1:8080, http://127.0.0.1:8081.

200 writers over 20 graphs for 56.5 s: 93835 requests, 4514 commits landed during the run, 1967 in-doubt responses.

Latency per operation and outcome (replays included; `refused` = admission control):

| op | outcome | count | p50 ms | p95 ms | p99 ms | max ms |
|---|---|---:|---:|---:|---:|---:|
| ref_read | ok | 39921 | 4.1 | 29.6 | 1089.8 | 2379.8 |
| ref_read | failed | 2496 | 0.1 | 40.7 | 161.3 | 180.0 |
| prepare | ok | 9532 | 101.6 | 198.0 | 231.8 | 2573.7 |
| prepare | conflict | 1943 | 1.1 | 30.2 | 44.9 | 128.5 |
| prepare | refused | 28446 | 2.7 | 16.4 | 32.0 | 259.6 |
| prepare | failed | 1415 | 0.1 | 72.7 | 145.1 | 210.8 |
| accept | ok | 4517 | 13.2 | 35.8 | 52.6 | 294.0 |
| accept | conflict | 5013 | 6.5 | 29.4 | 40.2 | 1719.4 |
| accept | failed | 552 | 0.1 | 7.7 | 20.8 | 27.1 |

In-doubt responses per fault window (`in-flight` = connection died or `503 DEPENDENCY_*` while being served; `never-sent` = connection refused while the replica was down):

- `postgres-kill-1 / in-flight`: 27
- `postgres-kill-2 / in-flight`: 24
- `postgres-kill-3 / in-flight`: 28
- `server-kill-1 ledger / in-flight`: 83
- `server-kill-1 ledger / never-sent`: 271
- `server-kill-2 ledger-b / in-flight`: 64
- `server-kill-2 ledger-b / never-sent`: 218
- `server-kill-3 ledger / in-flight`: 62
- `server-kill-3 ledger / never-sent`: 186
- `server-kill-4 ledger-b / in-flight`: 53
- `server-kill-4 ledger-b / never-sent`: 203
- `server-kill-5 ledger / in-flight`: 50
- `server-kill-5 ledger / never-sent`: 152
- `server-kill-6 ledger-b / in-flight`: 44
- `server-kill-6 ledger-b / never-sent`: 144
- `server-kill-7 ledger / in-flight`: 36
- `server-kill-7 ledger / never-sent`: 150
- `server-kill-8 ledger-b / in-flight`: 33
- `server-kill-8 ledger-b / never-sent`: 139

Error classes: `503 DEPENDENCY_UNAVAILABLE` ×264, `503 RESOURCE_LIMIT` ×28446, `transport connect` ×3369, `transport other` ×830. Unexpected classes (fail the run): none.

## Replay of every in-doubt response (verbatim: same key, body and actor)

- prepare: 1 durable before the crash (replayed), 1 executed on retry, 1413 HEAD_CHANGED on retry
- accept: **3 durable before the crash** (replayed identically; exactly one ref event each; observed by chance — the COMMIT-to-response window is sub-millisecond; minimum demanded 0), 0 executed on retry (one ref event each), 549 refused HEAD_CHANGED (zero ref events each)
- unresolved replays: 0; **inconsistent: 0**


## Invariants

Σ refs.version = 4517 = Σ ref_events = 4517 = Σ accepted decisions = 4517 = Σ outbox rows = 4517; client-observed landings (run + replays) = 4517 with 4517 distinct (graph, version) pairs, 4517 found as ref events with the same version and head; 0 graph(s) violate. Verifier: clean (0 violating check(s)).

## Kill log

```
server-kill-1 ledger: SIGKILL at 187 commits, ready again after 0.9 s, verify OK, load progressed to 574, in-doubt 354, replayed 354, durable-before-crash accepts so far 0, inconsistent 0
server-kill-2 ledger-b: SIGKILL at 678 commits, ready again after 0.9 s, verify OK, load progressed to 1108, in-doubt 636, replayed 636, durable-before-crash accepts so far 1, inconsistent 0
server-kill-3 ledger: SIGKILL at 1163 commits, ready again after 0.9 s, verify OK, load progressed to 1518, in-doubt 884, replayed 884, durable-before-crash accepts so far 2, inconsistent 0
server-kill-4 ledger-b: SIGKILL at 1593 commits, ready again after 1.0 s, verify OK, load progressed to 1914, in-doubt 1140, replayed 1140, durable-before-crash accepts so far 2, inconsistent 0
server-kill-5 ledger: SIGKILL at 1958 commits, ready again after 0.9 s, verify OK, load progressed to 2234, in-doubt 1342, replayed 1342, durable-before-crash accepts so far 2, inconsistent 0
server-kill-6 ledger-b: SIGKILL at 2272 commits, ready again after 0.9 s, verify OK, load progressed to 2528, in-doubt 1530, replayed 1530, durable-before-crash accepts so far 2, inconsistent 0
server-kill-7 ledger: SIGKILL at 2567 commits, ready again after 0.9 s, verify OK, load progressed to 2818, in-doubt 1716, replayed 1716, durable-before-crash accepts so far 2, inconsistent 0
server-kill-8 ledger-b: SIGKILL at 2853 commits, ready again after 1.0 s, verify OK, load progressed to 3102, in-doubt 1888, replayed 1888, durable-before-crash accepts so far 2, inconsistent 0
server kills: 8; durable-before-crash accepts observed: 2 (minimum demanded 0)
postgres-kill-1: SIGKILL at 3174 commits, both replicas ready again after 2.4 s without restart (crash recovery), verify OK, load progressed to 3594, in-doubt 1915, inconsistent 0
postgres-kill-2: SIGKILL at 3632 commits, both replicas ready again after 1.8 s without restart (crash recovery), verify OK, load progressed to 4038, in-doubt 1939, inconsistent 0
postgres-kill-3: SIGKILL at 4059 commits, both replicas ready again after 1.9 s without restart (crash recovery), verify OK, load progressed to 4458, in-doubt 1967, inconsistent 0
```
