#!/usr/bin/env bash
# Plan 0005 §8: backup/restore smoke against a live database.
# While a sustained write load runs (ledger-stress fault mode without any kills), take a
# logical dump (pg_dump -Fc) and a physical base backup (pg_basebackup); restore the dump into
# a new database and start a PostgreSQL instance from the base backup; then prove for both:
# `ledger-admin verify` is clean, every graph's ref-event chain is an exact prefix of the live
# chain, and a server pointed at the restored database reconstructs identical states for the
# restored heads (compared with the live server at the same commit ids).
# Usage: scripts/backup-restore.sh [writers] [graphs]
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker curl python3 cargo; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
WRITERS=${1:-50}
GRAPHS=${2:-10}
RUN=$(date -u +%Y%m%dT%H%M%SZ)
OUT="target/backup/${RUN}"; mkdir -p "${OUT}"
STOP="${OUT}/stop"; PROGRESS="${OUT}/progress.json"; WINDOW="${OUT}/window"; echo steady >"${WINDOW}"
PROJECT=ledger-qual-backup
NET="${PROJECT}_default"
BBVOL="${PROJECT}_basebackup"
BBNAME="${PROJECT}-bb"
PG_IMAGE=$(grep -oE 'image: postgres:[^ ]+' compose.yaml | head -1 | cut -d' ' -f2)
COMPOSE=(docker compose -p "${PROJECT}" -f compose.yaml -f compose.stress.yaml)
OWNER_HOST_URL='postgres://ledger:ledger-development-only@127.0.0.1:55432/ledger?sslmode=disable'
LOAD_PID=""
cleanup() {
  if [ -n "${LOAD_PID}" ]; then touch "${STOP}"; wait "${LOAD_PID}" 2>/dev/null || true; fi
  docker rm -f "${BBNAME}" "${PROJECT}-bbcopy" >/dev/null 2>&1 || true
  rm -rf "${OUT}/bb"
  docker rm -f "${PROJECT}-restored-server" >/dev/null 2>&1 || true
  "${COMPOSE[@]}" logs --no-color ledger ledger-b postgres >"${OUT}/containers.log" 2>&1 || true
  "${COMPOSE[@]}" down --remove-orphans --volumes >/dev/null 2>&1 || true
  docker volume rm "${BBVOL}" >/dev/null 2>&1 || true
}
trap cleanup EXIT
"${COMPOSE[@]}" config --quiet
"${COMPOSE[@]}" up --build -d --wait ledger ledger-b postgres
psql_q() { docker exec -i "$("${COMPOSE[@]}" ps -q postgres)" psql -U ledger -d "$1" -tAc "$2"; }
progress() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "${PROGRESS}" "$1" 2>/dev/null || echo 0; }
wait_progress() { local start; start=$(progress landed); local target=$(( start + $1 )); local i=0
  until [ "$(progress landed)" -ge "${target}" ]; do i=$((i+1)); [ "$i" -lt $(( $2 * 5 )) ] || { echo "FAIL: load did not progress within $2 s" >&2; return 1; }; sleep 0.2; done; }
wait_pg() { local i=0; until docker exec "$1" pg_isready -U ledger -d ledger >/dev/null 2>&1; do i=$((i+1)); [ "$i" -lt 300 ] || { echo "FAIL: $1 not ready" >&2; return 1; }; sleep 0.2; done; }
ulimit -n 65536 2>/dev/null || true
export LEDGER_STRESS_HS256_SECRET=development-only-hs256-secret-not-for-production-use
export LEDGER_STRESS_OWNER_DATABASE_URL="${OWNER_HOST_URL}"
cargo build --locked --release -p ledger-stress
./target/release/ledger-stress fault --replicas http://127.0.0.1:8080,http://127.0.0.1:8081 \
  --writers "${WRITERS}" --graphs "${GRAPHS}" --out "${OUT}/load" \
  --stop-file "${STOP}" --progress-file "${PROGRESS}" --window-file "${WINDOW}" >"${OUT}/load.log" 2>&1 &
LOAD_PID=$!
wait_progress 100 120
echo "live load established: $(progress landed) commits"

