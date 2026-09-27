# Quality gates

`./scripts/quality-gate.sh fast` is the local PR gate: formatting check, clippy with warnings denied, all workspace tests, documentation link/path checks, architecture dependency checks, and config sanity. `integration` additionally runs Docker-backed service tests. `full` adds Docker integration and the differential profile. Supply-chain auditing (`scripts/check-supply-chain.sh`, the `ci-security` workflow) is blocking; every finding is fixed or classified. Check modes never rewrite files. `scripts/stress.sh`, `scripts/fault.sh`, `scripts/backup-restore.sh`, `scripts/upgrade.sh` and `scripts/bench.sh` are qualification runs (Plan 0005), executed and recorded per release rather than per commit.

Claims must separate passed, failed, unavailable, and unimplemented work. Protocol changes require repeated golden tests, an ADR, and explicit vector review. CI workflows mirror these commands rather than hiding alternate behavior.
