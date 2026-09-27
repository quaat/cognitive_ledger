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

# Own compose project: never takes over or deletes the developer's default stack.
export COMPOSE_PROJECT_NAME=ledger-qual-integration
docker compose config --quiet
# Production-shaped start (ADR-0016): PostgreSQL → owner migration (one-shot `migrate`
# service, runtime role granted) → server with the least-privilege runtime identity.
docker compose --profile projection up --build -d --wait ledger postgres fuseki projector
trap 'docker compose --profile projection down --remove-orphans --volumes' EXIT
REQUIRED_SCHEMA=$(grep -oE 'REQUIRED_SCHEMA_VERSION: i64 = [0-9]+' crates/ledger-store/src/schema.rs | grep -oE '[0-9]+$')
REQUIRED_SCHEMA=$(printf '%04d' "${REQUIRED_SCHEMA}")
docker compose logs migrate | grep -q "schema at ${REQUIRED_SCHEMA}" || { echo "FAIL: owner migration step did not report schema ${REQUIRED_SCHEMA}" >&2; docker compose logs migrate >&2; exit 1; }
docker compose logs migrate | grep -q "granted runtime privileges to role ledger_runtime" || { echo "FAIL: owner migration step did not grant the runtime role" >&2; exit 1; }
docker compose logs migrate | grep -q "granted projector privileges to role ledger_projector" || { echo "FAIL: owner migration step did not grant the projector role" >&2; exit 1; }
echo "owner migration completed; server runs as the runtime identity"

# Owner identity for the host-run store suites (they create throwaway databases and run
# migrations); the runtime identity for the least-privilege suite.
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

# --- 1g. Least privilege (ADR-0016, P1.5): owner migrates, runtime serves, runtime cannot
#         alter/drop/disable/rewrite, schema level fail-closed ----------------------------
cargo test -p ledger-store --features postgres --test pg_least_privilege -- --ignored --nocapture

# --- 1h. Invariant verifier (Plan 0005 §20): clean history passes, bypasses detected -----
cargo test -p ledger-store --features postgres --test pg_verify -- --ignored --nocapture

# --- 1i. Semantic validation persistence and acceptance binding (Plan 0006, ADR-0018/0019) --
cargo test -p ledger-store --features postgres --test pg_validation -- --ignored --nocapture

# --- 1j. Phase 2 over HTTP with a deterministic fake validator: every ADR-0014 scenario,
#         revalidation, idempotency, capabilities, tenant isolation, limits -----------------
cargo test -p ledger-api --test pg_validation_api -- --ignored --nocapture

# --- 1k. Projection streams: enable rules, leases, fencing, guards, projector identity (Plan 0007)
cargo test -p ledger-store --features postgres --test pg_projection -- --ignored --nocapture

# --- 1l. Projection against the real Fuseki (compose `fuseki`, own TDB2 dataset): genesis,
#         advance, duplicate/stale/equal-version writes, outage + catch-up, lost response,
#         every crash window at genesis and over a predecessor, lost/corrupt/ahead/foreign
#         markers, stale replacement vs newer projection, reconciliation, literal
#         canonicalization, concurrent workers, many graphs ----------------------------------
# One test at a time: TDB2 has a single writer and every commit costs ~0.5-1 s on the
# qualification host, so twelve tests sharing one dataset in parallel queue past the client
# timeout (a correct, retried TARGET_TIMEOUT — but not the scenario each test asserts).
# Concurrency *within* a scenario is still exercised (two workers, two loops).
LEDGER_TEST_FUSEKI_URL=http://127.0.0.1:53030/ledger LEDGER_TEST_FUSEKI_PASSWORD=development-only \
  cargo test -p ledger-projector --test fuseki_projection -- --ignored --nocapture --test-threads=1

# --- 2. Containerised server: provision, authenticate, prepare/accept v2, restart, read ---
curl --fail --silent --retry 10 --retry-delay 2 --retry-all-errors --retry-connrefused "${BASE}/health" >/dev/null
curl --fail --silent "${BASE}/ready" >/dev/null

