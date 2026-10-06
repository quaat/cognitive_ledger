# Performance baseline `b6ac551ad` — complete

Single client, one graph, linear history of 1000 commits (constant one-quad state: each commit adds one quad and deletes the previous); up to 20 samples per cell: prepare/accept measured on the last commits before each depth, ref and state reads at exactly that depth; 103.8 s to build. Hardware: 12 × Intel(R) Core(TM) i7-4930K CPU @ 3.40GHz, 15 GiB RAM, kernel 5.10.0-44-amd64. PostgreSQL: PostgreSQL 17.2 (Debian 17.2-1.pgdg120+1) on x86_64-pc-linux-gnu, compiled by gcc (Debian 12.2.0-14) 12.2.0, 64-bit. Replica: http://127.0.0.1:8080.

| depth | commit samples | quads in state | prepare p50 / p95 ms | accept p50 / p95 ms | ref read p50 / p95 ms | state read p50 / p95 ms |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 1 | 10.8 / 10.8 | 16.9 / 16.9 | 0.6 / 0.9 | 0.9 / 1.4 |
| 100 | 20 | 1 | 18.9 / 22.9 | 3.6 / 4.3 | 0.7 / 0.9 | 20.4 / 23.3 |
| 1000 | 20 | 1 | 179.4 / 189.1 | 4.1 / 5.1 | 0.8 / 0.9 | 187.8 / 198.3 |
