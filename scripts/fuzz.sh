#!/usr/bin/env bash
# Plan 0005 §5: run every cargo-fuzz target for a bounded time (default 60 s each) with the
# checked-in corpora; fail on any crash/assertion. Requires a nightly toolchain and cargo-fuzz
# (`rustup toolchain install nightly --profile minimal && cargo +nightly install cargo-fuzz`).
# Usage: scripts/fuzz.sh [seconds_per_target] [target ...]
set -euo pipefail
cd "$(dirname "$0")/.."
SECONDS_PER_TARGET=${1:-60}
shift || true
# Sanitizer: `none` by default. The AddressSanitizer runtime crashes at start-up (SIGSEGV
# before the first input) on the qualification host (Debian, kernel 5.10) and is not yet
# validated on the CI runner; the workspace forbids `unsafe`, so the targets rely on
# libFuzzer's coverage guidance plus debug assertions and the properties asserted in each
# target. Set FUZZ_SANITIZER=address on a host where ASan works.
SANITIZER=${FUZZ_SANITIZER:-none}
command -v cargo-fuzz >/dev/null || { echo "cargo-fuzz is required (cargo +nightly install cargo-fuzz)" >&2; exit 69; }
cargo +nightly --version >/dev/null 2>&1 || { echo "a nightly toolchain is required" >&2; exit 69; }
TARGETS=("$@")
if [ ${#TARGETS[@]} -eq 0 ]; then
  mapfile -t TARGETS < <(cd fuzz && cargo +nightly fuzz list)
fi
OUT="target/fuzz/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "${OUT}"
STATUS=0
for t in "${TARGETS[@]}"; do
  corpus="fuzz/corpus/${t}"
  [ -d "${corpus}" ] || { echo "FAIL: no corpus for ${t}" >&2; STATUS=1; continue; }
  seeds=$(find "${corpus}" -type f | wc -l)
  echo "== ${t}: ${seeds} seed(s), ${SECONDS_PER_TARGET} s"
  # -max_total_time bounds the run; -timeout flags a single slow input; artifacts (crashes)
  # land under fuzz/artifacts/<target>/ and fail the run through cargo-fuzz's exit code.
  if (cd fuzz && cargo +nightly fuzz run -s "${SANITIZER}" "${t}" "corpus/${t}" -- \
        -max_total_time="${SECONDS_PER_TARGET}" -timeout=10 -rss_limit_mb=2048 -print_final_stats=1) \
        >"${OUT}/${t}.log" 2>&1; then
    execs=$(grep -oE 'stat::number_of_executed_units: *[0-9]+' "${OUT}/${t}.log" | grep -oE '[0-9]+$' || echo '?')
    cov=$(grep -oE '^#[0-9]+.*cov: [0-9]+' "${OUT}/${t}.log" | tail -1 | grep -oE 'cov: [0-9]+' || echo 'cov: ?')
    echo "ok   ${t}: ${execs} executions, ${cov}, no crash" | tee -a "${OUT}/summary.txt"
  else
    echo "FAIL ${t}: see ${OUT}/${t}.log and fuzz/artifacts/${t}/" | tee -a "${OUT}/summary.txt"
    tail -30 "${OUT}/${t}.log" >&2
    STATUS=1
  fi
done
echo "sanitizer: ${SANITIZER}" | tee -a "${OUT}/summary.txt"
echo "summary: ${OUT}/summary.txt"
[ "${STATUS}" -eq 0 ] && echo "FUZZ OK" || echo "FUZZ FAILED"
exit "${STATUS}"
