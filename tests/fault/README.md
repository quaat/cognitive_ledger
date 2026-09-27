# fault tests

Implemented in Plan 0005 as `ledger-stress fault` + `scripts/fault.sh` (SIGKILL of server
replicas and of PostgreSQL under sustained writes, verify after every recovery, verbatim
replay and database classification of every in-doubt response). Deterministic in-process
fault points live in `crates/ledger-store/tests/pg_workflow.rs` (`FailPoint`). Evidence:
`docs/quality/evidence/`.
