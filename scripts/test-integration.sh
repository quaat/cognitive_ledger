#!/usr/bin/env bash
# Genuine Docker-backed integration evidence for Plans 0002 and 0004 (P1.2–P1.4):
#   1.  Real-PostgreSQL store suites: CAS race, shared immutable store, graph authority
#       and migration 0007 upgrade, fs→pg migration, atomic workflow (idempotency by the
#       complete actor, lineage, fault injection, DB invariants).
#   1f. Authenticated HTTP API over real PostgreSQL: auth/authz, tenant isolation,
#       lost-response replay, fail-closed acceptance, limits, safe errors.
#   2.  The containerised server: an operator provisions a graph with ledger-admin, a
#       client authenticates, prepares and accepts CommitV2 candidates through the public
#       API (CI no-validation mode), the ledger container restarts, and authenticated reads
#       see the same head and state from PostgreSQL. The commit index holds version-2
#       commits only and every ref move has a ref event (the raw ref path is unused).
# No step is a local-filesystem stand-in for a container, and no skipped external test is
# reported as a pass. No end-user step issues SQL; SQL appears only in the final
# invariant verification.
set -euo pipefail

command -v docker >/dev/null || { echo 'Docker integration unavailable: docker executable not found' >&2; exit 69; }
command -v curl >/dev/null || { echo 'curl is required for the HTTP scenario' >&2; exit 69; }
command -v python3 >/dev/null || { echo 'python3 is required for JSON handling and token minting' >&2; exit 69; }

PG_HOST_PORT=55432
BASE=http://localhost:8080
# Must match compose.yaml (development authenticator; never a production secret).
AUTH_ISSUER=https://dev-issuer.example/
AUTH_AUDIENCE=api://sculpin-ledger-dev
AUTH_SECRET=development-only-hs256-secret-not-for-production-use

docker compose config --quiet
docker compose up --build -d --wait ledger postgres
trap 'docker compose down --remove-orphans --volumes' EXIT

export LEDGER_TEST_DATABASE_URL="postgres://ledger:ledger-development-only@localhost:${PG_HOST_PORT}/ledger?sslmode=disable"

# --- 1. Real-PostgreSQL two-connection CAS race -----------------------------------
cargo test -p ledger-store --features postgres --test pg_cas_race -- --ignored --nocapture

# --- 1b. Shared PostgreSQL immutable store (ADR-0012) ------------------------------------
cargo test -p ledger-store --features postgres --test pg_immutable_store -- --ignored --nocapture

# --- 1c. Graph authority schema (ADR-0010) incl. migration 0007 clean/upgrade -----------
cargo test -p ledger-store --features postgres --test pg_graphs_migration -- --ignored --nocapture

# --- 1d. Administrative filesystem -> PostgreSQL migration (ADR-0012) -------------------
cargo test -p ledger-store --features postgres --test pg_fs_migration -- --ignored --nocapture

# --- 1e. Atomic workflow persistence (ADR-0013): complete-actor idempotency, lineage,
#         fault injection, DB invariants ---------------------------------------------------
cargo test -p ledger-store --features postgres --test pg_workflow -- --ignored --nocapture

# --- 1f. Authenticated HTTP API over real PostgreSQL (P1.4) -------------------------------
cargo test -p ledger-api --test pg_api -- --ignored --nocapture

# --- 2. Containerised server: provision, authenticate, prepare/accept v2, restart, read ---
curl --fail --silent --retry 10 --retry-delay 2 "${BASE}/health" >/dev/null
curl --fail --silent "${BASE}/ready" >/dev/null

GRAPH="it-graph-$(date +%s)"
TENANT=tenant-integration
psql_q() { docker compose exec -T postgres psql -U ledger -d ledger -tAc "$1"; }
# Global baselines: the scenario must add exactly its own v2 commits and patches and no
# commit of any other version anywhere in the database.
OBJECTS_BEFORE=$(psql_q "select count(*) from immutable_objects")
NON_V2_BEFORE=$(psql_q "select count(*) from commit_index where version <> 2")
docker compose exec -T ledger ledger-admin graph create --graph "${GRAPH}" --tenant "${TENANT}" --status active --purpose 'integration test'
echo "graph provisioned by operator command: ${GRAPH}"

