#!/usr/bin/env bash
# Plan 0009 gate: upgrade qualification from the merged Phase-4 release (schema 0012) to the
# Phase-5 working tree (schema 0013, diff and merge).
#
# PREVIOUS (default 5216bce = main's PR #9 merge) is built from git in a worktree and deployed
# as owner/runtime/projector with a real Fuseki. Populated through the OLD API: the Phase-2
# workload (two tenants, a `dev` ref, rejected/pending proposals), the Phase-3 default-graph
# graphs, validations and validated acceptances, the Phase-4 branch workflow (agent/task-17
# created from main, three validated cycles, delete/restore), projection to lag 0 and a
# backlog.
#
# Proves, failing loudly: every pre-upgrade row byte-identical after migrate and after
# replaying every recorded key; 0001-0012 checksums untouched; no merge row created by 0013;
# VERIFY OK; the new projector consumes the old backlog; on the upgraded graph the branch the
# OLD release created is merged into main (preview read-only, propose, ordinary accept
# refused, validated apply, replays, repeat contained, reverse no_change) and the merge is
# projected; the old server refuses 0013; the new server and projector refuse 0012; clean
# 0013 install == upgraded 0013.
#
# Networking: host network, loopback binds; ports/names distinct from the other harnesses.
# Usage: scripts/upgrade-p5.sh [previous-git-rev] [commits-per-main-ref]
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker python3 git sha384sum; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
PREVIOUS=${1:-5216bce}
COMMITS=${2:-6}
RUN=$(date -u +%Y%m%dT%H%M%SZ)
OUT="target/upgrade-p5/${RUN}"; mkdir -p "${OUT}"
exec > >(tee "${OUT}/run.log") 2>&1
PROJECT=ledger-qual-upgrade-p5
PG="${PROJECT}-postgres"; OLD="${PROJECT}-previous"; NEW="${PROJECT}-ledger"
FUSEKI="${PROJECT}-fuseki"; PROJ="${PROJECT}-projector"
PGPORT=55438; PORT=18580; VPORT=18590; FPORT=53530; MPORT=19564
PG_IMAGE='postgres:17.2-bookworm@sha256:3267c505060a0052e5aa6e5175a7b41ab6b04da2f8c4540fc6e98a37210aa2d3'
FUSEKI_IMAGE=$(grep -oE 'stain/jena-fuseki:[^ ]+@sha256:[0-9a-f]{64}' compose.yaml | head -1)
OLD_IMAGE="cognitive_ledger-previous:${PREVIOUS:0:12}"
NEW_IMAGE="${PROJECT}:working-tree"
SRC="${OUT}/previous-src"
SRC_CREATED=""
OWNER_URL="postgres://ledger:ledger-development-only@127.0.0.1:${PGPORT}"
RUNTIME_URL="postgres://ledger_runtime:ledger-runtime-development-only@127.0.0.1:${PGPORT}"
PROJECTOR_URL="postgres://ledger_projector:ledger-projector-development-only@127.0.0.1:${PGPORT}"
FUSEKI_PASSWORD=$(cat deploy/fuseki/development-admin-password)
TARGET_ID=upgrade-p5
AUTH_ISSUER=https://dev-issuer.example/; AUTH_AUDIENCE=api://sculpin-ledger-dev
AUTH_SECRET=development-only-hs256-secret-not-for-production-use
VALIDATOR_SERVICE_ID="urn:upgrade-p5:fake-validator"
export AUTH_ISSUER AUTH_AUDIENCE AUTH_SECRET
VAL_PID=""
T0=$SECONDS; LAST=$SECONDS
step() { local now=$SECONDS; echo "[t+$((now-T0))s, previous step $((now-LAST))s] $*"; echo "$((now-LAST)) $*" >>"${OUT}/durations.txt"; LAST=$now; }
fail() { echo "FAIL: $*" >&2; exit 1; }
cleanup() {
  local rc=$?
  [ -n "${VAL_PID}" ] && kill "${VAL_PID}" 2>/dev/null || true
  for c in "${OLD}" "${PROJ}-old" "${NEW}" "${OLD}-ahead" "${NEW}-behind" "${PROJ}-behind" "${OLD}-restored" "${PROJ}" "${FUSEKI}"; do
    docker logs "$c" >"${OUT}/container-${c}.log" 2>&1 || true
    docker rm -fv "$c" >/dev/null 2>&1 || true
  done
  docker logs "${PG}" >"${OUT}/container-${PG}.log" 2>&1 || true
  docker rm -fv "${PG}" >/dev/null 2>&1 || true
  [ -n "${SRC_CREATED}" ] && git worktree remove "${SRC}" >/dev/null 2>&1 || true
  echo "exit ${rc}; total $((SECONDS-T0))s; report: ${OUT}"
}
trap cleanup EXIT

