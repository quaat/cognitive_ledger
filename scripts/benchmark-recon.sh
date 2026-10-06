#!/usr/bin/env bash
# Plan 0011: reconstruction characterization (`ledger-bench recon`), outside PR CI.
# Production-shaped stack (owner migration, runtime identity, distroless image) with the
# benchmark-only PostgreSQL instrumentation override (benchmark/compose.instrumented.yaml):
# pg_stat_statements in schema bench_stats, track_io_timing. Builds constant-state histories
# (1 / 1,000 / 10,000 quads) to depth 5,000 through the public API, then measures API state
# reads, direct store reconstruction, the fold's CPU work, prepare and merge previews per
# depth, warm and database-restart cold, with PostgreSQL statement and cgroup CPU/memory
# deltas. Ends with `ledger-admin verify`. Takes about an hour or more.
# Usage: scripts/benchmark-recon.sh [extra ledger-bench recon flags...]
# Output: target/benchmark/<UTC>-recon/{recon.json,recon.md,verify.log,...}
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker curl cargo git; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
OUT="target/benchmark/$(date -u +%Y%m%dT%H%M%SZ)-recon"
mkdir -p "${OUT}"
COMPOSE=(docker compose -p ledger-qual-recon -f compose.yaml -f benchmark/compose.instrumented.yaml)
teardown() {
  "${COMPOSE[@]}" logs --no-color ledger >"${OUT}/server.log" 2>&1 || true
  "${COMPOSE[@]}" down --remove-orphans --volumes >/dev/null 2>&1 || true
}
trap teardown EXIT
cargo build --locked --release -p ledger-bench
"${COMPOSE[@]}" config --quiet
"${COMPOSE[@]}" up --build -d --wait ledger postgres
curl -fs http://127.0.0.1:8080/ready >/dev/null
"${COMPOSE[@]}" exec -T postgres psql -v ON_ERROR_STOP=1 -U ledger -d ledger -c \
  "CREATE SCHEMA IF NOT EXISTS bench_stats; CREATE EXTENSION IF NOT EXISTS pg_stat_statements SCHEMA bench_stats;" >/dev/null
cgroup_dir() {
  local id
  id=$(docker inspect --format '{{.Id}}' "$("${COMPOSE[@]}" ps -q "$1")")
  for d in "/sys/fs/cgroup/system.slice/docker-${id}.scope" "/sys/fs/cgroup/docker/${id}"; do
    [ -r "$d/cpu.stat" ] && { echo "$d"; return 0; }
  done
  echo ""
}
SERVER_CG=$(cgroup_dir ledger); PG_CG=$(cgroup_dir postgres)
PG_CONTAINER=$("${COMPOSE[@]}" ps -q postgres)
export LEDGER_BENCH_HS256_SECRET=development-only-hs256-secret-not-for-production-use
export LEDGER_BENCH_OWNER_DATABASE_URL='postgres://ledger:ledger-development-only@127.0.0.1:55432/ledger?sslmode=disable'
set +e
./target/release/ledger-bench recon --replica http://127.0.0.1:8080 --out "${OUT}" \
  --restart-cmd "docker restart ${PG_CONTAINER} >/dev/null" \
  ${SERVER_CG:+--server-cgroup "${SERVER_CG}"} ${PG_CG:+--postgres-cgroup "${PG_CG}"} \
  --meta "build_rev=$(git rev-parse HEAD)" --meta "tracked_changes=$(git status --porcelain --untracked-files=no | wc -l | tr -d ' ')" \
  --meta "postgres=$("${COMPOSE[@]}" exec -T postgres psql -U ledger -d ledger -tAc 'SHOW server_version')" \
  --meta "postgres_config=benchmark-only instrumentation override (pg_stat_statements, track_io_timing=on); differs from the production-shaped compose.yaml" \
  --meta "server_image=$(docker inspect --format '{{.Image}}' "$("${COMPOSE[@]}" ps -q ledger)")" \
  --meta "acceptance_mode=unvalidated-development" "$@" 2>&1 | tee "${OUT}/run.log"
STATUS=${PIPESTATUS[0]}
set +e
"${COMPOSE[@]}" run --rm migrate verify >"${OUT}/verify.log" 2>&1
VERIFY=$?
set -e
tail -1 "${OUT}/verify.log"
if [ "${VERIFY}" != 0 ] || ! grep -q "VERIFY OK" "${OUT}/verify.log"; then echo "FAIL: verify" >&2; STATUS=1; fi
echo "report: ${OUT}/recon.md"
exit "${STATUS}"
