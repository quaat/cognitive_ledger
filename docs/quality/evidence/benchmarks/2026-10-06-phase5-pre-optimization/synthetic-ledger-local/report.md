# Benchmark run — profile `local`: **pass**

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
| server_image | `sha256:fa02b0c7ba9ec4558b8012480572ee2e47fef517d790b0195e5ab51ba1806731` |
| server_toolchain | `FROM rust:1.89-bookworm@sha256:948f9b08a66e7fe01b03a98ef1c7568292e07ec2e4fe90d88c07bb14563c84ff AS build` |
| tracked_changes | `1` |
| untracked_files | `2` |
| wall time | 328.6 s |
| host | Debian GNU/Linux 11 (bullseye) · kernel 5.10.0-44-amd64 · 12 × Intel(R) Core(TM) i7-4930K CPU @ 3.40GHz · 15936 MiB RAM |
| PostgreSQL | 17.2 (Debian 17.2-1.pgdg120+1) |
| server | http://127.0.0.1:8080 |

## Dataset `synthetic-ledger-local`

Generator `ledger-bench synthetic` `synthetic-ledger-gen/3`, seed `0x5eed000110ca1000`; checksum `sha256:9f678bd99c3d04af61e3ed41680862891d0ea3510556666479899658d5c35cfd` (matches the manifest); license: Apache-2.0 (project-owned generated data).

1530 commits (15 integration), 9 branches, 26 merge previews, 15 merges applied, 20 designed conflicts; genesis 12200 quads, final `main` 13660 quads; max parent-0 depth 615.

### Correctness: 4945 assertions, 0 failed

| check | assertions |
|---|---:|
| branch head | 9 |
| commit parents | 1530 |
| commit provenance | 1530 |
| diff(A, B) == expected | 6 |
| first-parent history (branch log) | 9 |
| historical reconstruction | 72 |
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
| state after commit | 1515 |
| state after merge (reconstruct(I) == merged) | 15 |

### Observations (not gates)

| phase | ms |
|---|---:|
| diff checks | 1495 |
| generate (oracle) | 12662 |
| historical reconstruction checks | 7026 |
| ingest and per-step checks | 306843 |
| persisted parents and provenance | 340 |
| throughput: ingest_steps_per_s (commits + branches + applied merges, with per-step checks) | 5.01 |

All samples: (p95/p99 shown only for n ≥ 20)

| category | operation | group | n | p50 ms | p95 ms | p99 ms | max ms | mean ms |
|---|---|---|---:|---:|---:|---:|---:|---:|
| algorithm | rdf_diff | all | 6 | 0.7 | — | — | 1.9 | 0.9 |
| api | accept | all | 1515 | 4.3 | 7.4 | 9.4 | 116.1 | 4.8 |
| api | branch_create | all | 8 | 11.1 | — | — | 63.3 | 34.7 |
| api | branch_log | all | 9 | 70.7 | — | — | 180.4 | 75.6 |
| api | merge_apply | all | 15 | 4.3 | — | — | 6.4 | 4.7 |
| api | merge_preview | all | 26 | 598.0 | 676.8 | 718.7 | 718.7 | 525.4 |
| api | merge_propose | all | 15 | 656.6 | — | — | 731.8 | 609.0 |
| api | prepare | all | 1515 | 77.9 | 149.2 | 165.6 | 235.3 | 85.1 |
| api | ref_read | all | 9 | 1.0 | — | — | 1.8 | 1.1 |
| api | state_read_head | all | 1530 | 81.2 | 156.6 | 183.3 | 244.1 | 89.3 |
| api | state_read_historical | all | 72 | 111.8 | 162.7 | 182.4 | 182.4 | 98.9 |
| persisted | commit_read | all | 1530 | 0.2 | 0.3 | 0.5 | 8.4 | 0.2 |

By history kind and parent-0 depth: (p95/p99 shown only for n ≥ 20)