psql_q() { docker exec -i "${PG}" psql -v ON_ERROR_STOP=1 -U ledger -d "$1" -tAc "$2"; }
admin() { local image=$1 db=$2; shift 2; docker run --rm --network host --entrypoint /usr/local/bin/ledger-admin \
  -e LEDGER_MIGRATION_DATABASE_URL="${OWNER_URL}/${db}?sslmode=disable" "${image}" "$@"; }
wait_ready() { local i=0; until python3 -c "import urllib.request,sys; urllib.request.urlopen('http://127.0.0.1:$1/ready', timeout=2)" 2>/dev/null; do
  i=$((i+1)); [ "$i" -lt $(( $2 * 4 )) ] || return 1; sleep 0.25; done; }
schema_level() { psql_q "$1" "SELECT max(version) FROM _sqlx_migrations"; }
server() { # name image db port [extra docker -e args...]
  local name=$1 image=$2 db=$3 port=$4; shift 4
  docker run -d --name "${name}" --network host \
    -e LEDGER_ADDR="127.0.0.1:${port}" -e LEDGER_DATABASE_URL="${RUNTIME_URL}/${db}?sslmode=disable" \
    -e LEDGER_AUTH_MODE=dev-hs256 -e LEDGER_AUTH_ISSUER="${AUTH_ISSUER}" -e LEDGER_AUTH_AUDIENCE="${AUTH_AUDIENCE}" \
    -e LEDGER_AUTH_DEV_HS256_SECRET="${AUTH_SECRET}" -e RUST_LOG=info "$@" "${image}" >/dev/null
}
validator_env=(-e LEDGER_VALIDATOR_URL="http://127.0.0.1:${VPORT}/validate" -e LEDGER_VALIDATOR_SERVICE_ID="${VALIDATOR_SERVICE_ID}")
projector() { # name db [args...]  (new image; PROJ_IMAGE overrides)
  local name=$1 db=$2; shift 2
  docker run -d --name "${name}" --network host --entrypoint /usr/local/bin/ledger-projector \
    -e LEDGER_PROJECTOR_DATABASE_URL="${PROJECTOR_URL}/${db}?sslmode=disable" -e LEDGER_PROJECTION_TARGET_ID="${TARGET_ID}" \
    -e LEDGER_PROJECTION_QUERY_URL="http://127.0.0.1:${FPORT}/ledger/query" -e LEDGER_PROJECTION_UPDATE_URL="http://127.0.0.1:${FPORT}/ledger/update" \
    -e LEDGER_PROJECTION_USERNAME=admin -e LEDGER_PROJECTION_PASSWORD_FILE=/run/secrets/fuseki-password \
    -v "${PWD}/deploy/fuseki/development-admin-password:/run/secrets/fuseki-password:ro" \
    -e LEDGER_PROJECTOR_DEVELOPMENT=allow-insecure-development-only -e LEDGER_PROJECTOR_ADDR="127.0.0.1:${MPORT}" \
    -e LEDGER_PROJECTOR_POLL_MS=200 -e RUST_LOG=info "${PROJ_IMAGE:-${NEW_IMAGE}}" "$@" >/dev/null
}
status_json() { admin "${NEW_IMAGE}" ledger projection status --target "${TARGET_ID}" --json 2>/dev/null | tail -1; }
# Ordered, column-explicit dump of every pre-upgrade table (columns as of 0011). $3 names a
# column-list directory (default: every column).
snapshot() { # db label [cols-dir]
  local dir="${OUT}/snap-$2" cdir="${3:-${OUT}/cols}"; mkdir -p "${dir}"
  while read -r t <&3; do
    local cols n where=""
    cols=$(cat "${cdir}/${t}"); n=$(tr ',' '\n' <"${cdir}/${t}" | wc -l)
    [ "$t" = "_sqlx_migrations" ] && where="WHERE version <= ${PREV_SCHEMA}"
    psql_q "$1" "COPY (SELECT ${cols} FROM public.${t} ${where} ORDER BY $(seq -s, 1 "${n}")) TO STDOUT" >"${dir}/${t}.tsv"
  done 3<"${OUT}/tables.txt"
}
same_snapshot() { # labelA labelB what
  local na nb; na=$(ls "${OUT}/snap-$1" | wc -l); nb=$(ls "${OUT}/snap-$2" | wc -l)
  [ "${na}" = "$(wc -l <"${OUT}/tables.txt")" ] && [ "${nb}" = "${na}" ] || fail "snapshot $1/$2 incomplete (${na}/${nb} tables)"
  diff -r "${OUT}/snap-$1" "${OUT}/snap-$2" >"${OUT}/snap-$1-vs-$2.diff" || { head -40 "${OUT}/snap-$1-vs-$2.diff" >&2; fail "$3: rows differ ($1 vs $2)"; }
}
schema_dump() { # db label
  docker exec "${PG}" pg_dump -U ledger --schema-only --no-owner -d "$1" | grep -vE '^(--|SET |SELECT pg_catalog|\\connect|\\restrict|\\unrestrict|$)' | sed -E 's/[[:space:]]+$//' >"${OUT}/schema-$2.sql"
  psql_q "$1" "SELECT 'rel|'||relname||'|'||pg_get_userbyid(relowner) FROM pg_class WHERE relnamespace='public'::regnamespace UNION ALL SELECT 'fn|'||proname||'|'||pg_get_userbyid(proowner) FROM pg_proc WHERE pronamespace='public'::regnamespace UNION ALL SELECT 'schema|public|'||pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='public' ORDER BY 1" >"${OUT}/owners-$2.txt"
}

