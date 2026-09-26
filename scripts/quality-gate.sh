#!/usr/bin/env bash
set -euo pipefail
mode=${1:-fast}
case "$mode" in
 fast) ./scripts/check-fast.sh ;;
 integration) ./scripts/check-fast.sh; ./scripts/test-integration.sh ;;
 full) ./scripts/check-fast.sh; ./scripts/test-integration.sh; ./scripts/test-differential.sh ;;
 *) echo "usage: $0 {fast|integration|full}" >&2; exit 2;;
esac
