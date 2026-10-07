# Reconstruction characterization: **pass**

Generated from `recon.json` (sculpin-ledger-bench-recon/v1); the JSON is authoritative. p95 only for n ≥ 20 (the second-largest of 20), p99 only for n ≥ 100. PostgreSQL figures are per operation, from `pg_stat_statements` (8 KiB buffer counts, not physical disk bytes). CPU is cgroup `cpu.stat` per operation of the whole container, read immediately around the measured operations (statistics queries outside); memory is `memory.current` after the batch (page cache included). `db-restart-first-ledger-op`: PostgreSQL restarted, then readiness and statistics queries, then the measured first ledger reconstruction; OS page cache not dropped.

| | |
|---|---|
| acceptance_mode | `unvalidated-development` |
| build_rev | `7408a3eea3982adc4c6b5aae54e5041818f9b6b4` |
| compose | `5.5.1` |
| compose_project | `ledger-qual-recon` |
| docker | `29.8.1` |
| docker_userland_proxy_processes | `45` |
| inputs_sha256(Dockerfile,compose.yaml,benchmark/compose.instrumented.yaml,Cargo.lock,deploy/postgres-init) | `fa79d33514812ba6d30ac91848f81833f0fac929249918c9f26a42d7e65d36e5` |
| official | `yes` |
| postgres | `17.2 (Debian 17.2-1.pgdg120+1)` |
| postgres_config | `benchmark-only instrumentation override (pg_stat_statements, track_io_timing=on); differs from the production-shaped compose.yaml` |
| postgres_image | `sha256:3267c505060a0052e5aa6e5175a7b41ab6b04da2f8c4540fc6e98a37210aa2d3` |
| rustc | `rustc 1.89.0 (29483883e 2025-08-04)` |
| server_image | `sha256:3e15c0b4bd1c0fcbc7ce9492699569143c5bfd50227465dce688b9cce1d2f1e9` |
| server_toolchain | `FROM rust:1.89-bookworm@sha256:948f9b08a66e7fe01b03a98ef1c7568292e07ec2e4fe90d88c07bb14563c84ff AS build` |
| tracked_changes | `0` |
| tracked_diff_sha256 | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| untracked_files | `0` |
| wall_s | `703` |
| cache_conditions | `warm: after warm-up on an active database; db-restart-first-ledger-op: PostgreSQL process and shared buffers restarted, then /ready, pool reconnection and statistics queries, then the measured first ledger reconstruction (OS page cache NOT dropped)` |
| cpu | `16 × Intel(R) Xeon(R) W-1270P CPU @ 3.80GHz` |
| depths | `[1, 10, 100, 500, 1000, 2500, 5000]` |
| kernel | `6.12.100+deb13-amd64` |
| mem_total_mib | `31878` |
| os | `Debian GNU/Linux 13 (trixie)` |
| reps | `20 (+3 warm-up); previews 10; cold 3 at [100, 1000, 5000]` |
| settle: CHECKPOINT | `done` |
| settle: VACUUM (ANALYZE) | `done` |
| states | `[1, 1000, 10000]` |
| build: all histories (concurrent) | 517.3 s |
| build: settle (vacuum analyze, checkpoint) | 1.1 s |

Correctness: {"API state equals the oracle state (digest)": 21, "merge preview classification": 42, "merge preview replies (classification, merged state digest)": 42, "prefetched fold equals the oracle state (digest)": 21, "prepared candidate equals base state plus probe (digest)": 21, "state after a database restart equals the oracle state (digest)": 18, "store reconstruction equals the oracle state (digest)": 21}; failures: 0