# --- 0. Disk guard, images ------------------------------------------------------------------
avail=$(df --output=avail -B1G / | tail -1 | tr -d ' ')
[ "${avail}" -ge 4 ] || fail "only ${avail} GB free on /; need >= 4 GB for the image builds"
[ -n "${FUSEKI_IMAGE}" ] || fail "no pinned Fuseki image in compose.yaml"
PREVIOUS=$(git rev-parse --verify "${PREVIOUS}^{commit}") || fail "unknown previous revision ${PREVIOUS}"
[ ! -e "${SRC}" ] || fail "${SRC} already exists"
git worktree add --detach "${SRC}" "${PREVIOUS}" >/dev/null && SRC_CREATED=1
git -C "${SRC}" rev-parse HEAD >"${OUT}/previous-rev.txt"
PREV_SCHEMA=$(grep -oE 'REQUIRED_SCHEMA_VERSION: i64 = [0-9]+' "${SRC}/crates/ledger-store/src/schema.rs" | grep -oE '[0-9]+$')
NEW_SCHEMA=$(grep -oE 'REQUIRED_SCHEMA_VERSION: i64 = [0-9]+' crates/ledger-store/src/schema.rs | grep -oE '[0-9]+$')
[ "${PREV_SCHEMA}" = 12 ] && [ "${NEW_SCHEMA}" = 13 ] || fail "expected previous schema 12 and new schema 13, got ${PREV_SCHEMA} -> ${NEW_SCHEMA}"
{ git rev-parse HEAD; git status --porcelain; echo "diff-sha256 $(git diff HEAD | sha256sum | cut -d' ' -f1)"; } >"${OUT}/working-tree.txt"
docker build -q -t "${OLD_IMAGE}" "${SRC}" >"${OUT}/previous-image.txt"
docker build -q -t "${NEW_IMAGE}" . >"${OUT}/new-image.txt"
step "images: previous $(cat "${OUT}/previous-rev.txt") (schema ${PREV_SCHEMA}) = ${OLD_IMAGE} $(cut -c1-19 "${OUT}/previous-image.txt"); working tree $(head -1 "${OUT}/working-tree.txt" | cut -c1-12)+$(($(wc -l <"${OUT}/working-tree.txt")-2)) changes (schema ${NEW_SCHEMA}) = ${NEW_IMAGE} $(cut -c1-19 "${OUT}/new-image.txt"); target ${FUSEKI_IMAGE}"

# --- 1. PostgreSQL, Fuseki and the Phase-4 deployment ---------------------------------------
docker rm -fv "${PG}" "${FUSEKI}" >/dev/null 2>&1 || true
docker run -d --name "${PG}" -p "127.0.0.1:${PGPORT}:5432" -e POSTGRES_USER=ledger -e POSTGRES_PASSWORD=ledger-development-only \
  -e POSTGRES_DB=ledger "${PG_IMAGE}" >/dev/null
docker run -d --name "${FUSEKI}" -p "127.0.0.1:${FPORT}:3030" -e ADMIN_PASSWORD="${FUSEKI_PASSWORD}" \
  -v "${PWD}/deploy/fuseki/ledger-projection.ttl:/staging/ledger-projection.ttl:ro" "${FUSEKI_IMAGE}" \
  /jena-fuseki/fuseki-server --config=/staging/ledger-projection.ttl >/dev/null
