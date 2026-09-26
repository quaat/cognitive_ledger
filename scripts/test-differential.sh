#!/usr/bin/env bash
# Differential seam against the pinned Fluree reference.
#
# Per the accepted decision, the Fluree image (BUSL-1.1) is pinned by digest but
# its container is NOT booted in CI pending license sign-off. This script therefore
# verifies the pin is present and validates the deterministic change model seam, but
# deliberately does NOT run the live comparison. Seam validation is not a
# differential pass — the live comparison is explicitly deferred, never faked.
set -euo pipefail

LOCK=test/reference-images.lock

digest=$(python3 -c 'import json;print(json.load(open("'"$LOCK"'")).get("digest") or "")')
[[ -n "$digest" ]] || { echo "Differential blocked: pin a verified Fluree image digest in $LOCK" >&2; exit 69; }
status=$(python3 -c 'import json;print(json.load(open("'"$LOCK"'")).get("status") or "")')

# Deterministic seam check (no live reference => seam-only, not a differential pass).
python3 tests/differential/compare_states.py tests/differential/scenario.json

echo "Fluree image pinned: ${digest}"
echo "LIVE differential comparison DEFERRED (container not started): ${status}"
echo "Enable by adding a reference-state capture step once BUSL-1.1 sign-off is recorded."
