#!/usr/bin/env bash
set -euo pipefail
mode=${1:-fast}
python3 -m json.tool .claude/settings.json >/dev/null
python3 scripts/check-doc-links.py
python3 scripts/check-architecture.py
if [[ "$mode" == sanity ]]; then
  cargo fmt --all -- --check
  exit 0
fi
cargo fmt --all -- --check
./scripts/lint.sh
./scripts/test.sh