# --- Backups while writes continue ---------------------------------------------------------
PG_CID=$("${COMPOSE[@]}" ps -q postgres)
git describe --always --dirty --long >"${OUT}/build-rev.txt" 2>/dev/null || git rev-parse HEAD >"${OUT}/build-rev.txt"
# Watermarks: commits the clients had already been acknowledged BEFORE each backup started;
# the restore must contain at least that many ref events and every graph.
DUMP_BEFORE=$(progress landed)
docker exec "${PG_CID}" pg_dump -U ledger -Fc -d ledger -f /tmp/ledger.dump
DUMP_AT_LANDED=$(progress landed)
# Physical base backup over the local socket (the development pg_hba has no network
# replication entry; a production deployment grants `replication` to a dedicated role and
# streams over TLS), then moved into a fresh volume for a second instance.
BB_BEFORE=$(progress landed)
docker exec "${PG_CID}" sh -c 'rm -rf /tmp/bb && pg_basebackup -U ledger -D /tmp/bb -c fast -X stream' >"${OUT}/basebackup.log" 2>&1
BB_AT_LANDED=$(progress landed)
docker volume create "${BBVOL}" >/dev/null
docker cp "${PG_CID}:/tmp/bb" "${OUT}/bb" >/dev/null
docker create --name "${PROJECT}-bbcopy" -v "${BBVOL}:/data" "${PG_IMAGE}" true >/dev/null
docker cp "${OUT}/bb/." "${PROJECT}-bbcopy:/data" && docker rm "${PROJECT}-bbcopy" >/dev/null
rm -rf "${OUT}/bb"
wait_progress 50 120
echo "backups taken under load: dump at ${DUMP_AT_LANDED} commits, base backup at ${BB_AT_LANDED}, load now at $(progress landed)"
# Stop the load (the generator's own gate: invariants, no inconsistent replays, verifier).
touch "${STOP}"; set +e; wait "${LOAD_PID}"; LOAD_EXIT=$?; set -e; LOAD_PID=""
[ "${LOAD_EXIT}" -eq 0 ] || { echo "FAIL: load generator exit ${LOAD_EXIT}" >&2; cat "${OUT}/load/report.md" >&2; exit 1; }

# --- Restore: logical dump into a new database, physical base backup as a new instance ------
# Documented restore path (runbook): restore objects only (no owner, no ACLs), then let
# `ledger-admin migrate --runtime-role` re-derive the runtime grants from migration 0008
# instead of trusting ACLs carried in the dump; grant CONNECT explicitly.
psql_q ledger "CREATE DATABASE restored_dump" >/dev/null
docker exec "${PG_CID}" pg_restore -U ledger -d restored_dump --no-owner --no-acl /tmp/ledger.dump >"${OUT}/pg_restore.log" 2>&1 || { echo "pg_restore reported errors:" >&2; cat "${OUT}/pg_restore.log" >&2; exit 1; }
psql_q ledger "GRANT CONNECT ON DATABASE restored_dump TO ledger_runtime" >/dev/null
"${COMPOSE[@]}" run --rm migrate migrate --runtime-role ledger_runtime --database-url 'postgres://ledger:ledger-development-only@postgres:5432/restored_dump?sslmode=disable' >"${OUT}/restored-migrate.log" 2>&1 || { echo "FAIL: migrate/grant on the restored database" >&2; cat "${OUT}/restored-migrate.log" >&2; exit 1; }
# `--no-acl` also dropped the migrations' REVOKE … FROM PUBLIC on the ledger functions; the
# grant step re-applies them, and no function may be executable by PUBLIC afterwards.
PUBLIC_EXEC=$(psql_q restored_dump "SELECT count(*) FROM pg_proc p, aclexplode(p.proacl) a WHERE p.pronamespace = 'public'::regnamespace AND a.grantee = 0")
[ "${PUBLIC_EXEC}" = "0" ] || { echo "FAIL: ${PUBLIC_EXEC} function privilege(s) granted to PUBLIC after restore" >&2; exit 1; }
docker run -d --name "${BBNAME}" --network "${NET}" -e POSTGRES_PASSWORD=ledger-development-only \
  -v "${BBVOL}:/var/lib/postgresql/data" "${PG_IMAGE}" >/dev/null
wait_pg "${BBNAME}"
echo "restored: logical dump -> database restored_dump; base backup -> instance ${BBNAME}"