# Mint an HS256 token exactly as the identity provider would (stdlib only).
mint() {
  python3 - "$1" "$2" <<'PY'
import base64, hashlib, hmac, json, sys, time, os
def b64(b): return base64.urlsafe_b64encode(b).rstrip(b"=").decode()
issuer, audience, secret = os.environ["AUTH_ISSUER"], os.environ["AUTH_AUDIENCE"], os.environ["AUTH_SECRET"]
now = int(time.time())
claims = {"iss": issuer, "aud": audience, "exp": now + 900, "nbf": now - 30,
          "tid": sys.argv[1], "oid": "integration-agent", "sculpin_principal_type": "agent",
          "roles": sys.argv[2].split(",")}
header = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
payload = b64(json.dumps(claims).encode())
sig = b64(hmac.new(secret.encode(), f"{header}.{payload}".encode(), hashlib.sha256).digest())
print(f"{header}.{payload}.{sig}")
PY
}
export AUTH_ISSUER AUTH_AUDIENCE AUTH_SECRET
TOKEN=$(mint "${TENANT}" "ledger.read,ledger.propose,ledger.review")
FOREIGN=$(mint "tenant-someone-else" "ledger.read,ledger.propose,ledger.review")

json_field() { python3 -c 'import sys,json; d=json.load(sys.stdin); v=d["'"$1"'"]; print("null" if v is None else v)'; }

# $1=method $2=path $3=token|"" $4=idempotency key|"" $5=json body|""  → prints "<status>\n<body>"
api() {
  local method=$1 path=$2 token=$3 key=$4 body=$5 args=()
  [ -n "${token}" ] && args+=(-H "authorization: Bearer ${token}")
  [ -n "${key}" ] && args+=(-H "idempotency-key: ${key}")
  [ -n "${body}" ] && args+=(-H 'content-type: application/json' --data "${body}")
  curl --silent -X "${method}" "${BASE}${path}" "${args[@]}" -w '\n%{http_code}'
}
status_of() { tail -n1; }
body_of() { sed '$d'; }

# Unauthenticated and foreign-tenant access are refused without disclosure.
R=$(api GET "/v1/graphs/${GRAPH}/refs?name=main" "" "" "")
[ "$(echo "${R}" | status_of)" = "401" ] || { echo "FAIL: unauthenticated read not 401: ${R}" >&2; exit 1; }
echo "${R}" | body_of | grep -q '"code":"UNAUTHENTICATED"' || { echo "FAIL: envelope missing UNAUTHENTICATED: ${R}" >&2; exit 1; }
R=$(api GET "/v1/graphs/${GRAPH}/refs?name=main" "${FOREIGN}" "" "")
[ "$(echo "${R}" | status_of)" = "404" ] || { echo "FAIL: foreign tenant read not 404: ${R}" >&2; exit 1; }
R=$(api POST "/v1/commits" "${TOKEN}" "k" '{}')
[ "$(echo "${R}" | status_of)" = "404" ] || { echo "FAIL: bootstrap /v1/commits still routed: ${R}" >&2; exit 1; }
echo "authentication boundary confirmed: 401 without token, 404 for a foreign tenant, no /v1/commits"

# $1=expected head ("null" or id) $2=N-Quad $3=message $4=key → candidate id
prepare() {
  local body
  body=$(python3 -c 'import json,sys; exp=None if sys.argv[1]=="null" else sys.argv[1]; print(json.dumps({"ref":"main","expected_head":exp,"operations":[{"op":"add","quad":sys.argv[2]}],"activity":"integration","message":sys.argv[3],"event_time":"2026-09-25T00:00:00Z","evidence_refs":["urn:evidence:integration"],"source_system":"test-integration.sh"}))' "$1" "$2" "$3")
  local r; r=$(api POST "/v1/graphs/${GRAPH}/proposals" "${TOKEN}" "$4" "${body}")
  [ "$(echo "${r}" | status_of)" = "201" ] || { echo "FAIL: prepare: ${r}" >&2; exit 1; }
  echo "${r}" | body_of | json_field candidate
}
# $1=expected head $2=candidate $3=key → head
accept() {
  local body; body=$(python3 -c 'import json,sys; exp=None if sys.argv[1]=="null" else sys.argv[1]; print(json.dumps({"ref":"main","expected_head":exp,"reason":"integration"}))' "$1")
  local r; r=$(api POST "/v1/graphs/${GRAPH}/proposals/$2/accept" "${TOKEN}" "$3" "${body}")
  [ "$(echo "${r}" | status_of)" = "200" ] || { echo "FAIL: accept: ${r}" >&2; exit 1; }
  echo "${r}" | body_of | json_field head
}

