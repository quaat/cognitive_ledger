# Benchmark run — profile `ci`: **pass**

Generated from `result.json` (sculpin-ledger-bench-result/v1); the JSON is authoritative.

| run | |
|---|---|
| harness | ledger-bench 0.1.0 |
| acceptance_mode | `unvalidated-development` |
| build_rev | `a68865e81dad095fbf9e9ba092a8bef57b22d5c3` |
| compose | `2.39.2-desktop.1` |
| compose_project | `ledger-qual-benchmark` |
| docker | `29.7.2` |
| inputs_sha256(Dockerfile,compose.yaml,Cargo.lock) | `3b6cedd2860ee46031eb80abf37c89b93e4f580867104b5dd86063377e49fe5d` |
| rustc | `rustc 1.89.0 (29483883e 2025-08-04)` |
| server_image | `sha256:4af7cb1af2c028260351348c73a5c1c381c8ec69e21ca5e379afa2595d4f298c` |
| server_toolchain | `FROM rust:1.89-bookworm@sha256:948f9b08a66e7fe01b03a98ef1c7568292e07ec2e4fe90d88c07bb14563c84ff AS build` |
| tracked_changes | `1` |
| untracked_files | `2` |
| wall time | 26.8 s |
| host | Debian GNU/Linux 11 (bullseye) · kernel 5.10.0-44-amd64 · 12 × Intel(R) Core(TM) i7-4930K CPU @ 3.40GHz · 15936 MiB RAM |
| PostgreSQL | 17.2 (Debian 17.2-1.pgdg120+1) |
| server | http://127.0.0.1:8080 |

## Dataset `synthetic-ledger-ci`

Generator `ledger-bench synthetic` `synthetic-ledger-gen/3`, seed `0x5eed0001c1ed6e00`; checksum `sha256:c8ad2fc2c2ada5f2df436898799c58093333e11d285d6536cabbcc79c1faf7ff` (matches the manifest); license: Apache-2.0 (project-owned generated data).

205 commits (15 integration), 9 branches, 26 merge previews, 15 merges applied, 10 designed conflicts; genesis 6100 quads, final `main` 6271 quads; max parent-0 depth 64.

### Correctness: 1115 assertions, 0 failed

| check | assertions |
|---|---:|
| branch head | 9 |
| commit parents | 205 |
| commit provenance | 205 |
| diff(A, B) == expected | 6 |
| first-parent history (branch log) | 9 |
| historical reconstruction | 217 |
| merge ahead/behind | 26 |
| merge apply moves the target to the candidate | 15 |
| merge base | 26 |
| merge base candidates | 26 |
| merge classification | 26 |
| merge conflict count | 26 |
| merge conflict keys | 26 |
| merge delta summary | 44 |
| merge preview token presence | 26 |
| merged state digest | 18 |
| state after commit | 190 |
| state after merge (reconstruct(I) == merged) | 15 |

### Observations (not gates)

| phase | ms |
|---|---:|
| diff checks | 487 |
| generate (oracle) | 1086 |
| historical reconstruction checks | 6392 |
| ingest and per-step checks | 18578 |
| persisted parents and provenance | 42 |
| throughput: ingest_steps_per_s (commits + branches + applied merges, with per-step checks) | 11.47 |

All samples: (p95/p99 shown only for n ≥ 20)

| category | operation | group | n | p50 ms | p95 ms | p99 ms | max ms | mean ms |
|---|---|---|---:|---:|---:|---:|---:|---:|
| algorithm | rdf_diff | all | 6 | 0.3 | — | — | 0.4 | 0.3 |
| api | accept | all | 190 | 4.0 | 6.6 | 20.0 | 20.7 | 4.6 |
| api | branch_create | all | 8 | 7.2 | — | — | 9.7 | 7.3 |
| api | branch_log | all | 9 | 11.2 | — | — | 13.3 | 10.1 |
| api | merge_apply | all | 15 | 4.5 | — | — | 6.4 | 4.7 |
| api | merge_preview | all | 26 | 140.6 | 163.4 | 167.2 | 167.2 | 127.2 |
| api | merge_propose | all | 15 | 158.1 | — | — | 184.3 | 153.4 |
| api | prepare | all | 190 | 27.9 | 41.6 | 48.2 | 49.2 | 29.2 |
| api | ref_read | all | 9 | 0.8 | — | — | 1.2 | 0.8 |
| api | state_read_head | all | 205 | 26.4 | 37.0 | 42.4 | 44.6 | 27.4 |
| api | state_read_historical | all | 217 | 26.0 | 41.1 | 50.1 | 52.5 | 28.0 |
| persisted | commit_read | all | 205 | 0.2 | 0.2 | 0.3 | 1.1 | 0.2 |

By history kind and parent-0 depth: (p95/p99 shown only for n ≥ 20)