GRAPH="it-graph-$(date +%s)"
TENANT=tenant-integration
psql_q() { docker compose exec -T postgres psql -U ledger -d ledger -tAc "$1"; }
# Global baselines: the scenario must add exactly its own v2 commits and patches and no
# commit of any other version anywhere in the database.
OBJECTS_BEFORE=$(psql_q "select count(*) from immutable_objects")
NON_V2_BEFORE=$(psql_q "select count(*) from commit_index where version <> 2")
# Provisioning is an owner operation: run it through the migrate service (owner URL), not
# inside the runtime container, which holds only the runtime identity.
docker compose run --rm migrate graph create --graph "${GRAPH}" --tenant "${TENANT}" --status active --purpose 'integration test' --kb "urn:integration:kb:${GRAPH}"
echo "graph provisioned by operator command under the owner identity: ${GRAPH}"
# Enable its accepted-state projection into the compose Fuseki (owner operation, ADR-0021).
docker compose run --rm migrate projection enable --graph "${GRAPH}" --target fuseki

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
curl --fail --silent --retry 20 --retry-delay 2 --retry-all-errors --retry-connrefused "${BASE}/health" >/dev/null
curl --fail --silent --retry 20 --retry-delay 2 --retry-all-errors --retry-connrefused "${BASE}/ready" >/dev/null

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

