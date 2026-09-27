#!/usr/bin/env bash
# Plan 0005 §3: real process/container kill fault injection during sustained writes.
# Two server replicas on real PostgreSQL carry a sustained prepare/accept load
# (`ledger-stress fault`); this harness SIGKILLs alternating replicas SERVER_KILLS times (and
# on, up to MAX_SERVER_KILLS, while fewer than MIN_DURABLE accepts were observed as committed
# before their response was lost), then SIGKILLs the PostgreSQL container PG_KILLS times. Recovery
# is detected by readiness and progress observation (no timing assumptions);
# `ledger-admin verify` runs after every recovery; the load generator replays every in-doubt
# response verbatim in each quiet window and classifies it against the database.
# Usage: scripts/fault.sh [server_kills] [postgres_kills] [writers] [graphs] [min_durable_accepts] [max_server_kills]
# With min_durable_accepts > 0 the server kills continue (up to max_server_kills) until that
# many accepts were observed as committed before their response was lost; by default the
# count is only reported (the window is sub-millisecond; the deterministic proof is the
# FailPoint unit test).
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker curl python3 cargo; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
SERVER_KILLS=${1:-8}
PG_KILLS=${2:-3}
WRITERS=${3:-200}
GRAPHS=${4:-20}
MIN_DURABLE=${5:-0}
MAX_SERVER_KILLS=${6:-30}
RUN=$(date -u +%Y%m%dT%H%M%SZ)
OUT="target/fault/${RUN}"
mkdir -p "${OUT}"
STOP="${OUT}/stop"; PROGRESS="${OUT}/progress.json"; WINDOW="${OUT}/window"; KILL_LOG="${OUT}/kills.log"
echo steady >"${WINDOW}"; : >"${KILL_LOG}"
COMPOSE=(docker compose -p "ledger-qual-fault" -f compose.yaml -f compose.stress.yaml)
"${COMPOSE[@]}" config --quiet
collect_logs() { "${COMPOSE[@]}" logs --no-color ledger ledger-b postgres >"${OUT}/containers.log" 2>&1 || true; }
LOAD_PID=""
cleanup() { if [ -n "${LOAD_PID}" ]; then touch "${STOP}"; wait "${LOAD_PID}" 2>/dev/null || true; fi; collect_logs; "${COMPOSE[@]}" down --remove-orphans --volumes; }
trap cleanup EXIT
"${COMPOSE[@]}" up --build -d --wait ledger ledger-b postgres
ready() { curl -fs -o /dev/null "http://127.0.0.1:$1/ready"; }
wait_ready() { # $1=port $2=max seconds
  local i=0
  until ready "$1"; do i=$((i+1)); [ "$i" -lt $(( $2 * 5 )) ] || { echo "FAIL: replica on port $1 not ready after $2 s" >&2; return 1; }; sleep 0.2; done
}
progress() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "${PROGRESS}" "$1" 2>/dev/null || echo 0; }
wait_progress() { # $1=minimum new landed commits from now, $2=max seconds
  local start; start=$(progress landed); local target=$(( start + $1 )); local i=0
  until [ "$(progress landed)" -ge "${target}" ]; do i=$((i+1)); [ "$i" -lt $(( $2 * 5 )) ] || { echo "FAIL: load did not progress (${start} -> $(progress landed) < ${target}) within $2 s" >&2; return 1; }; sleep 0.2; done
}
wait_replayed() { # wait until the generator has replayed everything collected so far ($1 max seconds)
  local i=0
  until [ "$(progress replayed)" -ge "$(progress in_doubt)" ]; do i=$((i+1)); [ "$i" -lt $(( $1 * 5 )) ] || { echo "FAIL: replays did not complete within $1 s" >&2; return 1; }; sleep 0.2; done
}
verify_clean() { "${COMPOSE[@]}" run --rm migrate verify >"${OUT}/verify-$1.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/verify-$1.log" || { echo "FAIL: verify after $1 reported violations" >&2; cat "${OUT}/verify-$1.log" >&2; return 1; }; }
started_at() { docker inspect --format '{{.State.StartedAt}}' "$("${COMPOSE[@]}" ps -q "$1")"; }
elapsed() { python3 -c 'import sys; print(f"{float(sys.argv[2]) - float(sys.argv[1]):.1f}")' "$1" "$2"; }
wait_ready 8080 30; wait_ready 8081 30
ulimit -n 65536 2>/dev/null || true
export LEDGER_STRESS_HS256_SECRET=development-only-hs256-secret-not-for-production-use
export LEDGER_STRESS_OWNER_DATABASE_URL='postgres://ledger:ledger-development-only@localhost:55432/ledger?sslmode=disable'
cargo build --locked --release -p ledger-stress
./target/release/ledger-stress fault \
  --replicas http://127.0.0.1:8080,http://127.0.0.1:8081 \
  --writers "${WRITERS}" --graphs "${GRAPHS}" --min-durable-accepts "${MIN_DURABLE}" --out "${OUT}" \
  --stop-file "${STOP}" --progress-file "${PROGRESS}" --window-file "${WINDOW}" >"${OUT}/stress.log" 2>&1 &
