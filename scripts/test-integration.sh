#!/usr/bin/env bash
# Genuine Docker-backed integration evidence for Plans 0002 and 0004 (P1.2):
#   1. The PostgreSQL ref CAS is lost-update-safe under two independent connections.
#   1b. Two ledger replicas sharing one PostgreSQL immutable store agree and reconstruct;
#       the commit index re-derives from bytes; the v1 graph-binding policy holds.
#   2. The HTTP server commits/reconstructs over real HTTP and its PostgreSQL-backed
#      ref head plus PostgreSQL-backed immutable objects survive a container restart.
# No step is a local-filesystem stand-in for a container, and no skipped external
# test is reported as a pass.
set -euo pipefail

command -v docker >/dev/null || { echo 'Docker integration unavailable: docker executable not found' >&2; exit 69; }
command -v curl >/dev/null || { echo 'curl is required for the HTTP scenario' >&2; exit 69; }

PG_HOST_PORT=55432
BASE=http://localhost:8080

docker compose config --quiet
docker compose up --build -d --wait ledger postgres
trap 'docker compose down --remove-orphans --volumes' EXIT

# --- 1. Real-PostgreSQL two-connection CAS race -----------------------------------
export LEDGER_TEST_DATABASE_URL="postgres://ledger:ledger-development-only@localhost:${PG_HOST_PORT}/ledger?sslmode=disable"
cargo test -p ledger-store --features postgres --test pg_cas_race -- --ignored --nocapture

# --- 1b. Shared PostgreSQL immutable store (ADR-0012): two replicas, truthful publication,
#         incompatible-binding race, same-graph ancestry, verified commit index -----------
cargo test -p ledger-store --features postgres --test pg_immutable_store -- --ignored --nocapture

# --- 1c. Graph authority schema (ADR-0010, migration 0004): constraints, FK integrity,
#         clean-install vs upgrade convergence, unknown-graph refusal ---------------------
cargo test -p ledger-store --features postgres --test pg_graphs_migration -- --ignored --nocapture

# --- 1d. Administrative filesystem -> PostgreSQL migration (ADR-0012) -------------------
cargo test -p ledger-store --features postgres --test pg_fs_migration -- --ignored --nocapture

# --- 1e. Atomic workflow persistence (ADR-0013, P1.3): prepare/accept/reject in one
#         transaction, idempotency under concurrency, lineage, fault injection, DB invariants
cargo test -p ledger-store --features postgres --test pg_workflow -- --ignored --nocapture

# --- 2. HTTP commit / state / restart durability ----------------------------------
curl --fail --silent --retry 10 --retry-delay 2 "${BASE}/health" >/dev/null

json_field() { python3 -c 'import sys,json; print(json.load(sys.stdin)["'"$1"'"])'; }

# Build the commit body with Python so RDF literals (which contain double quotes)
# are always escaped into valid JSON, then POST it. $1=expected head ("null" or a
# commit id), $2=N-Quad, $3=message.
commit() {
  python3 -c 'import json,sys; exp=None if sys.argv[1]=="null" else sys.argv[1]; print(json.dumps({"expected_head":exp,"operations":[{"op":"add","quad":sys.argv[2]}],"author":"urn:agent:integration","message":sys.argv[3],"event_time":"2026-09-25T00:00:00Z"}))' "$1" "$2" "$3" \
    | curl --fail --silent -X POST "${BASE}/v1/commits" -H 'content-type: application/json' --data @- \
    | json_field id
}

C1=$(commit null '<urn:material:a> <urn:temperature> "80" .' 'genesis')
echo "genesis commit: ${C1}"

HEAD=$(curl --fail --silent "${BASE}/v1/refs/main" | json_field head)
[ "${HEAD}" = "${C1}" ] || { echo "FAIL: head ${HEAD} != genesis ${C1}" >&2; exit 1; }

C2=$(commit "${C1}" '<urn:material:a> <urn:humidity> "40" .' 'second')
echo "second commit: ${C2}"

# Restart only the ledger process; PostgreSQL (ref head + immutable objects) must carry
# state across the restart. The ledger container keeps nothing it needs locally.
docker compose restart ledger
curl --fail --silent --retry 20 --retry-delay 2 "${BASE}/health" >/dev/null

HEAD_AFTER=$(curl --fail --silent "${BASE}/v1/refs/main" | json_field head)
[ "${HEAD_AFTER}" = "${C2}" ] || { echo "FAIL: head after restart ${HEAD_AFTER} != ${C2}" >&2; exit 1; }
echo "ref head survived restart: ${HEAD_AFTER}"

STATE=$(curl --fail --silent "${BASE}/v1/states/${C2}")
echo "${STATE}" | grep -q 'urn:temperature' || { echo "FAIL: reconstructed state missing temperature quad: ${STATE}" >&2; exit 1; }
echo "${STATE}" | grep -q 'urn:humidity' || { echo "FAIL: reconstructed state missing humidity quad: ${STATE}" >&2; exit 1; }
echo "state reconstructed across restart from the shared PostgreSQL immutable store"

# Prove the default backend really is PostgreSQL: the commits are rows in the shared
# store, and the ledger container holds no immutable objects of its own.
IN_PG=$(docker compose exec -T postgres psql -U ledger -d ledger -tAc "select count(*) from immutable_objects where id in ('${C1}','${C2}')")
[ "${IN_PG}" = "2" ] || { echo "FAIL: commits not in PostgreSQL immutable_objects (count=${IN_PG})" >&2; exit 1; }
INDEXED=$(docker compose exec -T postgres psql -U ledger -d ledger -tAc "select count(*) from commit_index where id in ('${C1}','${C2}') and graph_id = 'default'")
[ "${INDEXED}" = "2" ] || { echo "FAIL: commits not indexed under the bootstrap graph (count=${INDEXED})" >&2; exit 1; }
LOCAL_OBJECTS=$(docker compose exec -T ledger sh -c 'find /data -type f 2>/dev/null | wc -l')
[ "${LOCAL_OBJECTS}" = "0" ] || { echo "FAIL: ledger container holds ${LOCAL_OBJECTS} node-local object file(s)" >&2; exit 1; }
echo "shared backend confirmed: 2 commits in PostgreSQL, 2 indexed under 'default', 0 node-local object files"

echo "INTEGRATION OK"