# --- Checks ----------------------------------------------------------------------------------
OWNER_IN_NET='postgres://ledger:ledger-development-only@postgres:5432'
RUNTIME_IN_NET='postgres://ledger_runtime:ledger-runtime-development-only@postgres:5432'
for target in "dump|${OWNER_IN_NET}/restored_dump?sslmode=disable|${RUNTIME_IN_NET}/restored_dump?sslmode=disable" \
              "basebackup|postgres://ledger:ledger-development-only@${BBNAME}:5432/ledger?sslmode=disable|postgres://ledger_runtime:ledger-runtime-development-only@${BBNAME}:5432/ledger?sslmode=disable"; do
  IFS='|' read -r name owner_url runtime_url <<<"${target}"
  # 1. Invariants on the restored database (read-only verifier).
  "${COMPOSE[@]}" run --rm migrate verify --database-url "${owner_url}" >"${OUT}/verify-${name}.log" 2>&1 \
    && grep -q "VERIFY OK" "${OUT}/verify-${name}.log" || { echo "FAIL: verify on ${name} restore" >&2; cat "${OUT}/verify-${name}.log" >&2; exit 1; }
  # 2. Every graph's ref-event chain in the restore is an exact prefix of the live chain.
  if [ "${name}" = dump ]; then
    docker exec "${PG_CID}" psql -U ledger -d restored_dump -tAc "SELECT graph_id||'|'||branch||'|'||new_version||'|'||new_head FROM ref_events ORDER BY 1" >"${OUT}/events-${name}.txt"
  else
    docker exec "${BBNAME}" psql -U ledger -d ledger -tAc "SELECT graph_id||'|'||branch||'|'||new_version||'|'||new_head FROM ref_events ORDER BY 1" >"${OUT}/events-${name}.txt"
  fi
  docker exec "${PG_CID}" psql -U ledger -d ledger -tAc "SELECT graph_id||'|'||branch||'|'||new_version||'|'||new_head FROM ref_events ORDER BY 1" >"${OUT}/events-live.txt"
  # Audit rows below the snapshot must exist identically in the live database (decisions,
  # outbox, idempotency results, proposals): a restore may only be a prefix, never differ.
  ROWS_SQL="SELECT 'd|'||decision_id||'|'||coalesce(proposal_id::text,'')||'|'||decision||'|'||coalesce(ref_event_id::text,'') FROM decisions UNION ALL SELECT 'o|'||outbox_id||'|'||ref_event_id||'|'||commit_id||'|'||ref_version FROM projection_outbox UNION ALL SELECT 'i|'||md5(row_to_json(i)::text) FROM idempotency i UNION ALL SELECT 'p|'||proposal_id||'|'||graph_id||'|'||candidate_commit FROM proposals ORDER BY 1"
  if [ "${name}" = dump ]; then docker exec "${PG_CID}" psql -U ledger -d restored_dump -tAc "${ROWS_SQL}" >"${OUT}/rows-${name}.txt"; else docker exec "${BBNAME}" psql -U ledger -d ledger -tAc "${ROWS_SQL}" >"${OUT}/rows-${name}.txt"; fi
  docker exec "${PG_CID}" psql -U ledger -d ledger -tAc "${ROWS_SQL}" >"${OUT}/rows-live.txt"
  if [ "${name}" = dump ]; then BEFORE=${DUMP_BEFORE}; else BEFORE=${BB_BEFORE}; fi
  docker exec "${PG_CID}" psql -U ledger -d ledger -tAc "SELECT graph_id FROM graphs ORDER BY 1" >"${OUT}/graphs-live.txt"
  if [ "${name}" = dump ]; then docker exec "${PG_CID}" psql -U ledger -d restored_dump -tAc "SELECT graph_id FROM graphs ORDER BY 1" >"${OUT}/graphs-${name}.txt"; else docker exec "${BBNAME}" psql -U ledger -d ledger -tAc "SELECT graph_id FROM graphs ORDER BY 1" >"${OUT}/graphs-${name}.txt"; fi
  python3 - "${OUT}/events-${name}.txt" "${OUT}/events-live.txt" "${name}" "${BEFORE}" "${OUT}/rows-${name}.txt" "${OUT}/rows-live.txt" "${OUT}/graphs-${name}.txt" "${OUT}/graphs-live.txt" <<'PY'
import sys, collections
def load(p):
    d = collections.defaultdict(dict)
    for line in open(p):
        line=line.strip()
        if not line: continue
        g,b,v,h = line.split('|')
        d[(g,b)][int(v)] = h
    return d