i=0; until docker exec "${PG}" pg_isready -h 127.0.0.1 -U ledger -d ledger >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 120 ] || fail "postgres not ready"; sleep 0.5; done
sleep 1; docker exec "${PG}" pg_isready -h 127.0.0.1 -U ledger -d ledger >/dev/null
i=0; until curl -sf "http://127.0.0.1:${FPORT}/\$/ping" >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 120 ] || fail "fuseki not up"; sleep 0.5; done
psql_q ledger "CREATE ROLE ledger_runtime LOGIN PASSWORD 'ledger-runtime-development-only'" >/dev/null
psql_q ledger "CREATE ROLE ledger_projector LOGIN PASSWORD 'ledger-projector-development-only'" >/dev/null
admin "${OLD_IMAGE}" ledger migrate --runtime-role ledger_runtime --projector-role ledger_projector >"${OUT}/previous-migrate.log" 2>&1 || { cat "${OUT}/previous-migrate.log"; fail "previous release migrate"; }
[ "$(schema_level ledger)" = "${PREV_SCHEMA}" ] || fail "previous release did not migrate to ${PREV_SCHEMA}"
OLD_CHECKSUMS=$(psql_q ledger "SELECT string_agg(version||':'||encode(checksum,'hex'), ',' ORDER BY version) FROM _sqlx_migrations")
STAMP=$(date +%s)
GRAPHS="[{\"graph\":\"up5-a1-${STAMP}\",\"tenant\":\"tenant-a\",\"kb\":\"urn:upgrade:kb:a1-${STAMP}\"},{\"graph\":\"up5-b1-${STAMP}\",\"tenant\":\"tenant-b\",\"kb\":\"urn:upgrade:kb:b1-${STAMP}\"},{\"graph\":\"up5-a2-${STAMP}\",\"tenant\":\"tenant-a\",\"kb\":\"urn:upgrade:kb:a2-${STAMP}\"}]"
PGRAPHS="[{\"graph\":\"up5-pa-${STAMP}\",\"tenant\":\"tenant-a\",\"kb\":\"urn:upgrade:kb:pa-${STAMP}\"},{\"graph\":\"up5-pb-${STAMP}\",\"tenant\":\"tenant-b\",\"kb\":\"urn:upgrade:kb:pb-${STAMP}\"}]"
export GRAPHS PGRAPHS BASE="http://127.0.0.1:${PORT}"
python3 -c 'import json,os; [print(g["graph"], g["tenant"], g["kb"]) for g in json.loads(os.environ["GRAPHS"]) + json.loads(os.environ["PGRAPHS"])]' >"${OUT}/graphs.txt"
while read -r g t kb; do
  admin "${OLD_IMAGE}" ledger graph create --graph "$g" --tenant "$t" --status active --kb "$kb" --purpose 'upgrade-p5 qualification' >>"${OUT}/previous-graphs.log" 2>&1 || { cat "${OUT}/previous-graphs.log"; fail "graph create"; }
done <"${OUT}/graphs.txt"
server "${OLD}" "${OLD_IMAGE}" ledger "${PORT}" -e LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only
wait_ready "${PORT}" 60 || { docker logs "${OLD}" | tail -20; fail "previous server not ready"; }
UPGRADE_CREATE_BRANCHES=1 python3 scripts/upgrade-p2/workload.py populate "${OUT}/old-writes.json" "${COMMITS}"
python3 scripts/upgrade-p3/workload.py populate-default "${OUT}/p3.json" "${OUT}/recorded.json" "${COMMITS}"
docker stop -t 30 "${OLD}" >/dev/null; docker rm -f "${OLD}" >/dev/null
python3 scripts/upgrade-p2/fake-validator.py 127.0.0.1 "${VPORT}" "${OUT}/validator-calls.jsonl" >"${OUT}/fake-validator.log" 2>&1 &
VAL_PID=$!
i=0; until python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:${VPORT}/health', timeout=1)" 2>/dev/null; do i=$((i+1)); [ $i -lt 40 ] || fail "fake validator not up"; sleep 0.25; done
server "${OLD}" "${OLD_IMAGE}" ledger "${PORT}" "${validator_env[@]}"
wait_ready "${PORT}" 60 || { docker logs "${OLD}" | tail -20; fail "previous server (validator configured) not ready"; }
python3 scripts/upgrade-p3/workload.py phase2-old "${OUT}/old-writes.json" "${OUT}/p3.json" "${OUT}/recorded.json" "${OUT}/phase2-old.json"
# Phase-4 state through the OLD API: a cognitive branch with history, deleted and restored.
P_GRAPH=$(grep '^up5-pa-' "${OUT}/graphs.txt" | cut -d' ' -f1); P_TENANT=$(grep '^up5-pa-' "${OUT}/graphs.txt" | cut -d' ' -f2)
python3 scripts/upgrade-p4/workload.py branches "${P_GRAPH}" "${P_TENANT}" "${OUT}/branches.json"
# The previous release projects: streams for the projectable graphs, run to lag 0.
while read -r g t kb; do
  case "$g" in up5-p*) admin "${OLD_IMAGE}" ledger projection enable --graph "$g" --target "${TARGET_ID}" >>"${OUT}/enable.log" 2>&1 || { cat "${OUT}/enable.log"; fail "enable ${g}"; } ;; esac
