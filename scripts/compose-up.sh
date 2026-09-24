#!/usr/bin/env bash
set -euo pipefail
command -v docker >/dev/null || { echo 'Docker is required' >&2; exit 69; }
docker compose up --build -d ledger postgres fuseki
