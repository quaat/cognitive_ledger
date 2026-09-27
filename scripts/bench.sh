#!/usr/bin/env bash
# Plan 0005 §10: single-client performance baseline (prepare, accept, ref read, state read at
# chain depths 1/100/1,000/10,000) against the production-shaped stack. Output under
# target/bench/<run>/report.md, to be copied into docs/quality/performance-baselines.md.
# Usage: scripts/bench.sh [depths] [samples] [--constant-state]
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker curl cargo; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
DEPTHS=${1:-1,100,1000,10000}
SAMPLES=${2:-20}
EXTRA=${3:-}
OUT="target/bench/$(date -u +%Y%m%dT%H%M%SZ)"; mkdir -p "${OUT}"
COMPOSE=(docker compose -p ledger-qual-bench -f compose.yaml)
trap '"${COMPOSE[@]}" logs --no-color ledger >"${OUT}/server.log" 2>&1 || true; "${COMPOSE[@]}" down --remove-orphans --volumes' EXIT
"${COMPOSE[@]}" config --quiet
"${COMPOSE[@]}" up --build -d --wait ledger postgres
curl -fs http://127.0.0.1:8080/ready >/dev/null
git describe --always --dirty --long >"${OUT}/build-rev.txt" 2>/dev/null || git rev-parse HEAD >"${OUT}/build-rev.txt"
export LEDGER_STRESS_HS256_SECRET=development-only-hs256-secret-not-for-production-use
export LEDGER_STRESS_OWNER_DATABASE_URL='postgres://ledger:ledger-development-only@127.0.0.1:55432/ledger?sslmode=disable'
cargo build --locked --release -p ledger-stress
./target/release/ledger-stress bench --replica http://127.0.0.1:8080 --depths "${DEPTHS}" --samples "${SAMPLES}" ${EXTRA} --out "${OUT}" | tee "${OUT}/bench.log"
"${COMPOSE[@]}" run --rm migrate verify | tail -1
echo "report: ${OUT}/report.md"