done <"${OUT}/graphs.txt"
PROJ_IMAGE="${OLD_IMAGE}" projector "${PROJ}-old" ledger run
wait_ready "${MPORT}" 60 || { docker logs "${PROJ}-old" | tail -30; fail "previous projector not ready"; }
old_status() { admin "${OLD_IMAGE}" ledger projection status --target "${TARGET_ID}" --json 2>/dev/null | tail -1; }
ok=""; for _ in $(seq 1 240); do
  old_status >"${OUT}/old-status.json" || true
  python3 -c 'import json,sys; r=json.load(open(sys.argv[1]))["streams"]; sys.exit(0 if r and all(s["lag_versions"]==0 and s["status"]=="active" for s in r) else 1)' "${OUT}/old-status.json" 2>/dev/null && { ok=1; break; }
  sleep 0.5
done
[ -n "${ok}" ] || { cat "${OUT}/old-status.json"; fail "the previous projector did not project the populated history"; }
docker stop -t 30 "${PROJ}-old" >/dev/null; docker rm -f "${PROJ}-old" >/dev/null
# A backlog the old projector never sees: one more validated acceptance on a projectable graph.
python3 scripts/upgrade-p3/workload.py live "${OUT}/p3.json" "${OUT}/pre-upgrade-live.json"
admin "${OLD_IMAGE}" ledger verify >"${OUT}/previous-verify.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/previous-verify.log" || { cat "${OUT}/previous-verify.log"; fail "previous verify before upgrade"; }
psql_q ledger "SELECT 'validation_records '||count(*) FROM validation_records UNION ALL SELECT 'projection_state '||count(*) FROM projection_state UNION ALL SELECT 'outbox delivered '||count(*) FROM projection_outbox WHERE delivered_at IS NOT NULL UNION ALL SELECT 'outbox undelivered main '||count(*) FROM projection_outbox WHERE delivered_at IS NULL AND branch='main' UNION ALL SELECT 'refs '||count(*) FROM refs ORDER BY 1" >"${OUT}/previous-counts.txt"
[ "$(psql_q ledger "SELECT count(*) FROM projection_outbox o JOIN projection_state s ON s.graph_id = o.graph_id AND s.branch = o.branch WHERE o.delivered_at IS NULL")" -ge 1 ] || fail "no projection backlog was left for the new projector"
step "previous release populated and projecting: $(tr '\n' ';' <"${OUT}/previous-counts.txt") VERIFY OK"

# --- 2. Transition ----------------------------------------------------------------------------
docker stop -t 30 "${OLD}" >/dev/null; docker rm -f "${OLD}" >/dev/null
[ "$(psql_q ledger "SELECT count(*) FROM pg_stat_activity WHERE usename = 'ledger_runtime'")" = 0 ] || fail "runtime connections remain"
mkdir -p "${OUT}/cols" "${OUT}/cols-progress"
psql_q ledger "SELECT tablename FROM pg_tables WHERE schemaname='public' ORDER BY 1" >"${OUT}/tables.txt"
while read -r t <&3; do
  psql_q ledger "SELECT string_agg(quote_ident(column_name), ',' ORDER BY ordinal_position) FROM information_schema.columns WHERE table_schema='public' AND table_name='${t}'" >"${OUT}/cols/${t}"
  # The same without the outbox delivery columns (the projector's only writes to old tables).
  psql_q ledger "SELECT string_agg(quote_ident(column_name), ',' ORDER BY ordinal_position) FROM information_schema.columns WHERE table_schema='public' AND table_name='${t}' AND NOT (table_name = 'projection_outbox' AND column_name IN ('delivered_at', 'attempts'))" >"${OUT}/cols-progress/${t}"
