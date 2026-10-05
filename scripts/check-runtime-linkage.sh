#!/usr/bin/env bash
# Mechanical proof of the .trivyignore CVE-2026-84782 premise (docs/quality/security.md,
# Container image): no shipped binary dynamically links OpenSSL. Copies the binaries out of
# the built runtime image, prints every NEEDED entry (audit evidence) and fails if any
# names libssl or libcrypto, if a binary is missing, or if its dynamic section cannot be
# read. Usage: scripts/check-runtime-linkage.sh <image> (e.g. cognitive_ledger-ledger:ci)
set -euo pipefail
IMAGE=${1:?usage: $0 <image>}
for tool in docker readelf; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
BINARIES=(ledger-server ledger-admin ledger-projector)
WORK=$(mktemp -d)
CID=""
cleanup() { [ -n "${CID}" ] && docker rm -f "${CID}" >/dev/null 2>&1; rm -rf "${WORK}"; }
trap cleanup EXIT
CID=$(docker create "${IMAGE}")
failed=0
for bin in "${BINARIES[@]}"; do
  docker cp "${CID}:/usr/local/bin/${bin}" "${WORK}/${bin}" >/dev/null
  dynamic=$(readelf -d "${WORK}/${bin}") || { echo "FAIL: cannot read the dynamic section of ${bin}" >&2; exit 1; }
  needed=$(printf '%s\n' "${dynamic}" | sed -n 's/.*(NEEDED).*\[\(.*\)\]$/\1/p')
  [ -n "${needed}" ] || { echo "FAIL: ${bin} has no NEEDED entries (unexpected for the glibc image)" >&2; exit 1; }
  echo "${bin}: NEEDED $(printf '%s\n' "${needed}" | tr '\n' ' ')"
  if printf '%s\n' "${needed}" | grep -Ei 'libssl|libcrypto' >/dev/null; then
    echo "FAIL: ${bin} links OpenSSL; the .trivyignore exception for CVE-2026-84782 no longer holds" >&2
    failed=1
  fi
done
[ "${failed}" = 0 ] || exit 1
echo "RUNTIME LINKAGE OK: no shipped binary links libssl/libcrypto (${IMAGE})"