r, l, name, before = load(sys.argv[1]), load(sys.argv[2]), sys.argv[3], int(sys.argv[4])
rows_r = set(x.strip() for x in open(sys.argv[5]) if x.strip()); rows_l = set(x.strip() for x in open(sys.argv[6]) if x.strip())
graphs_r = sorted(x.strip() for x in open(sys.argv[7]) if x.strip()); graphs_l = sorted(x.strip() for x in open(sys.argv[8]) if x.strip())
if not r: sys.exit(f"FAIL: {name} restore has no ref events")
if graphs_r != graphs_l: sys.exit(f"FAIL: {name}: graph set differs ({len(graphs_r)} restored vs {len(graphs_l)} live)")
total = sum(len(e) for e in r.values())
if total < before: sys.exit(f"FAIL: {name}: {total} restored ref events < {before} commits acknowledged before the backup started")
missing = rows_r - rows_l
if missing: sys.exit(f"FAIL: {name}: {len(missing)} restored audit rows (decisions/outbox/idempotency/proposals) do not exist identically in the live database, e.g. {sorted(missing)[:2]}")
for ref, events in r.items():
    live = l.get(ref) or sys.exit(f"FAIL: {name}: ref {ref} missing live")
    n = max(events)
    if sorted(events) != list(range(1, n+1)): sys.exit(f"FAIL: {name}: {ref} chain not contiguous")
    for v, h in events.items():
        if live.get(v) != h: sys.exit(f"FAIL: {name}: {ref} version {v} head differs from live")
print(f"{name}: {len(graphs_r)} graphs (same set as live), {len(r)} refs, {total} ref events (>= {before} acknowledged before the backup), each chain an exact prefix of the live chain (live has {sum(len(e) for e in l.values())} events); {len(rows_r)} decision/outbox/idempotency/proposal rows all present identically in live")
PY
  # 3. A server against the restored database serves the restored heads and reconstructs
  #    states identical to the live server at the same commit ids.
  docker run -d --name "${PROJECT}-restored-server" --network "${NET}" -p 127.0.0.1:8090:8080 \
    -e LEDGER_ADDR=0.0.0.0:8080 -e LEDGER_DATABASE_URL="${runtime_url}" \
    -e LEDGER_AUTH_MODE=dev-hs256 -e LEDGER_AUTH_ISSUER=https://dev-issuer.example/ -e LEDGER_AUTH_AUDIENCE=api://sculpin-ledger-dev \
    -e LEDGER_AUTH_DEV_HS256_SECRET=development-only-hs256-secret-not-for-production-use \
    -e LEDGER_ALLOW_INSECURE_NON_LOOPBACK=allow-insecure-non-loopback-development-only \
    "$(docker inspect --format '{{.Image}}' "$("${COMPOSE[@]}" ps -q ledger)")" >/dev/null
  i=0; until curl -fs -o /dev/null http://127.0.0.1:8090/ready; do i=$((i+1)); [ "$i" -lt 150 ] || { echo "FAIL: restored server not ready (${name})" >&2; docker logs "${PROJECT}-restored-server" >&2; exit 1; }; sleep 0.2; done
  python3 - "${OUT}/events-${name}.txt" "${name}" <<'PY'
import base64, hashlib, hmac, json, sys, time, urllib.request, collections
def b64(b): return base64.urlsafe_b64encode(b).rstrip(b"=").decode()
now = int(time.time())
claims = {"iss": "https://dev-issuer.example/", "aud": "api://sculpin-ledger-dev", "exp": now + 900, "nbf": now - 30,
          "tid": "tenant-stress", "oid": "backup-check", "sculpin_principal_type": "agent", "roles": ["ledger.read"]}
h = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode()); p = b64(json.dumps(claims).encode())
sig = b64(hmac.new(b"development-only-hs256-secret-not-for-production-use", f"{h}.{p}".encode(), hashlib.sha256).digest())
token = f"{h}.{p}.{sig}"
def get(base, path):
    req = urllib.request.Request(base + path, headers={"authorization": f"Bearer {token}"})
    with urllib.request.urlopen(req, timeout=60) as r: return json.load(r)
heads = {}
for line in open(sys.argv[1]):
    line=line.strip()
    if not line: continue
    g,b,v,hd = line.split('|'); v=int(v)
    if v >= heads.get((g,b),(0,''))[0]: heads[(g,b)] = (v, hd)
checked = 0
for (g,b),(v,hd) in heads.items():
    r = get("http://127.0.0.1:8090", f"/v1/graphs/{g}/refs?name={b}")
    if r["head"] != hd or r["version"] != v: sys.exit(f"FAIL: {sys.argv[2]}: restored server ref {g}/{b} = {r} expected {hd}@{v}")
    restored = sorted(get("http://127.0.0.1:8090", f"/v1/graphs/{g}/commits/{hd}/state")["quads"])
    live = sorted(get("http://127.0.0.1:8080", f"/v1/graphs/{g}/commits/{hd}/state")["quads"])
    if restored != live: sys.exit(f"FAIL: {sys.argv[2]}: state of {hd} differs between restored and live")
    checked += 1