done 3<"${OUT}/tables.txt"
snapshot ledger before
snapshot ledger before-progress "${OUT}/cols-progress"
docker exec "${PG}" pg_dump -U ledger -Fc -d ledger >"${OUT}/pre-upgrade.dump"
[ -s "${OUT}/pre-upgrade.dump" ] || fail "backup is empty"
psql_q ledger "CREATE DATABASE restored_0012" >/dev/null
docker exec -i "${PG}" pg_restore -U ledger -d restored_0012 --exit-on-error <"${OUT}/pre-upgrade.dump" >"${OUT}/restore.log" 2>&1 || { cat "${OUT}/restore.log"; fail "restore"; }
snapshot restored_0012 restored
same_snapshot before restored "backup restore"
# The restore is a usable rollback target: the previous (Phase-4) server serves it.
server "${OLD}-restored" "${OLD_IMAGE}" restored_0012 "$((PORT+3))" "${validator_env[@]}"
wait_ready "$((PORT+3))" 60 || { docker logs "${OLD}-restored" | tail -20; fail "the previous server does not serve the logically restored backup"; }
docker rm -f "${OLD}-restored" >/dev/null
step "backup: $(du -h "${OUT}/pre-upgrade.dump" | cut -f1) dump restored into restored_0012 with identical rows; the previous server serves it (rollback target)"

if admin "${NEW_IMAGE}" ledger migrate --runtime-role ledger_runtime --projector-role ledger_runtime >"${OUT}/same-role.log" 2>&1; then fail "migrate accepted the runtime role as the projector role"; fi
grep -q "must name distinct roles" "${OUT}/same-role.log" || { cat "${OUT}/same-role.log"; fail "migrate refused the shared role for another reason"; }
[ "$(schema_level ledger)" = "${PREV_SCHEMA}" ] || fail "a refused migrate changed the schema level"
admin "${NEW_IMAGE}" ledger migrate --runtime-role ledger_runtime --projector-role ledger_projector >"${OUT}/upgrade-migrate.log" 2>&1 || { cat "${OUT}/upgrade-migrate.log"; fail "owner migrate to ${NEW_SCHEMA}"; }
admin "${NEW_IMAGE}" ledger migrate --runtime-role ledger_runtime --projector-role ledger_projector >"${OUT}/upgrade-migrate-rerun.log" 2>&1 || { cat "${OUT}/upgrade-migrate-rerun.log"; fail "re-running migrate failed"; }
grep -q "granted projector privileges to role ledger_projector" "${OUT}/upgrade-migrate.log" || fail "migrate did not grant the projector role"
[ "$(schema_level ledger)" = "${NEW_SCHEMA}" ] || fail "upgrade did not reach ${NEW_SCHEMA}"
NEW_CHECKSUMS=$(psql_q ledger "SELECT string_agg(version||':'||encode(checksum,'hex'), ',' ORDER BY version) FROM _sqlx_migrations WHERE version <= ${PREV_SCHEMA}")
[ "${OLD_CHECKSUMS}" = "${NEW_CHECKSUMS}" ] || fail "checksums of previously applied migrations changed"
for f in "${SRC}"/migrations/0*.sql; do
  cmp -s "$f" "migrations/$(basename "$f")" || fail "migration $(basename "$f") differs between ${PREVIOUS} and the working tree"
done
[ "$(psql_q ledger "SELECT encode(checksum,'hex') FROM _sqlx_migrations WHERE version = ${NEW_SCHEMA}")" = "$(sha384sum migrations/0013_diff_and_merge.sql | cut -d' ' -f1)" ] || fail "0013 checksum is not sha384 of the working-tree file"
snapshot ledger after-migrate
same_snapshot before after-migrate "migration 0013 changed pre-existing rows"
# 0013 is additive: no merge row exists until a merge is proposed.
[ "$(psql_q ledger "SELECT count(*) FROM merge_proposals")" = 0 ] || fail "migration 0013 wrote merge rows"
step "owner migrate ${PREV_SCHEMA} -> ${NEW_SCHEMA} (runtime role refused as projector; re-run idempotent), 0001..0012 untouched, no merge rows, all $(wc -l <"${OUT}/tables.txt") pre-upgrade tables byte-identical"