C1=$(prepare null '<urn:material:a> <urn:temperature> "80" .' 'genesis' "it-p1")
# A lost prepare response: the retry is 200 with the identical candidate.
R=$(api POST "/v1/graphs/${GRAPH}/proposals" "${TOKEN}" "it-p1" "$(python3 -c 'import json,sys; print(json.dumps({"ref":"main","expected_head":None,"operations":[{"op":"add","quad":sys.argv[1]}],"activity":"integration","message":"genesis","event_time":"2026-09-25T00:00:00Z","evidence_refs":["urn:evidence:integration"],"source_system":"test-integration.sh"}))' '<urn:material:a> <urn:temperature> "80" .')")
[ "$(echo "${R}" | status_of)" = "200" ] || { echo "FAIL: prepare retry status: ${R}" >&2; exit 1; }
[ "$(echo "${R}" | body_of | json_field candidate)" = "${C1}" ] || { echo "FAIL: prepare retry candidate differs: ${R}" >&2; exit 1; }
[ "$(echo "${R}" | body_of | json_field replayed)" = "True" ] || { echo "FAIL: prepare retry not replayed: ${R}" >&2; exit 1; }
H1=$(accept null "${C1}" "it-a1")
[ "${H1}" = "${C1}" ] || { echo "FAIL: accepted head ${H1} != candidate ${C1}" >&2; exit 1; }
echo "genesis accepted: ${C1}"
# A lost accept response: the retry replays the identical decision instead of moving the ref twice.
R=$(api POST "/v1/graphs/${GRAPH}/proposals/${C1}/accept" "${TOKEN}" "it-a1" '{"ref":"main","expected_head":null,"reason":"integration"}')
[ "$(echo "${R}" | status_of)" = "200" ] || { echo "FAIL: accept retry status: ${R}" >&2; exit 1; }
[ "$(echo "${R}" | body_of | json_field replayed)" = "True" ] || { echo "FAIL: accept retry did not replay: ${R}" >&2; exit 1; }
[ "$(echo "${R}" | body_of | json_field head)" = "${C1}" ] || { echo "FAIL: accept retry head differs: ${R}" >&2; exit 1; }
[ "$(echo "${R}" | body_of | json_field ref_version)" = "1" ] || { echo "FAIL: accept retry ref_version differs: ${R}" >&2; exit 1; }

C2=$(prepare "${C1}" '<urn:material:a> <urn:humidity> "40" .' 'second' "it-p2")
H2=$(accept "${C1}" "${C2}" "it-a2")
[ "${H2}" = "${C2}" ] || { echo "FAIL: accepted head ${H2} != candidate ${C2}" >&2; exit 1; }
echo "second accepted: ${C2}"

# Restart only the ledger process; PostgreSQL carries refs, objects, workflow state.
docker compose restart ledger
curl --fail --silent --retry 20 --retry-delay 2 "${BASE}/health" >/dev/null
curl --fail --silent --retry 20 --retry-delay 2 "${BASE}/ready" >/dev/null

R=$(api GET "/v1/graphs/${GRAPH}/refs?name=main" "${TOKEN}" "" "")
HEAD_AFTER=$(echo "${R}" | body_of | json_field head)
[ "${HEAD_AFTER}" = "${C2}" ] || { echo "FAIL: head after restart ${HEAD_AFTER} != ${C2}" >&2; exit 1; }
echo "ref head survived restart: ${HEAD_AFTER}"

R=$(api GET "/v1/graphs/${GRAPH}/commits/${C2}/state" "${TOKEN}" "" "")
[ "$(echo "${R}" | status_of)" = "200" ] || { echo "FAIL: state read status: ${R}" >&2; exit 1; }
QUADS=$(echo "${R}" | body_of | python3 -c 'import sys,json; print(json.dumps(json.load(sys.stdin)["quads"]))')
EXPECTED_QUADS='["<urn:material:a> <urn:humidity> \"40\" .", "<urn:material:a> <urn:temperature> \"80\" ."]'
[ "${QUADS}" = "${EXPECTED_QUADS}" ] || { echo "FAIL: reconstructed state is not exactly the two quads: ${QUADS}" >&2; exit 1; }
echo "state reconstructed across restart through the authenticated, graph-scoped read"