# --- Accepted-state projection (Plan 0007): the projector container converges Fuseki to ------
#     exactly the accepted state with a marker naming C2 / version 2 ------------------------
COGNITIVE=$(python3 -c 'import sys; kb=sys.argv[1]; print("urn:sculpin:kb:"+"".join(c if c.isalnum() or c in "-._~" else "%%%02X"%ord(c) for c in kb)+":cognitive")' "urn:integration:kb:${GRAPH}")
sparql() { curl --silent --fail -X POST -H 'content-type: application/sparql-query' -H 'accept: application/sparql-results+json' --data-binary "$1" http://127.0.0.1:53030/ledger/query; }
for _ in $(seq 1 120); do
  V=$(sparql "SELECT ?v WHERE { GRAPH <urn:sculpin:ledger-projection:v1:markers> { <${COGNITIVE}> <urn:sculpin:ledger-projection:v1#refVersion> ?v } }" | python3 -c 'import sys,json; b=json.load(sys.stdin)["results"]["bindings"]; print(b[0]["v"]["value"] if b else "")')
  [ "${V}" = "2" ] && break; sleep 0.5
done
[ "${V}" = "2" ] || { echo "FAIL: the projector did not project version 2 (marker: '${V}')" >&2; docker compose logs projector >&2; exit 1; }
PROJECTED=$(sparql "SELECT ?s ?p ?o WHERE { GRAPH <${COGNITIVE}> { ?s ?p ?o } } ORDER BY ?p" | python3 -c 'import sys,json; print(json.dumps(sorted((b["s"]["value"],b["p"]["value"],b["o"]["value"]) for b in json.load(sys.stdin)["results"]["bindings"])))')
EXPECTED_PROJECTED='[["urn:material:a", "urn:humidity", "40"], ["urn:material:a", "urn:temperature", "80"]]'
[ "${PROJECTED}" = "${EXPECTED_PROJECTED}" ] || { echo "FAIL: projected graph is not exactly the accepted state: ${PROJECTED}" >&2; exit 1; }
MARKER_COMMIT=$(sparql "SELECT ?c WHERE { GRAPH <urn:sculpin:ledger-projection:v1:markers> { <${COGNITIVE}> <urn:sculpin:ledger-projection:v1#commitId> ?c } }" | python3 -c 'import sys,json; print(json.load(sys.stdin)["results"]["bindings"][0]["c"]["value"])')
[ "${MARKER_COMMIT}" = "${C2}" ] || { echo "FAIL: marker commit ${MARKER_COMMIT} != ${C2}" >&2; exit 1; }
docker compose run --rm migrate projection status --target fuseki --json >target/integration-projection-status.json 2>/dev/null || true
python3 - "${GRAPH}" target/integration-projection-status.json <<'PY' || { echo "FAIL: projection status does not show the stream caught up" >&2; exit 1; }
import json, sys
graph = sys.argv[1]
status = json.loads(open(sys.argv[2]).read().strip().splitlines()[-1])
row = next(s for s in status["streams"] if s["graph_id"] == graph)
assert row["status"] == "active" and row["projected_ref_version"] == 2 and row["lag_versions"] == 0 and row["pending_events"] == 0, row
PY
curl --silent --fail http://127.0.0.1:59464/metrics | grep -q "projection_lag_versions{graph=\"${GRAPH}\",ref=\"main\",target=\"fuseki\",status=\"active\"} 0" || { echo "FAIL: projector metrics do not report zero lag for ${GRAPH}" >&2; exit 1; }
echo "projection: Fuseki cognitive graph <${COGNITIVE}> holds exactly the accepted state at C2 (marker v2), status lag 0, metrics lag 0"
# The operator verify command (projector identity, read-only on the target) agrees.
docker compose run --rm --no-deps projector verify --graph "${GRAPH}" | grep -q "PROJECTION CONSISTENT" || { echo "FAIL: ledger-projector verify does not report the stream consistent" >&2; exit 1; }
# A second deployment pointed at the same dataset under another target id refuses to start
# (dataset binding, ADR-0020): it never writes a target another deployment owns.
if OUT=$(docker compose run --rm --no-deps -e LEDGER_PROJECTION_TARGET_ID=fuseki-other projector run 2>&1); then
  echo "FAIL: a projector with another target id started against a bound dataset" >&2; exit 1
fi
echo "${OUT}" | grep -q "TARGET_CONFLICT" || { echo "FAIL: the second target id was not refused with TARGET_CONFLICT: ${OUT}" >&2; exit 1; }
# Target restart: TDB2 keeps the projection (durable commit), and the projector (restarted
# with it: it shares the target's network namespace in development) re-probes, re-binds and
# still verifies the stream consistent.
docker compose restart fuseki >/dev/null
docker compose up -d --wait --force-recreate --no-deps projector >/dev/null 2>&1 || { echo "FAIL: the projector did not become ready after a target restart" >&2; docker compose logs projector >&2; exit 1; }
V=$(sparql "SELECT ?v WHERE { GRAPH <urn:sculpin:ledger-projection:v1:markers> { <${COGNITIVE}> <urn:sculpin:ledger-projection:v1#refVersion> ?v } }" | python3 -c 'import sys,json; b=json.load(sys.stdin)["results"]["bindings"]; print(b[0]["v"]["value"] if b else "")')
[ "${V}" = "2" ] || { echo "FAIL: the projection did not survive a target restart (marker: '${V}')" >&2; exit 1; }
docker compose run --rm --no-deps projector verify --graph "${GRAPH}" | grep -q "PROJECTION CONSISTENT" || { echo "FAIL: projection inconsistent after a target restart" >&2; exit 1; }
echo "projection: verify consistent; a second target id is refused; the projection survives a target restart"

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
# The serving process holds only the runtime identity: no owner credentials anywhere in its
# environment, its database sessions belong to ledger_runtime and none to the owner, and
# the runtime role cannot alter the schema.
# The runtime image is distroless (no shell): inspect the container's configured environment
# from the outside instead of exec-ing into it.
LEDGER_CONTAINER=$(docker compose ps -q ledger)
LEDGER_ENV=$(docker inspect --format '{{join .Config.Env "\n"}}' "${LEDGER_CONTAINER}")
grep -qE '^LEDGER_MIGRATION' <<<"${LEDGER_ENV}" && { echo "FAIL: runtime container holds an owner/migration variable" >&2; exit 1; }
grep -qE 'ledger-development-only@|postgres://ledger:' <<<"${LEDGER_ENV}" && { echo "FAIL: runtime container holds the owner credentials" >&2; exit 1; }
[ "$(docker inspect --format '{{.Config.User}}' "${LEDGER_CONTAINER}")" = "65532:65532" ] || { echo "FAIL: runtime container does not run as the non-root distroless user" >&2; exit 1; }
SHELL_PROBE=$(docker compose exec -T ledger /bin/sh -c 'true' 2>&1 || true)
grep -qiE 'executable file not found|no such file or directory' <<<"${SHELL_PROBE}" || { echo "FAIL: runtime image appears to contain a shell: ${SHELL_PROBE}" >&2; exit 1; }
RT_SESSIONS=$(psql_q "select count(*) from pg_stat_activity where datname = 'ledger' and usename = 'ledger_runtime' and client_addr is not null")
OWNER_SESSIONS=$(psql_q "select count(*) from pg_stat_activity where datname = 'ledger' and usename = 'ledger' and client_addr is not null and application_name <> 'psql'")
[ "${RT_SESSIONS}" -ge 1 ] || { echo "FAIL: the server has no sessions as ledger_runtime (${RT_SESSIONS})" >&2; exit 1; }
[ "${OWNER_SESSIONS}" = "0" ] || { echo "FAIL: ${OWNER_SESSIONS} network session(s) run as the owner while only the server should be connected" >&2; exit 1; }
RT_ALTER=$(docker compose exec -T postgres psql -U ledger_runtime -d ledger -tAc "ALTER TABLE immutable_objects DISABLE TRIGGER immutable_objects_write_once" 2>&1 || true)
echo "${RT_ALTER}" | grep -q "must be owner of table immutable_objects" || { echo "FAIL: runtime role could alter a ledger table: ${RT_ALTER}" >&2; exit 1; }
# Readiness follows the schema level live: a migration recorded by a newer build makes
# /ready refuse until it is gone.
psql_q "insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (9999, 'from the future', true, '\\x00', 0)" >/dev/null
READY_DRIFT=$(curl --silent -o /dev/null -w '%{http_code}' "${BASE}/ready")
psql_q "delete from _sqlx_migrations where version = 9999" >/dev/null
[ "${READY_DRIFT}" = "503" ] || { echo "FAIL: /ready returned ${READY_DRIFT} on a drifted schema" >&2; exit 1; }
curl --fail --silent --retry 5 --retry-delay 1 --retry-all-errors "${BASE}/ready" >/dev/null
# A server pointed at a never-migrated database refuses to start (process level).
psql_q "create database stale_schema" >/dev/null
set +e
STALE_OUT=$(docker compose run --rm --no-deps -e LEDGER_DATABASE_URL="postgres://ledger_runtime:ledger-runtime-development-only@postgres:5432/stale_schema?sslmode=disable" ledger 2>&1)
STALE_EXIT=$?
set -e
[ "${STALE_EXIT}" -ne 0 ] || { echo "FAIL: server started against a never-migrated database" >&2; exit 1; }
echo "${STALE_OUT}" | grep -q "never been migrated\|permission denied\|cannot read the migration metadata" || { echo "FAIL: stale-schema refusal lacks an actionable message: ${STALE_OUT}" >&2; exit 1; }
# The owner identity is refused as a runtime identity even on a correct schema.
set +e
OWNER_OUT=$(docker compose run --rm --no-deps -e LEDGER_DATABASE_URL="postgres://ledger:ledger-development-only@postgres:5432/ledger?sslmode=disable" ledger 2>&1)
OWNER_EXIT=$?
set -e
[ "${OWNER_EXIT}" -ne 0 ] || { echo "FAIL: server started as the schema owner" >&2; exit 1; }
echo "${OWNER_OUT}" | grep -q "RUNTIME_IDENTITY" || { echo "FAIL: owner-identity refusal lacks the RUNTIME_IDENTITY reason: ${OWNER_OUT}" >&2; exit 1; }
echo "identity boundary confirmed: server sessions run as ledger_runtime, owner refused as runtime, stale schema refused, /ready follows schema drift"
# Node-local object files would live in the ledger-data volume; read it from a throwaway
# busybox container because the runtime image has no shell.
DATA_VOLUME=$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Name}}{{end}}{{end}}' "${LEDGER_CONTAINER}")
[ -n "${DATA_VOLUME}" ] || { echo "FAIL: could not resolve the ledger /data volume" >&2; exit 1; }
LOCAL_OBJECTS=$(docker run --rm -v "${DATA_VOLUME}:/data:ro" busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e sh -c 'find /data -type f | wc -l' | tr -d '[:space:]')
[ "${LOCAL_OBJECTS}" = "0" ] || { echo "FAIL: ledger container holds ${LOCAL_OBJECTS} node-local object file(s)" >&2; exit 1; }
echo "invariants confirmed: 2 v2 commits indexed under ${GRAPH}, no non-v2 commit anywhere, exactly 4 new objects, refs.version=2 with 2 ref events, 2 accepted decisions, 2 outbox rows, 4 idempotency rows, correlation ids recorded, 0 node-local object files"
# The reusable invariant suite (Plan 0005 §20) over the whole database, as the owner.
docker compose run --rm migrate verify | tail -3 | grep -q "VERIFY OK" || { echo "FAIL: ledger-admin verify reported violations" >&2; docker compose run --rm migrate verify >&2 || true; exit 1; }
echo "ledger-admin verify: VERIFY OK"

echo "INTEGRATION OK"