server "${NEW}" "${NEW_IMAGE}" ledger "${PORT}" "${validator_env[@]}"
wait_ready "${PORT}" 60 || { docker logs "${NEW}" | tail -30; fail "Phase-5 server not ready on the upgraded database"; }
[ "$(psql_q ledger "SELECT count(*) FROM pg_stat_activity WHERE usename = 'ledger_runtime'")" -ge 1 ] || fail "Phase-5 server is not connected as the runtime role"
admin "${NEW_IMAGE}" ledger verify >"${OUT}/verify-after-migrate.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/verify-after-migrate.log" || { cat "${OUT}/verify-after-migrate.log"; fail "verify after migrate"; }
calls_before=$(wc -l <"${OUT}/validator-calls.jsonl")
python3 scripts/upgrade-p2/workload.py check "${OUT}/old-writes.json" "${OUT}/check.json"
python3 scripts/upgrade-p3/workload.py replay "${OUT}/recorded.json"
[ "$(wc -l <"${OUT}/validator-calls.jsonl")" = "${calls_before}" ] || fail "reads/replays called the validator"
snapshot ledger after-replay
same_snapshot before after-replay "replay of old idempotency keys changed rows"
step "Phase-5 server ready as ledger_runtime; VERIFY OK; every recorded key (populate, Phase-2 validations, validated acceptances) replays identically; pre-upgrade rows byte-identical"

# --- 3. Merge and projection on the upgraded ledger ---------------------------------------------
python3 scripts/upgrade-p5/workload.py merge "${P_GRAPH}" "${P_TENANT}" "${OUT}/merge.json"
admin "${NEW_IMAGE}" ledger verify >"${OUT}/verify-after-merge.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/verify-after-merge.log" || { cat "${OUT}/verify-after-merge.log"; fail "verify after the merge"; }
# The new projector consumes the old backlog; branch traffic is not a projection backlog.
projector "${PROJ}" ledger run
wait_ready "${MPORT}" 60 || { docker logs "${PROJ}" | tail -30; fail "projector not ready"; }
ok=""; for _ in $(seq 1 240); do
  status_json >"${OUT}/status.json" || true
  python3 -c 'import json,sys; r=json.load(open(sys.argv[1]))["streams"]; sys.exit(0 if r and all(s["lag_versions"]==0 and s["status"]=="active" and s["pending_events"]==0 for s in r) else 1)' "${OUT}/status.json" 2>/dev/null && { ok=1; break; }
  sleep 0.5
done
[ -n "${ok}" ] || { cat "${OUT}/status.json"; docker logs "${PROJ}" | tail -30; fail "the new projector did not consume the old backlog"; }
UNCONFIGURED=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["unconfigured_pending_events"])' "${OUT}/status.json")
EXPECTED_UNCONFIGURED=$(psql_q ledger "SELECT count(*) FROM projection_outbox o WHERE o.delivered_at IS NULL AND o.branch = 'main' AND NOT EXISTS (SELECT 1 FROM projection_state s WHERE s.graph_id = o.graph_id AND s.branch = o.branch AND s.status <> 'disabled')")
BRANCH_ROWS=$(psql_q ledger "SELECT count(*) FROM projection_outbox WHERE graph_id = '${P_GRAPH}' AND branch = 'agent/task-17' AND delivered_at IS NULL")
[ "${UNCONFIGURED}" = "${EXPECTED_UNCONFIGURED}" ] || fail "unconfigured backlog ${UNCONFIGURED} != main-only ${EXPECTED_UNCONFIGURED}"
[ "${BRANCH_ROWS}" = 4 ] || fail "the four branch acceptances did not each leave one undelivered outbox row (${BRANCH_ROWS})"
# Non-main rows without a stream exist (the branch acceptances, the Phase-2 `dev` ref) and are
# not counted: the all-refs figure is strictly larger by exactly those rows.
ALL_REFS_UNCONFIGURED=$(psql_q ledger "SELECT count(*) FROM projection_outbox o WHERE o.delivered_at IS NULL AND NOT EXISTS (SELECT 1 FROM projection_state s WHERE s.graph_id = o.graph_id AND s.branch = o.branch AND s.status <> 'disabled')")
NON_MAIN_UNDELIVERED=$(psql_q ledger "SELECT count(*) FROM projection_outbox WHERE delivered_at IS NULL AND branch <> 'main'")
[ "${ALL_REFS_UNCONFIGURED}" = "$((UNCONFIGURED + NON_MAIN_UNDELIVERED))" ] && [ "${NON_MAIN_UNDELIVERED}" -gt "${BRANCH_ROWS}" ] || fail "non-main backlog accounting (${ALL_REFS_UNCONFIGURED} all refs, ${UNCONFIGURED} main, ${NON_MAIN_UNDELIVERED} non-main)"
curl -sf "http://127.0.0.1:${MPORT}/metrics" >"${OUT}/metrics.txt"
grep -qx "projection_unconfigured_pending ${UNCONFIGURED}" "${OUT}/metrics.txt" || fail "metrics unconfigured backlog is not the main-only figure ${UNCONFIGURED}"
python3 scripts/upgrade-p3/workload.py projection "${OUT}/p3.json" "http://127.0.0.1:${FPORT}/ledger/query"
step "merge on the upgraded ledger: the branch created by the previous release integrated into main (validated, replays identical), VERIFY OK; new projector consumed the old backlog and projected the merge (lag 0, target = accepted main state), ${BRANCH_ROWS} branch outbox rows kept but not counted (unconfigured ${UNCONFIGURED} = main-only; ${NON_MAIN_UNDELIVERED} non-main rows excluded)"