# Foreign tenant still cannot see the commit after it exists.
R=$(api GET "/v1/graphs/${GRAPH}/commits/${C2}/state" "${FOREIGN}" "" "")
[ "$(echo "${R}" | status_of)" = "404" ] || { echo "FAIL: foreign tenant state read not 404: ${R}" >&2; exit 1; }

# --- Invariant verification (the only SQL in this script) ---------------------------------
IN_PG=$(psql_q "select count(*) from immutable_objects where id in ('${C1}','${C2}')")
[ "${IN_PG}" = "2" ] || { echo "FAIL: commits not in immutable_objects (count=${IN_PG})" >&2; exit 1; }
OBJECTS_AFTER=$(psql_q "select count(*) from immutable_objects")
NON_V2_AFTER=$(psql_q "select count(*) from commit_index where version <> 2")
[ "$((OBJECTS_AFTER - OBJECTS_BEFORE))" = "4" ] || { echo "FAIL: expected exactly 4 new objects (2 commits + 2 patches), got $((OBJECTS_AFTER - OBJECTS_BEFORE))" >&2; exit 1; }
[ "${NON_V2_AFTER}" = "${NON_V2_BEFORE}" ] || { echo "FAIL: a non-v2 commit was indexed somewhere during the scenario" >&2; exit 1; }
V2=$(psql_q "select count(*) from commit_index where id in ('${C1}','${C2}') and graph_id = '${GRAPH}' and version = 2")
[ "${V2}" = "2" ] || { echo "FAIL: commits not indexed as version 2 under ${GRAPH} (count=${V2})" >&2; exit 1; }
V1=$(psql_q "select count(*) from commit_index where graph_id = '${GRAPH}' and version <> 2")
[ "${V1}" = "0" ] || { echo "FAIL: non-v2 commits indexed under ${GRAPH} (count=${V1})" >&2; exit 1; }
REF_VERSION=$(psql_q "select version from refs where graph_id = '${GRAPH}' and branch = 'main'")
EVENTS=$(psql_q "select count(*) from ref_events where graph_id = '${GRAPH}' and branch = 'main'")
[ "${REF_VERSION}" = "2" ] && [ "${EVENTS}" = "2" ] || { echo "FAIL: refs.version=${REF_VERSION} ref_events=${EVENTS}; every ref move must have a ref event (raw ref path unused)" >&2; exit 1; }
DECISIONS=$(psql_q "select count(*) from decisions where graph_id = '${GRAPH}' and decision = 'accepted'")
[ "${DECISIONS}" = "2" ] || { echo "FAIL: expected 2 accepted decisions, got ${DECISIONS}" >&2; exit 1; }
OUTBOX=$(psql_q "select count(*) from projection_outbox where graph_id = '${GRAPH}'")
[ "${OUTBOX}" = "2" ] || { echo "FAIL: expected 2 outbox rows (one per ref move), got ${OUTBOX}" >&2; exit 1; }
IDEMPOTENCY=$(psql_q "select count(*) from idempotency where graph_id = '${GRAPH}'")
[ "${IDEMPOTENCY}" = "4" ] || { echo "FAIL: expected 4 idempotency rows (2 prepares + 2 accepts; retries add none), got ${IDEMPOTENCY}" >&2; exit 1; }
CORR=$(psql_q "select count(*) from decisions d join ref_events e on e.event_id = d.ref_event_id where d.graph_id = '${GRAPH}' and d.correlation_id is not null and e.correlation_id is not null")
[ "${CORR}" = "2" ] || { echo "FAIL: correlation ids missing on accepted decisions/events (count=${CORR})" >&2; exit 1; }
LOCAL_OBJECTS=$(docker compose exec -T ledger sh -c 'find /data -type f 2>/dev/null | wc -l')
[ "${LOCAL_OBJECTS}" = "0" ] || { echo "FAIL: ledger container holds ${LOCAL_OBJECTS} node-local object file(s)" >&2; exit 1; }
echo "invariants confirmed: 2 v2 commits indexed under ${GRAPH}, no non-v2 commit anywhere, exactly 4 new objects, refs.version=2 with 2 ref events, 2 accepted decisions, 2 outbox rows, 4 idempotency rows, correlation ids recorded, 0 node-local object files"

echo "INTEGRATION OK"