| S quads | depth | fold ops | state bytes | category | op | cache | n | p50 ms | p95 ms | mean ms | resp KiB | PG calls | rows | blks hit | blks read | read ms | exec ms | server CPU ms | PG CPU ms |
|---:|---:|---:|---:|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 3 | 36 | api | state_read | warm | 20 | 0.53 | 0.7 | 0.55 | 0.1 | 4.0 | 6.0 | 28.0 | 0.0 | 0.0 | 0.1 | 0.3 | 0.3 |
| 1 | 1 | 3 | 36 | persisted | store_reconstruct | warm | 20 | 0.47 | 0.5 | 0.47 | — | 2.0 | 4.0 | 23.0 | 0.0 | 0.0 | 0.1 | 0.0 | 0.3 |
| 1 | 1 | 3 | 36 | algorithm | fold_cpu | warm | 20 | 0.01 | 0.0 | 0.01 | — | — | — | — | — | — | — | — | — |
| 1 | 1 | 3 | 36 | api | prepare | warm | 20 | 2.36 | 2.9 | 2.44 | — | 28.0 | 26.0 | 197.3 | 0.0 | 0.0 | 0.8 | 0.7 | 1.6 |
| 1 | 1 | 3 | 36 | api | merge_preview_contained | warm | 10 | 1.52 | — | 1.69 | — | 8.0 | 9.0 | 75.9 | 0.0 | 0.0 | 0.1 | 0.4 | 1.3 |
| 1 | 1 | 3 | 36 | api | merge_preview_divergent | warm | 10 | 1.88 | — | 2.18 | — | 14.0 | 26.0 | 185.0 | 0.0 | 0.0 | 0.3 | 0.6 | 1.6 |
| 1 | 10 | 21 | 37 | api | state_read | warm | 20 | 0.64 | 0.7 | 0.65 | 0.1 | 4.0 | 24.0 | 136.0 | 0.0 | 0.0 | 0.2 | 0.3 | 0.3 |
| 1 | 10 | 21 | 37 | persisted | store_reconstruct | warm | 20 | 0.56 | 0.6 | 0.56 | — | 2.0 | 22.0 | 131.0 | 0.0 | 0.0 | 0.2 | 0.0 | 0.3 |
| 1 | 10 | 21 | 37 | algorithm | fold_cpu | warm | 20 | 0.06 | 0.1 | 0.06 | — | — | — | — | — | — | — | — | — |
| 1 | 10 | 21 | 37 | api | prepare | warm | 20 | 2.62 | 2.9 | 2.63 | — | 28.0 | 44.0 | 306.1 | 0.0 | 0.0 | 0.9 | 0.8 | 1.7 |
| 1 | 10 | 21 | 37 | api | merge_preview_contained | warm | 10 | 1.75 | — | 2.37 | — | 10.0 | 27.0 | 335.5 | 0.0 | 0.0 | 0.4 | 0.5 | 1.8 |
| 1 | 10 | 21 | 37 | api | merge_preview_divergent | warm | 10 | 3.00 | — | 3.56 | — | 16.0 | 98.0 | 768.0 | 0.0 | 0.0 | 0.9 | 1.0 | 2.5 |
| 1 | 100 | 201 | 38 | api | state_read | warm | 20 | 2.86 | 3.1 | 2.90 | 0.1 | 4.0 | 204.0 | 1216.0 | 0.0 | 0.0 | 1.4 | 1.4 | 1.6 |
| 1 | 100 | 201 | 38 | persisted | store_reconstruct | warm | 20 | 2.63 | 2.8 | 2.64 | — | 2.0 | 202.0 | 1211.0 | 0.0 | 0.0 | 1.3 | 0.0 | 1.5 |
| 1 | 100 | 201 | 38 | algorithm | fold_cpu | warm | 20 | 0.56 | 0.8 | 0.60 | — | — | — | — | — | — | — | — | — |
| 1 | 100 | 201 | 38 | api | prepare | warm | 20 | 4.80 | 5.4 | 4.84 | — | 28.0 | 224.0 | 1385.4 | 0.0 | 0.0 | 2.0 | 1.8 | 2.8 |
| 1 | 100 | 201 | 38 | api | merge_preview_contained | warm | 10 | 5.36 | — | 6.47 | — | 14.0 | 207.0 | 2941.5 | 0.0 | 0.0 | 3.3 | 0.9 | 5.5 |
| 1 | 100 | 201 | 38 | api | merge_preview_divergent | warm | 10 | 13.85 | — | 14.71 | — | 20.0 | 818.0 | 6614.0 | 0.0 | 0.0 | 7.6 | 4.8 | 10.2 |
| 1 | 100 | 201 | 38 | api | state_read | db-restart-first-ledger-op | 3 | 6.71 | — | 6.62 | — | 4.0 | 204.0 | 913.0 | 306.0 | 1.1 | 2.8 | — | — |
| 1 | 100 | 201 | 38 | persisted | store_reconstruct | db-restart-first-ledger-op | 3 | 5.77 | — | 5.82 | — | 2.0 | 202.0 | 913.0 | 301.0 | 1.1 | 2.7 | — | — |
| 1 | 500 | 1001 | 38 | api | state_read | warm | 20 | 12.26 | 13.3 | 12.49 | 0.1 | 6.0 | 1004.0 | 6016.0 | 0.0 | 0.0 | 6.5 | 5.7 | 7.0 |
| 1 | 500 | 1001 | 38 | persisted | store_reconstruct | warm | 20 | 12.21 | 17.9 | 12.96 | — | 4.0 | 1002.0 | 6011.0 | 0.0 | 0.0 | 7.3 | 0.0 | 7.7 |
| 1 | 500 | 1001 | 38 | algorithm | fold_cpu | warm | 20 | 2.68 | 2.7 | 2.69 | — | — | — | — | — | — | — | — | — |
| 1 | 500 | 1001 | 38 | api | prepare | warm | 20 | 15.30 | 16.4 | 15.36 | — | 30.0 | 1024.0 | 6182.4 | 2.5 | 0.0 | 7.8 | 6.3 | 11.4 |
| 1 | 500 | 1001 | 38 | api | merge_preview_contained | warm | 10 | 20.96 | — | 23.80 | — | 16.0 | 1007.0 | 14539.8 | 0.0 | 0.0 | 17.6 | 2.8 | 20.6 |
| 1 | 500 | 1001 | 38 | api | merge_preview_divergent | warm | 10 | 59.91 | — | 62.43 | — | 28.0 | 4018.0 | 32612.0 | 0.0 | 0.0 | 38.8 | 21.1 | 42.6 |
| 1 | 1000 | 2001 | 39 | api | state_read | warm | 20 | 25.16 | 28.3 | 25.48 | 0.1 | 10.0 | 2004.0 | 12016.0 | 0.0 | 0.0 | 13.6 | 11.9 | 14.4 |
| 1 | 1000 | 2001 | 39 | persisted | store_reconstruct | warm | 20 | 25.27 | 32.7 | 27.27 | — | 8.0 | 2002.0 | 12011.0 | 0.0 | 0.0 | 13.7 | 0.0 | 14.2 |
| 1 | 1000 | 2001 | 39 | algorithm | fold_cpu | warm | 20 | 5.36 | 5.5 | 5.38 | — | — | — | — | — | — | — | — | — |
| 1 | 1000 | 2001 | 39 | api | prepare | warm | 20 | 27.12 | 35.5 | 28.78 | — | 34.0 | 2024.0 | 12184.6 | 1.6 | 0.0 | 14.3 | 13.5 | 15.8 |
| 1 | 1000 | 2001 | 39 | api | merge_preview_contained | warm | 10 | 39.89 | — | 40.10 | — | 20.0 | 2007.0 | 30022.0 | 0.0 | 0.0 | 33.6 | 5.1 | 34.8 |
| 1 | 1000 | 2001 | 39 | api | merge_preview_divergent | warm | 10 | 111.99 | — | 116.24 | — | 44.0 | 8018.0 | 66036.4 | 0.0 | 0.0 | 74.9 | 41.9 | 77.9 |
| 1 | 1000 | 2001 | 39 | api | state_read | db-restart-first-ledger-op | 3 | 32.16 | — | 32.73 | — | 10.0 | 2004.0 | 10886.0 | 1133.0 | 4.2 | 18.4 | — | — |
| 1 | 1000 | 2001 | 39 | persisted | store_reconstruct | db-restart-first-ledger-op | 3 | 33.44 | — | 33.18 | — | 8.0 | 2002.0 | 10886.0 | 1128.0 | 4.4 | 19.2 | — | — |
| 1 | 2500 | 5001 | 39 | api | state_read | warm | 20 | 62.30 | 79.1 | 65.74 | 0.1 | 22.0 | 5004.0 | 30016.0 | 0.0 | 0.0 | 35.2 | 31.4 | 39.2 |
| 1 | 2500 | 5001 | 39 | persisted | store_reconstruct | warm | 20 | 76.11 | 92.1 | 75.45 | — | 20.0 | 5002.0 | 30011.0 | 0.0 | 0.0 | 42.0 | 0.0 | 43.4 |
| 1 | 2500 | 5001 | 39 | algorithm | fold_cpu | warm | 20 | 16.44 | 24.6 | 17.55 | — | — | — | — | — | — | — | — | — |
| 1 | 2500 | 5001 | 39 | api | prepare | warm | 20 | 62.91 | 67.3 | 63.71 | — | 46.0 | 5024.0 | 30185.5 | 2.1 | 0.0 | 34.6 | 28.5 | 36.9 |
| 1 | 2500 | 5001 | 39 | api | merge_preview_contained | warm | 10 | 103.01 | — | 107.39 | — | 32.0 | 5007.0 | 74965.1 | 0.0 | 0.0 | 91.7 | 13.7 | 94.2 |
| 1 | 2500 | 5001 | 39 | api | merge_preview_divergent | warm | 10 | 295.92 | — | 297.02 | — | 92.0 | 20018.0 | 165071.0 | 0.0 | 0.0 | 197.0 | 103.0 | 203.6 |
| 1 | 5000 | 10001 | 39 | api | state_read | warm | 20 | 121.43 | 133.9 | 123.31 | 0.1 | 42.0 | 10004.0 | 60016.0 | 0.0 | 0.0 | 68.3 | 56.7 | 71.1 |
| 1 | 5000 | 10001 | 39 | persisted | store_reconstruct | warm | 20 | 124.25 | 134.0 | 126.79 | — | 40.0 | 10002.0 | 60011.0 | 0.0 | 0.0 | 70.2 | 0.0 | 72.5 |
| 1 | 5000 | 10001 | 39 | algorithm | fold_cpu | warm | 20 | 26.77 | 27.9 | 26.90 | — | — | — | — | — | — | — | — | — |
| 1 | 5000 | 10001 | 39 | api | prepare | warm | 20 | 130.73 | 140.4 | 131.62 | — | 66.0 | 10024.0 | 60185.6 | 1.8 | 0.0 | 71.2 | 60.6 | 75.0 |
| 1 | 5000 | 10001 | 39 | api | merge_preview_contained | warm | 10 | 202.22 | — | 200.38 | — | 52.0 | 10007.0 | 149970.0 | 0.0 | 0.0 | 172.8 | 25.9 | 176.1 |
| 1 | 5000 | 10001 | 39 | api | merge_preview_divergent | warm | 10 | 579.08 | — | 607.38 | — | 172.0 | 40018.0 | 330043.0 | 0.0 | 0.0 | 402.3 | 212.3 | 413.4 |
| 1 | 5000 | 10001 | 39 | api | state_read | db-restart-first-ledger-op | 3 | 135.44 | — | 136.84 | — | 42.0 | 10004.0 | 57768.0 | 2251.0 | 8.8 | 78.2 | — | — |
| 1 | 5000 | 10001 | 39 | persisted | store_reconstruct | db-restart-first-ledger-op | 3 | 134.36 | — | 136.28 | — | 40.0 | 10002.0 | 57768.0 | 2246.0 | 9.1 | 77.8 | — | — |
| 1000 | 1 | 1002 | 39783 | api | state_read | warm | 20 | 2.37 | 2.8 | 2.48 | 42.8 | 4.0 | 6.0 | 31.0 | 0.0 | 0.0 | 0.1 | 1.8 | 0.5 |
| 1000 | 1 | 1002 | 39783 | persisted | store_reconstruct | warm | 20 | 2.04 | 2.2 | 2.06 | — | 2.0 | 4.0 | 26.0 | 0.0 | 0.0 | 0.1 | 0.0 | 0.4 |
| 1000 | 1 | 1002 | 39783 | algorithm | fold_cpu | warm | 20 | 1.16 | 1.2 | 1.16 | — | — | — | — | — | — | — | — | — |
| 1000 | 1 | 1002 | 39783 | api | prepare | warm | 20 | 4.55 | 5.2 | 4.56 | — | 28.0 | 25.0 | 196.2 | 2.4 | 0.0 | 1.0 | 2.1 | 2.1 |
| 1000 | 1 | 1002 | 39783 | api | merge_preview_contained | warm | 10 | 1.20 | — | 1.66 | — | 8.0 | 9.0 | 75.3 | 0.0 | 0.0 | 0.1 | 0.4 | 1.2 |
| 1000 | 1 | 1002 | 39783 | api | merge_preview_divergent | warm | 10 | 9.71 | — | 10.27 | — | 14.0 | 26.0 | 194.0 | 0.0 | 0.0 | 0.5 | 8.0 | 2.2 |
| 1000 | 10 | 1020 | 39809 | api | state_read | warm | 20 | 2.65 | 2.8 | 2.67 | 42.9 | 4.0 | 24.0 | 139.0 | 0.0 | 0.0 | 0.2 | 1.9 | 0.6 |
| 1000 | 10 | 1020 | 39809 | persisted | store_reconstruct | warm | 20 | 2.00 | 2.2 | 2.03 | — | 2.0 | 22.0 | 134.0 | 0.0 | 0.0 | 0.2 | 0.0 | 0.5 |
| 1000 | 10 | 1020 | 39809 | algorithm | fold_cpu | warm | 20 | 1.21 | 1.2 | 1.22 | — | — | — | — | — | — | — | — | — |
| 1000 | 10 | 1020 | 39809 | api | prepare | warm | 20 | 4.76 | 5.2 | 4.71 | — | 28.0 | 43.0 | 304.4 | 2.1 | 0.0 | 1.1 | 2.2 | 2.2 |
| 1000 | 10 | 1020 | 39809 | api | merge_preview_contained | warm | 10 | 1.34 | — | 1.41 | — | 10.0 | 27.0 | 342.0 | 0.0 | 0.0 | 0.4 | 0.4 | 0.9 |
| 1000 | 10 | 1020 | 39809 | api | merge_preview_divergent | warm | 10 | 10.80 | — | 10.92 | — | 16.0 | 98.0 | 784.0 | 0.0 | 0.0 | 1.2 | 8.3 | 2.3 |
| 1000 | 100 | 1200 | 39988 | api | state_read | warm | 20 | 4.62 | 5.2 | 4.69 | 43.0 | 4.0 | 204.0 | 1219.0 | 0.0 | 0.0 | 1.4 | 2.9 | 1.7 |
| 1000 | 100 | 1200 | 39988 | persisted | store_reconstruct | warm | 20 | 3.96 | 4.1 | 3.98 | — | 2.0 | 202.0 | 1214.0 | 0.0 | 0.0 | 1.4 | 0.0 | 1.5 |
| 1000 | 100 | 1200 | 39988 | algorithm | fold_cpu | warm | 20 | 1.73 | 1.7 | 1.72 | — | — | — | — | — | — | — | — | — |
| 1000 | 100 | 1200 | 39988 | api | prepare | warm | 20 | 6.26 | 6.9 | 6.37 | — | 28.0 | 223.0 | 1383.5 | 1.8 | 0.0 | 2.1 | 3.2 | 3.0 |
| 1000 | 100 | 1200 | 39988 | api | merge_preview_contained | warm | 10 | 5.40 | — | 5.37 | — | 14.0 | 207.0 | 3034.0 | 0.0 | 0.0 | 3.4 | 1.0 | 4.1 |
| 1000 | 100 | 1200 | 39988 | api | merge_preview_divergent | warm | 10 | 20.52 | — | 20.59 | — | 20.0 | 818.0 | 6716.0 | 0.0 | 0.0 | 7.7 | 12.0 | 8.8 |
| 1000 | 100 | 1200 | 39988 | api | state_read | db-restart-first-ledger-op | 3 | 8.31 | — | 8.32 | — | 4.0 | 204.0 | 939.0 | 306.0 | 1.1 | 2.8 | — | — |
| 1000 | 100 | 1200 | 39988 | persisted | store_reconstruct | db-restart-first-ledger-op | 3 | 7.34 | — | 7.38 | — | 2.0 | 202.0 | 939.0 | 301.0 | 1.1 | 2.8 | — | — |
| 1000 | 500 | 2000 | 40388 | api | state_read | warm | 20 | 15.15 | 20.5 | 16.29 | 43.4 | 6.0 | 1004.0 | 6019.0 | 0.0 | 0.0 | 8.3 | 7.6 | 8.9 |
| 1000 | 500 | 2000 | 40388 | persisted | store_reconstruct | warm | 20 | 13.60 | 13.9 | 13.57 | — | 4.0 | 1002.0 | 6014.0 | 0.0 | 0.0 | 6.6 | 0.0 | 7.0 |
| 1000 | 500 | 2000 | 40388 | algorithm | fold_cpu | warm | 20 | 3.96 | 4.1 | 3.97 | — | — | — | — | — | — | — | — | — |
| 1000 | 500 | 2000 | 40388 | api | prepare | warm | 20 | 16.42 | 21.7 | 17.05 | — | 30.0 | 1023.0 | 6183.6 | 2.6 | 0.0 | 8.0 | 7.7 | 9.4 |
| 1000 | 500 | 2000 | 40388 | api | merge_preview_contained | warm | 10 | 21.50 | — | 22.02 | — | 16.0 | 1007.0 | 14997.1 | 0.0 | 0.0 | 17.5 | 2.9 | 18.8 |
| 1000 | 500 | 2000 | 40388 | api | merge_preview_divergent | warm | 10 | 66.13 | — | 65.96 | — | 28.0 | 4018.0 | 33112.0 | 0.0 | 0.0 | 37.7 | 28.0 | 39.5 |
| 1000 | 1000 | 3000 | 40890 | api | state_read | warm | 20 | 26.43 | 27.8 | 26.65 | 43.9 | 10.0 | 2004.0 | 12019.0 | 0.0 | 0.0 | 13.4 | 13.1 | 14.1 |
| 1000 | 1000 | 3000 | 40890 | persisted | store_reconstruct | warm | 20 | 25.52 | 27.6 | 25.76 | — | 8.0 | 2002.0 | 12014.0 | 0.0 | 0.0 | 13.2 | 0.0 | 13.8 |
| 1000 | 1000 | 3000 | 40890 | algorithm | fold_cpu | warm | 20 | 6.75 | 6.8 | 6.70 | — | — | — | — | — | — | — | — | — |
| 1000 | 1000 | 3000 | 40890 | api | prepare | warm | 20 | 29.40 | 32.5 | 29.97 | — | 34.0 | 2023.0 | 12185.2 | 1.7 | 0.0 | 15.0 | 14.1 | 16.5 |
| 1000 | 1000 | 3000 | 40890 | api | merge_preview_contained | warm | 10 | 44.08 | — | 44.12 | — | 20.0 | 2007.0 | 30022.0 | 0.0 | 0.0 | 37.2 | 5.9 | 38.6 |
| 1000 | 1000 | 3000 | 40890 | api | merge_preview_divergent | warm | 10 | 125.09 | — | 128.47 | — | 44.0 | 8018.0 | 66104.0 | 0.0 | 0.0 | 78.6 | 50.3 | 81.5 |
| 1000 | 1000 | 3000 | 40890 | api | state_read | db-restart-first-ledger-op | 3 | 41.42 | — | 41.48 | — | 10.0 | 2004.0 | 10898.0 | 1147.0 | 4.8 | 23.3 | — | — |
| 1000 | 1000 | 3000 | 40890 | persisted | store_reconstruct | db-restart-first-ledger-op | 3 | 33.67 | — | 33.82 | — | 8.0 | 2002.0 | 10898.0 | 1142.0 | 4.2 | 18.5 | — | — |
| 1000 | 2500 | 6000 | 40890 | api | state_read | warm | 20 | 63.80 | 76.6 | 66.30 | 43.9 | 22.0 | 5004.0 | 30019.0 | 0.0 | 0.0 | 36.3 | 30.6 | 40.2 |
| 1000 | 2500 | 6000 | 40890 | persisted | store_reconstruct | warm | 20 | 63.92 | 84.6 | 67.86 | — | 20.0 | 5002.0 | 30014.0 | 0.0 | 0.0 | 35.1 | 0.0 | 36.4 |
| 1000 | 2500 | 6000 | 40890 | algorithm | fold_cpu | warm | 20 | 15.19 | 15.8 | 15.25 | — | — | — | — | — | — | — | — | — |
| 1000 | 2500 | 6000 | 40890 | api | prepare | warm | 20 | 65.17 | 66.5 | 65.17 | — | 46.0 | 5023.0 | 30184.2 | 2.1 | 0.0 | 34.3 | 30.3 | 36.6 |
| 1000 | 2500 | 6000 | 40890 | api | merge_preview_contained | warm | 10 | 104.50 | — | 104.38 | — | 32.0 | 5007.0 | 74969.1 | 0.0 | 0.0 | 89.4 | 13.2 | 91.9 |
| 1000 | 2500 | 6000 | 40890 | api | merge_preview_divergent | warm | 10 | 297.03 | — | 299.27 | — | 92.0 | 20018.0 | 165084.0 | 0.0 | 0.0 | 192.1 | 109.3 | 198.1 |
| 1000 | 5000 | 11000 | 40890 | api | state_read | warm | 20 | 131.72 | 162.0 | 137.71 | 43.9 | 42.0 | 10004.0 | 60019.0 | 0.0 | 0.0 | 74.9 | 65.3 | 78.0 |
| 1000 | 5000 | 11000 | 40890 | persisted | store_reconstruct | warm | 20 | 128.84 | 132.6 | 128.33 | — | 40.0 | 10002.0 | 60014.0 | 0.0 | 0.0 | 69.9 | 0.0 | 72.4 |
| 1000 | 5000 | 11000 | 40890 | algorithm | fold_cpu | warm | 20 | 29.38 | 30.5 | 29.44 | — | — | — | — | — | — | — | — | — |
| 1000 | 5000 | 11000 | 40890 | api | prepare | warm | 20 | 123.79 | 164.5 | 130.05 | — | 66.0 | 10023.0 | 60190.1 | 1.8 | 0.0 | 68.4 | 60.6 | 72.0 |
| 1000 | 5000 | 11000 | 40890 | api | merge_preview_contained | warm | 10 | 216.06 | — | 222.36 | — | 52.0 | 10007.0 | 150068.0 | 0.0 | 0.0 | 190.1 | 30.4 | 194.1 |
| 1000 | 5000 | 11000 | 40890 | api | merge_preview_divergent | warm | 10 | 603.36 | — | 605.48 | — | 172.0 | 40018.0 | 330150.0 | 0.0 | 0.0 | 392.7 | 219.6 | 404.4 |
| 1000 | 5000 | 11000 | 40890 | api | state_read | db-restart-first-ledger-op | 3 | 139.37 | — | 141.09 | — | 42.0 | 10004.0 | 57781.0 | 2264.0 | 8.8 | 78.0 | — | — |
| 1000 | 5000 | 11000 | 40890 | persisted | store_reconstruct | db-restart-first-ledger-op | 3 | 140.48 | — | 141.04 | — | 40.0 | 10002.0 | 57781.0 | 2259.0 | 9.0 | 80.0 | — | — |
| 10000 | 1 | 10000 | 417780 | api | state_read | warm | 20 | 17.64 | 18.1 | 17.70 | 447.1 | 4.0 | 6.0 | 42.0 | 0.0 | 0.0 | 0.5 | 15.4 | 1.1 |
| 10000 | 1 | 10000 | 417780 | persisted | store_reconstruct | warm | 20 | 15.07 | 16.8 | 15.40 | — | 2.0 | 4.0 | 37.0 | 0.0 | 0.0 | 0.5 | 0.0 | 3.2 |
| 10000 | 1 | 10000 | 417780 | algorithm | fold_cpu | warm | 20 | 12.46 | 13.4 | 12.61 | — | — | — | — | — | — | — | — | — |
| 10000 | 1 | 10000 | 417780 | api | prepare | warm | 20 | 18.99 | 19.9 | 18.95 | — | 28.0 | 25.0 | 212.8 | 2.2 | 0.0 | 1.6 | 15.3 | 3.1 |
| 10000 | 1 | 10000 | 417780 | api | merge_preview_contained | warm | 10 | 1.19 | — | 1.73 | — | 8.0 | 9.0 | 81.3 | 0.0 | 0.0 | 0.2 | 0.4 | 1.3 |
| 10000 | 1 | 10000 | 417780 | api | merge_preview_divergent | warm | 10 | 84.26 | — | 87.34 | — | 14.0 | 26.0 | 233.0 | 0.0 | 0.0 | 1.5 | 83.6 | 3.6 |
| 10000 | 10 | 10018 | 417815 | api | state_read | warm | 20 | 17.58 | 19.1 | 18.34 | 447.2 | 4.0 | 24.0 | 150.0 | 0.0 | 0.0 | 0.6 | 16.3 | 1.0 |
| 10000 | 10 | 10018 | 417815 | persisted | store_reconstruct | warm | 20 | 15.18 | 16.2 | 15.36 | — | 2.0 | 22.0 | 145.0 | 0.0 | 0.0 | 0.6 | 0.0 | 0.9 |
| 10000 | 10 | 10018 | 417815 | algorithm | fold_cpu | warm | 20 | 12.39 | 13.0 | 12.44 | — | — | — | — | — | — | — | — | — |
| 10000 | 10 | 10018 | 417815 | api | prepare | warm | 20 | 19.07 | 20.3 | 19.52 | — | 28.0 | 43.0 | 322.6 | 2.1 | 0.0 | 1.7 | 15.9 | 3.1 |
| 10000 | 10 | 10018 | 417815 | api | merge_preview_contained | warm | 10 | 1.35 | — | 1.43 | — | 10.0 | 27.0 | 348.0 | 0.0 | 0.0 | 0.5 | 0.4 | 0.9 |
| 10000 | 10 | 10018 | 417815 | api | merge_preview_divergent | warm | 10 | 83.80 | — | 84.23 | — | 16.0 | 98.0 | 823.0 | 0.0 | 0.0 | 2.2 | 80.5 | 3.5 |
| 10000 | 100 | 10198 | 418084 | api | state_read | warm | 20 | 19.92 | 21.2 | 20.12 | 447.4 | 4.0 | 204.0 | 1230.0 | 0.0 | 0.0 | 2.0 | 16.9 | 2.4 |
| 10000 | 100 | 10198 | 418084 | persisted | store_reconstruct | warm | 20 | 17.32 | 20.9 | 17.76 | — | 2.0 | 202.0 | 1225.0 | 0.0 | 0.0 | 2.0 | 0.0 | 2.2 |
| 10000 | 100 | 10198 | 418084 | algorithm | fold_cpu | warm | 20 | 16.30 | 16.5 | 16.17 | — | — | — | — | — | — | — | — | — |
| 10000 | 100 | 10198 | 418084 | api | prepare | warm | 20 | 24.09 | 24.8 | 24.11 | — | 28.0 | 223.0 | 1402.0 | 1.5 | 0.0 | 3.2 | 20.0 | 4.1 |
| 10000 | 100 | 10198 | 418084 | api | merge_preview_contained | warm | 10 | 5.25 | — | 5.24 | — | 14.0 | 207.0 | 3040.0 | 0.0 | 0.0 | 3.4 | 1.0 | 4.2 |
| 10000 | 100 | 10198 | 418084 | api | merge_preview_divergent | warm | 10 | 95.79 | — | 95.84 | — | 20.0 | 818.0 | 6755.0 | 0.0 | 0.0 | 9.2 | 85.6 | 10.4 |
| 10000 | 100 | 10198 | 418084 | api | state_read | db-restart-first-ledger-op | 3 | 23.79 | — | 24.30 | — | 4.0 | 204.0 | 915.0 | 341.0 | 1.2 | 3.3 | — | — |
| 10000 | 100 | 10198 | 418084 | persisted | store_reconstruct | db-restart-first-ledger-op | 3 | 20.77 | — | 20.76 | — | 2.0 | 202.0 | 915.0 | 336.0 | 1.2 | 3.3 | — | — |
| 10000 | 500 | 10998 | 418884 | api | state_read | warm | 20 | 29.73 | 33.0 | 30.73 | 448.2 | 6.0 | 1004.0 | 6030.0 | 0.0 | 0.0 | 7.5 | 22.2 | 8.1 |
| 10000 | 500 | 10998 | 418884 | persisted | store_reconstruct | warm | 20 | 27.21 | 28.6 | 27.41 | — | 4.0 | 1002.0 | 6025.0 | 0.0 | 0.0 | 7.6 | 0.0 | 10.3 |
| 10000 | 500 | 10998 | 418884 | algorithm | fold_cpu | warm | 20 | 14.95 | 15.8 | 15.03 | — | — | — | — | — | — | — | — | — |
| 10000 | 500 | 10998 | 418884 | api | prepare | warm | 20 | 30.79 | 32.1 | 31.00 | — | 30.0 | 1023.0 | 6201.7 | 2.5 | 0.0 | 8.4 | 21.2 | 9.8 |
| 10000 | 500 | 10998 | 418884 | api | merge_preview_contained | warm | 10 | 21.41 | — | 21.53 | — | 16.0 | 1007.0 | 15003.1 | 0.0 | 0.0 | 17.1 | 2.9 | 18.4 |
| 10000 | 500 | 10998 | 418884 | api | merge_preview_divergent | warm | 10 | 140.43 | — | 143.41 | — | 28.0 | 4018.0 | 33151.0 | 0.0 | 0.0 | 41.5 | 101.7 | 43.5 |
| 10000 | 1000 | 11998 | 419883 | api | state_read | warm | 20 | 41.55 | 42.3 | 41.76 | 449.2 | 10.0 | 2004.0 | 12030.0 | 0.0 | 0.0 | 14.1 | 26.6 | 14.9 |
| 10000 | 1000 | 11998 | 419883 | persisted | store_reconstruct | warm | 20 | 39.88 | 51.7 | 41.68 | — | 8.0 | 2002.0 | 12025.0 | 0.0 | 0.0 | 15.5 | 0.0 | 16.0 |
| 10000 | 1000 | 11998 | 419883 | algorithm | fold_cpu | warm | 20 | 17.47 | 17.9 | 17.52 | — | — | — | — | — | — | — | — | — |
| 10000 | 1000 | 11998 | 419883 | api | prepare | warm | 20 | 43.41 | 48.9 | 43.84 | — | 34.0 | 2023.0 | 12203.2 | 1.6 | 0.0 | 15.5 | 27.4 | 17.1 |
| 10000 | 1000 | 11998 | 419883 | api | merge_preview_contained | warm | 10 | 41.41 | — | 41.36 | — | 20.0 | 2007.0 | 30028.0 | 0.0 | 0.0 | 34.8 | 5.3 | 36.0 |
| 10000 | 1000 | 11998 | 419883 | api | merge_preview_divergent | warm | 10 | 201.99 | — | 205.54 | — | 44.0 | 8018.0 | 66143.0 | 0.0 | 0.0 | 83.8 | 123.3 | 86.9 |
| 10000 | 1000 | 11998 | 419883 | api | state_read | db-restart-first-ledger-op | 3 | 50.87 | — | 50.82 | — | 10.0 | 2004.0 | 10810.0 | 1246.0 | 4.7 | 19.7 | — | — |
| 10000 | 1000 | 11998 | 419883 | persisted | store_reconstruct | db-restart-first-ledger-op | 3 | 49.37 | — | 49.89 | — | 8.0 | 2002.0 | 10810.0 | 1241.0 | 4.8 | 20.7 | — | — |
| 10000 | 2500 | 14998 | 421383 | api | state_read | warm | 20 | 98.68 | 118.0 | 100.09 | 450.7 | 22.0 | 5004.0 | 30030.0 | 0.0 | 0.0 | 48.8 | 51.8 | 52.9 |
| 10000 | 2500 | 14998 | 421383 | persisted | store_reconstruct | warm | 20 | 79.58 | 86.3 | 80.59 | — | 20.0 | 5002.0 | 30025.0 | 0.0 | 0.0 | 37.9 | 0.0 | 39.1 |
| 10000 | 2500 | 14998 | 421383 | algorithm | fold_cpu | warm | 20 | 25.99 | 27.5 | 26.13 | — | — | — | — | — | — | — | — | — |
| 10000 | 2500 | 14998 | 421383 | api | prepare | warm | 20 | 82.46 | 119.6 | 86.01 | — | 46.0 | 5023.0 | 30202.4 | 2.1 | 0.0 | 41.1 | 44.0 | 43.5 |
| 10000 | 2500 | 14998 | 421383 | api | merge_preview_contained | warm | 10 | 108.48 | — | 108.86 | — | 32.0 | 5007.0 | 74971.1 | 0.0 | 0.0 | 94.5 | 12.4 | 96.9 |
| 10000 | 2500 | 14998 | 421383 | api | merge_preview_divergent | warm | 10 | 391.24 | — | 393.33 | — | 92.0 | 20018.0 | 165119.0 | 0.0 | 0.0 | 210.2 | 185.9 | 216.3 |
| 10000 | 5000 | 19998 | 423883 | api | state_read | warm | 20 | 148.04 | 161.3 | 151.32 | 453.1 | 42.0 | 10004.0 | 60030.0 | 0.0 | 0.0 | 76.2 | 76.5 | 79.1 |
| 10000 | 5000 | 19998 | 423883 | persisted | store_reconstruct | warm | 20 | 156.85 | 180.1 | 159.52 | — | 40.0 | 10002.0 | 60025.0 | 0.0 | 0.0 | 83.9 | 0.0 | 86.8 |
| 10000 | 5000 | 19998 | 423883 | algorithm | fold_cpu | warm | 20 | 40.83 | 41.9 | 41.05 | — | — | — | — | — | — | — | — | — |
| 10000 | 5000 | 19998 | 423883 | api | prepare | warm | 20 | 153.10 | 182.6 | 160.43 | — | 66.0 | 10023.0 | 60201.9 | 1.8 | 0.0 | 81.7 | 79.3 | 85.1 |
| 10000 | 5000 | 19998 | 423883 | api | merge_preview_contained | warm | 10 | 239.85 | — | 241.25 | — | 52.0 | 10007.0 | 150178.0 | 0.0 | 0.0 | 210.1 | 30.0 | 214.6 |
| 10000 | 5000 | 19998 | 423883 | api | merge_preview_divergent | warm | 10 | 712.98 | — | 721.27 | — | 172.0 | 40018.0 | 330293.0 | 0.0 | 0.0 | 427.9 | 300.7 | 440.9 |
| 10000 | 5000 | 19998 | 423883 | api | state_read | db-restart-first-ledger-op | 3 | 164.60 | — | 173.75 | — | 42.0 | 10004.0 | 57773.0 | 2283.0 | 10.2 | 88.8 | — | — |
| 10000 | 5000 | 19998 | 423883 | persisted | store_reconstruct | db-restart-first-ledger-op | 3 | 171.11 | — | 165.99 | — | 40.0 | 10002.0 | 57773.0 | 2278.0 | 9.4 | 83.5 | — | — |