LOAD_PID=$!
wait_progress 50 120
echo "sustained load established: $(progress landed) commits, $(progress requests) requests"
# --- Server kills: alternate replicas until enough durable-before-crash accepts were seen.
i=0
while [ "$i" -lt "${SERVER_KILLS}" ] || { [ "$(progress durable_accepts)" -lt "${MIN_DURABLE}" ] && [ "$i" -lt "${MAX_SERVER_KILLS}" ]; }; do
  i=$((i+1))
  if [ $((i % 2)) -eq 1 ]; then svc=ledger; port=8080; else svc=ledger-b; port=8081; fi
  cid=$("${COMPOSE[@]}" ps -q "${svc}")
  echo "server-kill-${i} ${svc}" >"${WINDOW}"
  before=$(progress landed)
  t0=$(date +%s.%N)
  docker kill -s KILL "${cid}" >/dev/null
  docker wait "${cid}" >/dev/null   # the process is gone before we restart it
  "${COMPOSE[@]}" up -d --no-deps --no-build "${svc}" >/dev/null 2>&1
  wait_ready "${port}" 60
  t1=$(date +%s.%N)
  echo "steady" >"${WINDOW}"
  verify_clean "server-kill-${i}"
  wait_replayed 180
  wait_progress 30 120
  printf 'server-kill-%s %s: SIGKILL at %s commits, ready again after %s s, verify OK, load progressed to %s, in-doubt %s, replayed %s, durable-before-crash accepts so far %s, inconsistent %s\n' \
    "$i" "${svc}" "${before}" "$(elapsed "${t0}" "${t1}")" "$(progress landed)" "$(progress in_doubt)" "$(progress replayed)" "$(progress durable_accepts)" "$(progress inconsistent)" | tee -a "${KILL_LOG}"
done
echo "server kills: ${i}; durable-before-crash accepts observed: $(progress durable_accepts) (minimum demanded ${MIN_DURABLE})" | tee -a "${KILL_LOG}"
# --- PostgreSQL kills: the replicas must reconnect without being restarted.
PG_CID=$("${COMPOSE[@]}" ps -q postgres)
for j in $(seq 1 "${PG_KILLS}"); do
  a0=$(started_at ledger); b0=$(started_at ledger-b)
  echo "postgres-kill-${j}" >"${WINDOW}"
  before=$(progress landed)
  t0=$(date +%s.%N)
  docker kill -s KILL "${PG_CID}" >/dev/null
  docker wait "${PG_CID}" >/dev/null
  "${COMPOSE[@]}" up -d --no-deps --no-build postgres >/dev/null 2>&1
  wait_ready 8080 90; wait_ready 8081 90
  t1=$(date +%s.%N)
  [ "$(started_at ledger)" = "${a0}" ] && [ "$(started_at ledger-b)" = "${b0}" ] || { echo "FAIL: a replica was restarted during the PostgreSQL kill" >&2; exit 1; }
  echo "steady" >"${WINDOW}"
  verify_clean "postgres-kill-${j}"
  wait_replayed 180
  wait_progress 30 180
  printf 'postgres-kill-%s: SIGKILL at %s commits, both replicas ready again after %s s without restart (crash recovery), verify OK, load progressed to %s, in-doubt %s, inconsistent %s\n' \
    "$j" "${before}" "$(elapsed "${t0}" "${t1}")" "$(progress landed)" "$(progress in_doubt)" "$(progress inconsistent)" | tee -a "${KILL_LOG}"
done
# Stop the load; the generator replays what is left and writes its report.
touch "${STOP}"
set +e
wait "${LOAD_PID}"; LOAD_EXIT=$?
set -e
LOAD_PID=""
cat "${OUT}/report.md"
verify_clean final
echo "report: ${OUT}/report.md (kills: ${KILL_LOG})"
[ "${LOAD_EXIT}" -eq 0 ] || { echo "FAIL: ledger-stress fault exit ${LOAD_EXIT}" >&2; exit "${LOAD_EXIT}"; }
echo "FAULT GATE OK"
