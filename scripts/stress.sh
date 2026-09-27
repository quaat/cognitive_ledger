#!/usr/bin/env bash
# Plan 0005 §2: ≥1,000 concurrent writers across two server replicas on real PostgreSQL.
# Brings up the production-shaped stack (owner migration → least-privilege runtime) plus a
# second replica in an isolated compose project, runs `ledger-stress`, then `ledger-admin
# verify`. Reports land under target/stress/<run>/ (report.md, report.json, replica
# environment, container logs). Usage: scripts/stress.sh [writers] [contended_seconds] [graphs] [commits_per_writer]
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker curl python3 cargo; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
WRITERS=${1:-1000}
CONTENDED_SECONDS=${2:-60}
GRAPHS=${3:-100}
COMMITS=${4:-3}
RUN=$(date -u +%Y%m%dT%H%M%SZ)
OUT="target/stress/${RUN}"
mkdir -p "${OUT}"
# Own compose project: never takes over or deletes the developer's default stack.
COMPOSE=(docker compose -p "ledger-qual-stress" -f compose.yaml -f compose.stress.yaml)
"${COMPOSE[@]}" config --quiet
collect_logs() { "${COMPOSE[@]}" logs --no-color ledger ledger-b postgres >"${OUT}/containers.log" 2>&1 || true; }
trap 'collect_logs; "${COMPOSE[@]}" down --remove-orphans --volumes' EXIT
"${COMPOSE[@]}" up --build -d --wait ledger ledger-b postgres
for port in 8080 8081; do
  curl -fs "http://127.0.0.1:${port}/ready" >/dev/null || { echo "FAIL: replica on ${port} is not ready" >&2; exit 1; }
done
# Record what was actually exercised (image digest, non-secret env, user).
for svc in ledger ledger-b; do
  cid=$("${COMPOSE[@]}" ps -q "${svc}")
  docker inspect --format '{{.Image}} {{.Config.User}}' "${cid}" >"${OUT}/${svc}.image"
  docker inspect --format '{{join .Config.Env "\n"}}' "${cid}" | grep -E '^LEDGER_(DB_|ADDR|IMMUTABLE|AUTH_MODE)' | grep -vE 'SECRET|PASSWORD|://[^/]*@' >"${OUT}/${svc}.env" || true
done
# The stress client keeps ~2,000 sockets open (two per writer under duplication).
ulimit -n 65536 2>/dev/null || true
export LEDGER_STRESS_HS256_SECRET=development-only-hs256-secret-not-for-production-use
export LEDGER_STRESS_OWNER_DATABASE_URL='postgres://ledger:ledger-development-only@localhost:55432/ledger?sslmode=disable'
cargo build --locked --release -p ledger-stress
set +e
./target/release/ledger-stress \
  --replicas http://127.0.0.1:8080,http://127.0.0.1:8081 \
  --writers "${WRITERS}" --contended-seconds "${CONTENDED_SECONDS}" --graphs "${GRAPHS}" \
  --commits-per-writer "${COMMITS}" --out "${OUT}" | tee "${OUT}/stress.log"
STRESS_EXIT=${PIPESTATUS[0]}
set -e
# Independent confirmation with the operator tool.
"${COMPOSE[@]}" run --rm migrate verify | tee "${OUT}/verify.log"
grep -q "VERIFY OK" "${OUT}/verify.log" || { echo "FAIL: ledger-admin verify reported violations" >&2; exit 1; }
echo "report: ${OUT}/report.md"
[ "${STRESS_EXIT}" -eq 0 ] || { echo "FAIL: ledger-stress exit ${STRESS_EXIT}" >&2; exit "${STRESS_EXIT}"; }
echo "STRESS GATE OK"
