#!/usr/bin/env bash
set -euo pipefail
command -v docker >/dev/null || { echo 'Differential test unavailable: docker executable not found' >&2; exit 69; }
image=$(jq -r '.image // empty' test/reference-images.lock)
[[ -n "$image" ]] || { echo 'Differential test blocked: pin a verified Fluree image digest in test/reference-images.lock' >&2; exit 69; }
FLUREE_IMAGE="$image" docker compose --profile differential up -d --wait fluree-reference
trap 'FLUREE_IMAGE="$image" docker compose --profile differential down --remove-orphans' EXIT
python3 tests/differential/compare_states.py tests/differential/scenario.json