| category | operation | group | n | p50 ms | p95 ms | p99 ms | max ms | mean ms |
|---|---|---|---:|---:|---:|---:|---:|---:|
| api | merge_preview | feature · depth 10-49 | 1 | 128.3 | — | — | 128.3 | 128.3 |
| api | merge_preview | feature · depth 50-99 | 4 | 150.5 | — | — | 163.4 | 154.7 |
| api | merge_preview | main · depth 10-49 | 2 | 139.3 | — | — | 155.9 | 147.6 |
| api | merge_preview | main · depth 50-99 | 4 | 131.1 | — | — | 154.8 | 137.8 |
| api | merge_preview | merge · depth 10-49 | 11 | 140.1 | — | — | 159.2 | 108.7 |
| api | merge_preview | merge · depth 50-99 | 4 | 154.7 | — | — | 167.2 | 129.8 |
| api | prepare | bulk · depth 0-9 | 2 | 30.3 | — | — | 41.6 | 35.9 |
| api | prepare | churn · depth 0-9 | 3 | 21.1 | — | — | 38.7 | 26.9 |
| api | prepare | churn · depth 10-49 | 32 | 27.8 | 35.7 | 49.2 | 49.2 | 28.5 |
| api | prepare | feature · depth 10-49 | 47 | 28.5 | 40.3 | 46.0 | 46.0 | 28.5 |
| api | prepare | feature · depth 50-99 | 17 | 32.0 | — | — | 44.0 | 33.7 |
| api | prepare | growth · depth 0-9 | 3 | 21.5 | — | — | 21.5 | 21.3 |
| api | prepare | growth · depth 10-49 | 32 | 27.2 | 43.7 | 48.2 | 48.2 | 29.0 |
| api | prepare | main · depth 0-9 | 8 | 24.9 | — | — | 33.2 | 25.8 |
| api | prepare | main · depth 10-49 | 34 | 26.4 | 38.5 | 40.6 | 40.6 | 27.3 |
| api | prepare | main · depth 50-99 | 12 | 35.5 | — | — | 43.6 | 37.0 |
| api | state_read_head | bulk · depth 0-9 | 2 | 17.2 | — | — | 19.7 | 18.4 |
| api | state_read_head | churn · depth 0-9 | 3 | 21.5 | — | — | 25.1 | 22.2 |
| api | state_read_head | churn · depth 10-49 | 32 | 25.5 | 36.5 | 36.7 | 36.7 | 27.1 |
| api | state_read_head | feature · depth 10-49 | 47 | 26.0 | 38.6 | 39.2 | 39.2 | 26.7 |
| api | state_read_head | feature · depth 50-99 | 17 | 31.2 | — | — | 37.0 | 31.7 |
| api | state_read_head | growth · depth 0-9 | 3 | 20.3 | — | — | 22.2 | 20.9 |
| api | state_read_head | growth · depth 10-49 | 32 | 26.2 | 41.1 | 44.6 | 44.6 | 27.6 |
| api | state_read_head | main · depth 0-9 | 8 | 22.1 | — | — | 25.2 | 22.3 |
| api | state_read_head | main · depth 10-49 | 34 | 25.3 | 29.9 | 43.5 | 43.5 | 25.8 |
| api | state_read_head | main · depth 50-99 | 12 | 32.3 | — | — | 38.0 | 32.9 |
| api | state_read_head | merge · depth 10-49 | 7 | 27.7 | — | — | 28.8 | 27.7 |
| api | state_read_head | merge · depth 50-99 | 8 | 30.2 | — | — | 42.4 | 32.5 |
| api | state_read_historical | bulk · depth 0-9 | 3 | 19.5 | — | — | 21.6 | 19.1 |
| api | state_read_historical | churn · depth 0-9 | 3 | 21.3 | — | — | 25.3 | 22.6 |
| api | state_read_historical | churn · depth 10-49 | 33 | 25.4 | 29.7 | 37.0 | 37.0 | 25.7 |
| api | state_read_historical | feature · depth 10-49 | 47 | 25.8 | 37.5 | 51.7 | 51.7 | 27.7 |
| api | state_read_historical | feature · depth 50-99 | 19 | 32.5 | — | — | 50.1 | 36.0 |
| api | state_read_historical | growth · depth 0-9 | 3 | 20.8 | — | — | 21.5 | 20.9 |
| api | state_read_historical | growth · depth 10-49 | 33 | 24.9 | 40.6 | 40.7 | 40.7 | 25.9 |
| api | state_read_historical | main · depth 0-9 | 10 | 20.9 | — | — | 27.9 | 22.2 |
| api | state_read_historical | main · depth 10-49 | 35 | 26.2 | 36.6 | 37.4 | 37.4 | 27.3 |
| api | state_read_historical | main · depth 50-99 | 13 | 34.6 | — | — | 42.1 | 35.9 |
| api | state_read_historical | merge · depth 10-49 | 10 | 29.7 | — | — | 31.9 | 29.3 |
| api | state_read_historical | merge · depth 50-99 | 8 | 30.8 | — | — | 52.5 | 33.7 |

### Resources

| resource | value |
|---|---|
| harness peak RSS (includes oracle generation) | 65.8 MiB |
| database before | 8.8 MiB |
| database after | 10.9 MiB |
| `immutable_objects` on disk after | 0.4 MiB |
| `immutable_objects` logical bytes after | 0.4 MiB |
| phases | harness-build 0s;dataset-validate 14s;stack-build-and-start 15s;benchmark-run 27s; |
| postgres_peak_memory | 135 MiB (memory.current sampled every 0.5 s) |
| server_peak_memory | 45 MiB (memory.current sampled every 0.5 s) |

