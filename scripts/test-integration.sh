#!/usr/bin/env bash
set -euo pipefail
command -v docker >/dev/null || { echo 'Docker integration unavailable: docker executable not found' >&2; exit 69; }
docker compose config --quiet
docker compose up --build -d --wait ledger postgres fuseki
trap 'docker compose down --remove-orphans' EXIT
curl --fail --retry 10 --retry-delay 2 http://localhost:8080/health
cargo test -p ledger-testkit --test walking_skeleton
