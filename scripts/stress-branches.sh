#!/usr/bin/env bash
# Plan 0008: 100-branch stress across two server replicas on real PostgreSQL.
# Brings up the production-shaped stack (owner migration → least-privilege runtime) plus a
# second replica in an isolated compose project, runs `ledger-stress branches` (branch
# creation from head and history, concurrent work-branch cycles while `main` moves,
# delete-vs-accept races, restores, reads; invariants checked under the owner identity),
# then `ledger-admin verify`. Reports land under target/stress-branches/<run>/.
# Usage: scripts/stress-branches.sh [branches] [commits_per_branch] [main_writers]
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker curl python3 cargo; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
BRANCHES=${1:-100}
COMMITS=${2:-3}
MAIN_WRITERS=${3:-4}
RUN=$(date -u +%Y%m%dT%H%M%SZ)
OUT="target/stress-branches/${RUN}"
mkdir -p "${OUT}"
# Own compose project: never takes over or deletes the developer's default stack.
COMPOSE=(docker compose -p "ledger-qual-branch-stress" -f compose.yaml -f compose.stress.yaml)
"${COMPOSE[@]}" config --quiet
collect_logs() { "${COMPOSE[@]}" logs --no-color ledger ledger-b postgres >"${OUT}/containers.log" 2>&1 || true; }
trap 'collect_logs; "${COMPOSE[@]}" down --remove-orphans --volumes' EXIT
"${COMPOSE[@]}" up --build -d --wait ledger ledger-b postgres
for port in 8080 8081; do
  curl -fs "http://127.0.0.1:${port}/ready" >/dev/null || { echo "FAIL: replica on ${port} is not ready" >&2; exit 1; }
done
for svc in ledger ledger-b; do
  cid=$("${COMPOSE[@]}" ps -q "${svc}")
  docker inspect --format '{{.Image}} {{.Config.User}}' "${cid}" >"${OUT}/${svc}.image"
done
export LEDGER_STRESS_HS256_SECRET=development-only-hs256-secret-not-for-production-use
export LEDGER_STRESS_OWNER_DATABASE_URL='postgres://ledger:ledger-development-only@localhost:55432/ledger?sslmode=disable'
cargo build --locked --release -p ledger-stress
set +e
./target/release/ledger-stress branches \
  --replicas http://127.0.0.1:8080,http://127.0.0.1:8081 \
  --branches "${BRANCHES}" --commits-per-branch "${COMMITS}" --main-writers "${MAIN_WRITERS}" \
  --out "${OUT}" | tee "${OUT}/stress.log"
STRESS_EXIT=${PIPESTATUS[0]}
set -e
"${COMPOSE[@]}" run --rm migrate verify | tee "${OUT}/verify.log"
grep -q "VERIFY OK" "${OUT}/verify.log" || { echo "FAIL: ledger-admin verify reported violations" >&2; exit 1; }
echo "report: ${OUT}/report.md"
[ "${STRESS_EXIT}" -eq 0 ] || { echo "FAIL: ledger-stress branches exit ${STRESS_EXIT}" >&2; exit "${STRESS_EXIT}"; }
echo "BRANCH STRESS GATE OK"
