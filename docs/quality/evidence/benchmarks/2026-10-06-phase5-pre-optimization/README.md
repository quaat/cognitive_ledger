# Pre-optimization baseline of the Phase-5 implementation (Plan 0010)

These are the machine-readable artifacts of the runs that Plan 0010 records. They are
authoritative; the `report.md` files are generated from them.
- Code: `a68865e`. Production code is identical to `main` `c7a94d5`.
- Host: the qualification workstation (12 × i7-4930K, 15 GiB, Linux 5.10, Docker, PostgreSQL
  17.2 in Compose).
- Date: 2026-10-06.

| directory | command | content |
|---|---|---|
| `synthetic-ledger-ci/` | `scripts/benchmark.sh ci` | the PR profile: correctness oracle plus observations |
| `synthetic-ledger-local/` | `scripts/benchmark.sh local` | the deeper profile (parent-0 depth up to 615) |
| `linear-depth-constant-state/` | `scripts/bench.sh 1,100,1000 20 --constant-state` | the Plan 0005 depth control (one-quad state), re-run on Phase-5 code |

**Official status.** These are workstation observations, not official results under
[REPRODUCIBILITY.md](../../../../benchmarks/REPRODUCIBILITY.md). The `ci` and `local` runs
record `tracked_changes=1` and `untracked_files=2`:
- the tracked change is a documentation-only count correction (`docs/` is excluded from the
  image build context);
- the untracked files are a session note and the supplied requirements copy, not sources.

The official `ci` result is the clean-checkout hosted `ci-benchmark` run recorded in Plan
0010.

**Not comparable across hosts.** Timings compare only with the same profile on the same host
class. The historical Plan 0005 baseline stays in
[performance-baselines.md](../../../performance-baselines.md), unchanged.
