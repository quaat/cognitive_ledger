#!/usr/bin/env bash
set -euo pipefail
mode=${1:-fast}
python3 -m json.tool .claude/settings.json >/dev/null
python3 scripts/check-doc-links.py
python3 scripts/check-architecture.py
# Independent reference encoders must agree with the frozen golden vectors (commit v2 and
# request identity v1); the Rust side checks the same fixtures in cargo test.
python3 scripts/golden/commit_v2_reference.py check
python3 scripts/golden/request_v1_reference.py check
if [[ "$mode" == sanity ]]; then
  cargo fmt --all -- --check
  exit 0
fi
cargo fmt --all -- --check
./scripts/lint.sh
./scripts/test.sh
