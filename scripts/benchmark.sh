#!/usr/bin/env bash
# Plan 0010 (Phase 6A): deterministic benchmark profiles (`ledger-bench`) against the
# production-shaped stack (owner migration, least-privilege runtime identity, distroless
# image), through the public API. Each dataset is validated against its committed manifest
# before the stack starts; correctness assertions fail the run, timings are observations.
# After the run, `ledger-admin verify` must report VERIFY OK.
#
# Usage: scripts/benchmark.sh [profile]   (profiles: ci (default), local)
# Output: target/benchmark/<UTC>-<profile>/{result.json,report.md,phases.txt,verify.log,...}
# Network: image build and crate downloads happen before the run; the run itself talks only
# to loopback (the ledger and PostgreSQL).
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker curl cargo git; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
PROFILE=${1:-ci}
OUT="target/benchmark/$(date -u +%Y%m%dT%H%M%SZ)-${PROFILE}"
mkdir -p "${OUT}"
COMPOSE=(docker compose -p ledger-qual-benchmark -f compose.yaml)
teardown() {
  "${COMPOSE[@]}" logs --no-color ledger >"${OUT}/server.log" 2>&1 || true
  "${COMPOSE[@]}" down --remove-orphans --volumes >/dev/null 2>&1 || true
}
trap teardown EXIT
phase() { echo "$1 $(( $(date +%s) - T0 ))s" | tee -a "${OUT}/phases.txt"; T0=$(date +%s); }
T0=$(date +%s)

cargo build --locked --release -p ledger-bench
phase harness-build
# Fail fast on an invalid dataset (no stack needed).
./target/release/ledger-bench validate --profile "${PROFILE}"
phase dataset-validate

"${COMPOSE[@]}" config --quiet
"${COMPOSE[@]}" up --build -d --wait ledger postgres
curl -fs http://127.0.0.1:8080/ready >/dev/null
phase stack-build-and-start

REV=$(git rev-parse HEAD)
DIRTY=$(git status --porcelain --untracked-files=no | wc -l | tr -d ' ')
IMAGE=$(docker inspect --format '{{.Image}}' "$("${COMPOSE[@]}" ps -q ledger)")
# Container memory: cgroup v2 `memory.peak` when the kernel has it (>= 5.19, e.g. hosted
# runners); otherwise a 0.5 s sampler of `memory.current` records the observed maximum.
cgroup_dir() {
  local id
  id=$(docker inspect --format '{{.Id}}' "$("${COMPOSE[@]}" ps -q "$1")" 2>/dev/null) || return 1
  for d in "/sys/fs/cgroup/system.slice/docker-${id}.scope" "/sys/fs/cgroup/docker/${id}"; do
    [ -r "$d/memory.current" ] && { echo "$d"; return 0; }
  done
  return 1
}
sample_memory() {
  local dir=$1 file=$2 max=0 cur
  while :; do
    cur=$(cat "$dir/memory.current" 2>/dev/null) || break
    [ "$cur" -gt "$max" ] && { max=$cur; echo "$max" >"$file"; }
    sleep 0.5
  done
}
SAMPLERS=()
for svc in ledger postgres; do
  if dir=$(cgroup_dir "$svc"); then
    sample_memory "$dir" "${OUT}/${svc}-memory-sampled-max" &
    SAMPLERS+=("$!")
  fi
done
export LEDGER_BENCH_HS256_SECRET=development-only-hs256-secret-not-for-production-use
export LEDGER_BENCH_OWNER_DATABASE_URL='postgres://ledger:ledger-development-only@127.0.0.1:55432/ledger?sslmode=disable'
set +e
./target/release/ledger-bench run --profile "${PROFILE}" --replica http://127.0.0.1:8080 --out "${OUT}" \
  --meta "build_rev=${REV}" --meta "tracked_changes=${DIRTY}" --meta "rustc=$(rustc --version)" \
  --meta "server_image=${IMAGE}" --meta "compose_project=ledger-qual-benchmark" 2>&1 | tee "${OUT}/run.log"
STATUS=${PIPESTATUS[0]}
set -e
for pid in "${SAMPLERS[@]}"; do kill "$pid" 2>/dev/null || true; done
phase benchmark-run

# Peak memory (page cache included) of the server and PostgreSQL containers: cgroup v2
# `memory.peak` (container lifetime, includes the image start), else the sampled maximum of
# `memory.current` during the run, else `unavailable`.
peak() {
  local dir
  if dir=$(cgroup_dir "$1"); then
    if [ -r "$dir/memory.peak" ]; then echo "$(( $(cat "$dir/memory.peak") / 1048576 )) MiB (cgroup memory.peak)"; return; fi
  fi
  if [ -s "${OUT}/$1-memory-sampled-max" ]; then
    echo "$(( $(cat "${OUT}/$1-memory-sampled-max") / 1048576 )) MiB (memory.current sampled every 0.5 s)"; return
  fi
  echo unavailable
}
if [ -f "${OUT}/result.json" ]; then
  ./target/release/ledger-bench annotate "${OUT}/result.json" \
    "server_peak_memory=$(peak ledger)" "postgres_peak_memory=$(peak postgres)" \
    "phases=$(tr '\n' ';' <"${OUT}/phases.txt")"
fi

# The production verifier over everything the run wrote.
"${COMPOSE[@]}" run --rm migrate verify >"${OUT}/verify.log" 2>&1 || true
tail -1 "${OUT}/verify.log"
grep -q "VERIFY OK" "${OUT}/verify.log" || { echo "FAIL: ledger-admin verify did not report VERIFY OK" >&2; STATUS=1; }
phase verify
echo "report: ${OUT}/report.md"
exit "${STATUS}"
