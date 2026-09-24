# Quality gates

`./scripts/quality-gate.sh fast` is the local PR gate: formatting check, clippy with warnings denied, all workspace tests, documentation link/path checks, architecture dependency checks, and config sanity. `integration` additionally runs Docker-backed service tests. `full` adds Docker integration and the differential profile. Supply-chain auditing is a separate, non-blocking classified CI job until its policy is matured. Check modes never rewrite files.

Claims must separate passed, failed, unavailable, and unimplemented work. Protocol changes require repeated golden tests, an ADR, and explicit vector review. CI workflows mirror these commands rather than hiding alternate behavior.