print(f"{sys.argv[2]}: {checked} restored heads served with versions matching, states identical to the live server ({sum(1 for _ in heads)} graphs, digest sha256:{hashlib.sha256(json.dumps(sorted(heads.items())).encode()).hexdigest()[:16]})")
PY
  docker rm -f "${PROJECT}-restored-server" >/dev/null
done | tee "${OUT}/checks.log"
grep -q "^dump: .* exact prefix.*present identically" "${OUT}/checks.log" && grep -q "^basebackup: .* exact prefix.*present identically" "${OUT}/checks.log" \
  && grep -q "^dump: .* states identical" "${OUT}/checks.log" && grep -q "^basebackup: .* states identical" "${OUT}/checks.log" \
  || { echo "FAIL: not every restore check passed" >&2; exit 1; }
# --- A restore that lost one integrity control or one grant must be refused at start-up,
#     not fail on first use (ADR-0016 verifier). Each case restores the dump afresh.
SERVER_IMAGE=$(docker inspect --format '{{.Image}}' "$("${COMPOSE[@]}" ps -q ledger)")
try_start() { # $1=database  → prints the server's exit code and first refusal line
  local out; out=$(timeout 60 docker run --rm --network "${NET}" \
    -e LEDGER_ADDR=0.0.0.0:8080 -e LEDGER_DATABASE_URL="${RUNTIME_IN_NET}/$1?sslmode=disable" \
    -e LEDGER_AUTH_MODE=dev-hs256 -e LEDGER_AUTH_ISSUER=https://dev-issuer.example/ -e LEDGER_AUTH_AUDIENCE=api://sculpin-ledger-dev \
    -e LEDGER_AUTH_DEV_HS256_SECRET=development-only-hs256-secret-not-for-production-use \
    -e LEDGER_ALLOW_INSECURE_NON_LOOPBACK=allow-insecure-non-loopback-development-only \
    "${SERVER_IMAGE}" 2>&1); local code=$?
  printf '%s\n%s\n' "${code}" "$(grep -m1 -E 'startup refused' <<<"${out}" || echo "${out}" | tail -1)"
}
declare -a DRIFTS=(
  "trigger|DROP TRIGGER refs_movement_audited ON refs|integrity trigger refs_movement_audited is missing on public.refs"
  "check|ALTER TABLE immutable_objects DROP CONSTRAINT immutable_objects_content_addressed|immutable_objects_content_addressed is missing"
  "column grant|REVOKE INSERT (bytes) ON immutable_objects FROM ledger_runtime|lacks INSERT on public.immutable_objects.bytes"
  "sequence grant|REVOKE USAGE ON SEQUENCE proposals_proposal_id_seq FROM ledger_runtime|lacks USAGE on sequence public.proposals_proposal_id_seq"
)
for d in "${DRIFTS[@]}"; do
  IFS='|' read -r name sql expect <<<"${d}"
  psql_q ledger "DROP DATABASE IF EXISTS restored_drift WITH (FORCE)" >/dev/null
  psql_q ledger "CREATE DATABASE restored_drift" >/dev/null
  docker exec "${PG_CID}" pg_restore -U ledger -d restored_drift --no-owner --no-acl /tmp/ledger.dump >/dev/null 2>&1 || { echo "FAIL: pg_restore for drift case ${name}" >&2; exit 1; }
  psql_q ledger "GRANT CONNECT ON DATABASE restored_drift TO ledger_runtime" >/dev/null
  "${COMPOSE[@]}" run --rm migrate migrate --runtime-role ledger_runtime --database-url 'postgres://ledger:ledger-development-only@postgres:5432/restored_drift?sslmode=disable' >/dev/null 2>&1
  psql_q restored_drift "${sql}" >/dev/null
  RESULT=$(try_start restored_drift); CODE=$(head -1 <<<"${RESULT}"); LINE=$(tail -1 <<<"${RESULT}")
  [ "${CODE}" != 0 ] && grep -q "${expect}" <<<"${LINE}" && echo "drift refused (${name}): exit ${CODE}: ${LINE#*startup refused: }" || { echo "FAIL: drift ${name} not refused (exit ${CODE}): ${LINE}" >&2; exit 1; }
done | tee -a "${OUT}/checks.log"
psql_q ledger "DROP DATABASE IF EXISTS restored_drift WITH (FORCE)" >/dev/null
[ "$(grep -c '^drift refused' "${OUT}/checks.log")" = 4 ] || { echo "FAIL: expected four refused drift cases" >&2; exit 1; }
cp "${OUT}/load/report.md" "${OUT}/load-report.md"
echo "report: ${OUT}/checks.log"
echo "BACKUP RESTORE OK"
