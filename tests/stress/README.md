# stress tests

Implemented in Plan 0005 as `apps/ledger-stress` + `scripts/stress.sh` (≥1,000 concurrent
writers across two containerised replicas on real PostgreSQL, duplicated requests across
replicas, SQL invariants, `ledger-admin verify`). Evidence: `docs/quality/evidence/`.
This directory stays empty: the harness is a workspace binary so it is linted and unit-tested
by the fast gate, and it is never shipped in the runtime image.