# --- 4. Version skew in both directions ----------------------------------------------------------
docker stop -t 30 "${NEW}" "${PROJ}" >/dev/null
server "${OLD}-ahead" "${OLD_IMAGE}" ledger "$((PORT+1))" "${validator_env[@]}"
rc=$(timeout 90 docker wait "${OLD}-ahead" || echo timeout)
docker logs "${OLD}-ahead" >"${OUT}/skew-ahead.log" 2>&1
[ "${rc}" != timeout ] && [ "${rc}" != 0 ] || fail "previous server did not refuse schema ${NEW_SCHEMA} (exit ${rc})"
grep -q "database schema is at 00${NEW_SCHEMA}, newer than the 00${PREV_SCHEMA} this build supports" "${OUT}/skew-ahead.log" || { cat "${OUT}/skew-ahead.log"; fail "previous server refused for another reason than 'ahead'"; }
server "${NEW}-behind" "${NEW_IMAGE}" restored_0012 "$((PORT+2))" "${validator_env[@]}"
rc2=$(timeout 90 docker wait "${NEW}-behind" || echo timeout)
docker logs "${NEW}-behind" >"${OUT}/skew-behind.log" 2>&1
[ "${rc2}" != timeout ] && [ "${rc2}" != 0 ] || fail "Phase-5 server did not refuse schema ${PREV_SCHEMA} (exit ${rc2})"
grep -q "database schema is at 00${PREV_SCHEMA}; this build requires 00${NEW_SCHEMA}" "${OUT}/skew-behind.log" || { cat "${OUT}/skew-behind.log"; fail "Phase-5 server refused for another reason than 'behind'"; }
projector "${PROJ}-behind" restored_0012 run
rc3=$(timeout 90 docker wait "${PROJ}-behind" || echo timeout)
docker logs "${PROJ}-behind" >"${OUT}/skew-projector-behind.log" 2>&1
[ "${rc3}" != timeout ] && [ "${rc3}" != 0 ] || fail "the projector did not refuse schema ${PREV_SCHEMA} (exit ${rc3})"
grep -q "database schema is at 00${PREV_SCHEMA}; this build requires 00${NEW_SCHEMA}" "${OUT}/skew-projector-behind.log" || { cat "${OUT}/skew-projector-behind.log"; fail "the projector refused schema ${PREV_SCHEMA} for another reason"; }
[ "$(schema_level restored_0012)" = "${PREV_SCHEMA}" ] || fail "restored_0012 changed level"
step "skew: previous server exit ${rc} (ahead); Phase-5 server exit ${rc2} and projector exit ${rc3} (behind)"

# --- 5. Schema convergence: clean 0013 install vs upgraded 0013 ---------------------------------
psql_q ledger "CREATE DATABASE clean_install" >/dev/null
admin "${NEW_IMAGE}" clean_install migrate --runtime-role ledger_runtime --projector-role ledger_projector >"${OUT}/clean-migrate.log" 2>&1 || { cat "${OUT}/clean-migrate.log"; fail "clean install migrate"; }
schema_dump ledger ledger; schema_dump clean_install clean_install
diff -u "${OUT}/schema-clean_install.sql" "${OUT}/schema-ledger.sql" >"${OUT}/schema.diff" || { head -60 "${OUT}/schema.diff" >&2; fail "clean install and upgraded DDL/grants differ"; }
diff -u "${OUT}/owners-clean_install.txt" "${OUT}/owners-ledger.txt" >"${OUT}/owners.diff" || { head -60 "${OUT}/owners.diff" >&2; fail "clean install and upgraded ownership differ"; }
grep -q "CREATE TABLE public.merge_proposals" "${OUT}/schema-ledger.sql" && grep -q "TO ledger_projector" "${OUT}/schema-ledger.sql" || fail "schema dump lacks the Phase-5 objects or projector grants"
step "convergence: clean 0013 and upgraded 0013 identical ($(wc -l <"${OUT}/schema-ledger.sql") DDL/grant lines, $(wc -l <"${OUT}/owners-ledger.txt") owned objects)"
echo "UPGRADE-P5 OK (previous $(cat "${OUT}/previous-rev.txt") schema ${PREV_SCHEMA} -> working tree schema ${NEW_SCHEMA})"