| category | operation | group | n | p50 ms | p95 ms | p99 ms | max ms | mean ms |
|---|---|---|---:|---:|---:|---:|---:|---:|
| api | merge_preview | feature · depth 100-199 | 1 | 413.0 | — | — | 413.0 | 413.0 |
| api | merge_preview | feature · depth 200-499 | 4 | 665.9 | — | — | 718.7 | 673.0 |
| api | merge_preview | main · depth 200-499 | 6 | 500.1 | — | — | 672.5 | 523.8 |
| api | merge_preview | merge · depth 200-499 | 15 | 598.0 | — | — | 675.6 | 494.2 |
| api | prepare | bulk · depth 0-9 | 3 | 43.9 | — | — | 48.6 | 44.6 |
| api | prepare | churn · depth 0-9 | 2 | 39.1 | — | — | 40.6 | 39.9 |
| api | prepare | churn · depth 10-49 | 40 | 45.8 | 76.0 | 92.5 | 92.5 | 51.1 |
| api | prepare | churn · depth 100-199 | 100 | 68.1 | 81.8 | 84.9 | 86.7 | 68.3 |
| api | prepare | churn · depth 200-499 | 108 | 85.2 | 113.9 | 141.2 | 167.1 | 89.5 |
| api | prepare | churn · depth 50-99 | 50 | 52.4 | 90.1 | 96.3 | 96.3 | 56.8 |
| api | prepare | feature · depth 10-49 | 74 | 44.9 | 73.9 | 89.7 | 89.7 | 49.0 |
| api | prepare | feature · depth 100-199 | 27 | 62.2 | 108.3 | 111.1 | 111.1 | 68.0 |
| api | prepare | feature · depth 200-499 | 107 | 118.9 | 142.3 | 157.6 | 158.3 | 122.1 |
| api | prepare | feature · depth 50-99 | 100 | 52.5 | 76.5 | 96.5 | 99.8 | 56.4 |
| api | prepare | growth · depth 0-9 | 2 | 37.1 | — | — | 39.2 | 38.1 |
| api | prepare | growth · depth 10-49 | 40 | 44.0 | 73.1 | 96.2 | 96.2 | 48.4 |
| api | prepare | growth · depth 100-199 | 100 | 66.9 | 90.1 | 106.3 | 119.1 | 69.7 |
| api | prepare | growth · depth 200-499 | 108 | 85.2 | 110.7 | 152.1 | 157.0 | 88.5 |
| api | prepare | growth · depth 50-99 | 50 | 53.5 | 75.7 | 90.3 | 90.3 | 56.6 |
| api | prepare | main · depth 0-9 | 7 | 38.5 | — | — | 67.7 | 42.5 |
| api | prepare | main · depth 10-49 | 40 | 45.5 | 68.6 | 171.7 | 171.7 | 52.2 |
| api | prepare | main · depth 100-199 | 100 | 67.4 | 100.7 | 112.4 | 121.6 | 71.0 |
| api | prepare | main · depth 200-499 | 291 | 103.4 | 144.9 | 162.7 | 163.6 | 109.0 |
| api | prepare | main · depth 50-99 | 50 | 53.0 | 76.7 | 96.9 | 96.9 | 56.0 |
| api | prepare | main · depth 500-999 | 116 | 149.3 | 197.7 | 219.4 | 235.3 | 152.9 |
| api | state_read_head | bulk · depth 0-9 | 3 | 31.0 | — | — | 37.5 | 28.2 |
| api | state_read_head | churn · depth 0-9 | 2 | 38.3 | — | — | 63.5 | 50.9 |
| api | state_read_head | churn · depth 10-49 | 40 | 45.7 | 64.7 | 84.5 | 84.5 | 49.5 |
| api | state_read_head | churn · depth 100-199 | 100 | 68.7 | 83.2 | 91.1 | 99.0 | 69.6 |
| api | state_read_head | churn · depth 200-499 | 108 | 89.1 | 139.1 | 154.6 | 173.6 | 95.6 |
| api | state_read_head | churn · depth 50-99 | 50 | 53.4 | 78.2 | 98.4 | 98.4 | 57.3 |
| api | state_read_head | feature · depth 10-49 | 74 | 44.9 | 67.7 | 93.7 | 93.7 | 48.1 |
| api | state_read_head | feature · depth 100-199 | 27 | 63.6 | 90.3 | 96.5 | 96.5 | 67.4 |
| api | state_read_head | feature · depth 200-499 | 107 | 123.4 | 144.8 | 174.7 | 193.7 | 126.8 |
| api | state_read_head | feature · depth 50-99 | 100 | 54.9 | 85.7 | 109.9 | 114.2 | 59.7 |
| api | state_read_head | growth · depth 0-9 | 2 | 37.9 | — | — | 43.9 | 40.9 |
| api | state_read_head | growth · depth 10-49 | 40 | 45.1 | 71.7 | 89.0 | 89.0 | 49.7 |
| api | state_read_head | growth · depth 100-199 | 100 | 69.8 | 84.7 | 94.6 | 123.7 | 71.4 |
| api | state_read_head | growth · depth 200-499 | 108 | 91.5 | 121.9 | 124.1 | 172.2 | 94.7 |
| api | state_read_head | growth · depth 50-99 | 50 | 53.7 | 69.6 | 100.2 | 100.2 | 56.7 |
| api | state_read_head | main · depth 0-9 | 7 | 38.8 | — | — | 51.5 | 42.3 |
| api | state_read_head | main · depth 10-49 | 40 | 44.9 | 74.2 | 84.9 | 84.9 | 50.1 |
| api | state_read_head | main · depth 100-199 | 100 | 71.1 | 121.9 | 145.6 | 159.7 | 76.7 |
| api | state_read_head | main · depth 200-499 | 291 | 110.1 | 156.9 | 184.4 | 223.6 | 115.8 |
| api | state_read_head | main · depth 50-99 | 50 | 54.1 | 99.3 | 103.7 | 103.7 | 60.2 |
| api | state_read_head | main · depth 500-999 | 116 | 156.0 | 194.7 | 217.1 | 244.1 | 160.4 |
| api | state_read_head | merge · depth 100-199 | 1 | 63.7 | — | — | 63.7 | 63.7 |
| api | state_read_head | merge · depth 200-499 | 14 | 121.7 | — | — | 156.1 | 126.9 |
| api | state_read_historical | bulk · depth 0-9 | 4 | 30.8 | — | — | 40.3 | 31.3 |
| api | state_read_historical | churn · depth 100-199 | 2 | 56.6 | — | — | 64.7 | 60.6 |
| api | state_read_historical | churn · depth 200-499 | 4 | 82.5 | — | — | 111.8 | 91.6 |
| api | state_read_historical | churn · depth 50-99 | 1 | 48.1 | — | — | 48.1 | 48.1 |
| api | state_read_historical | feature · depth 100-199 | 3 | 61.2 | — | — | 73.9 | 65.1 |
| api | state_read_historical | feature · depth 200-499 | 9 | 128.8 | — | — | 161.3 | 130.4 |
| api | state_read_historical | feature · depth 50-99 | 2 | 49.5 | — | — | 53.4 | 51.4 |
| api | state_read_historical | growth · depth 100-199 | 2 | 76.9 | — | — | 96.2 | 86.5 |
| api | state_read_historical | growth · depth 200-499 | 4 | 88.9 | — | — | 93.8 | 90.2 |
| api | state_read_historical | growth · depth 50-99 | 1 | 47.9 | — | — | 47.9 | 47.9 |
| api | state_read_historical | main · depth 0-9 | 3 | 38.9 | — | — | 39.6 | 39.0 |
| api | state_read_historical | main · depth 10-49 | 2 | 40.1 | — | — | 46.5 | 43.3 |
| api | state_read_historical | main · depth 100-199 | 2 | 55.1 | — | — | 65.0 | 60.0 |
| api | state_read_historical | main · depth 200-499 | 9 | 112.8 | — | — | 152.7 | 109.1 |
| api | state_read_historical | main · depth 50-99 | 1 | 47.1 | — | — | 47.1 | 47.1 |
| api | state_read_historical | main · depth 500-999 | 5 | 162.7 | — | — | 169.5 | 163.7 |
| api | state_read_historical | merge · depth 100-199 | 1 | 63.6 | — | — | 63.6 | 63.6 |
| api | state_read_historical | merge · depth 200-499 | 17 | 121.1 | — | — | 182.4 | 127.7 |

### Resources

| resource | value |
|---|---|
| harness peak RSS (includes oracle generation) | 153.8 MiB |
| database before | 8.8 MiB |
| database after | 20.4 MiB |
| `immutable_objects` on disk after | 1.9 MiB |
| `immutable_objects` logical bytes after | 1.7 MiB |
| phases | harness-build 19s;dataset-validate 15s;stack-build-and-start 140s;benchmark-run 329s; |
| postgres_peak_memory | 212 MiB (memory.current sampled every 0.5 s) |
| server_peak_memory | 84 MiB (memory.current sampled every 0.5 s) |

