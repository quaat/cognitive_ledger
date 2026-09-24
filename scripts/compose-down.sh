#!/usr/bin/env bash
set -euo pipefail
command -v docker >/dev/null || { echo 'Docker is required' >&2; exit 69; }
docker compose --profile differential --profile fault down --remove-orphans
