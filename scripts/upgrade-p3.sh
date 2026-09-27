#!/usr/bin/env bash
# Plan 0007 gate: upgrade qualification from the merged Phase-2 release (schema 0010) to the
# Phase-3 working tree (schema 0011, accepted-state projection).
#
# PREVIOUS (default 0a56092 = main's PR #7 merge) is built from git in a worktree, never from
# the working tree. Its deployment model is reproduced exactly (ADR-0016): the owner runs
# `ledger-admin migrate --runtime-role`, the server connects as the runtime role. Data is
# loaded through the old release's API: the Phase-2 upgrade workload (graphs in two tenants,
# two refs, rejected/pending proposals, named-graph quads on `main`), default-graph-only
# graphs whose history is projectable, and — with the old release's validator configured —
# validation records and validated acceptances. The old release has no projector, so every
# outbox row is an undelivered backlog.
#
# Transition: stop old server -> pg_dump -Fc backup (restored; the old server serves the
# restore) -> operator creates the projector role -> owner applies 0011 with the runtime and
# projector grants (re-run: idempotent) -> Phase-3 server (runtime role) + Fuseki (TDB2,
# deploy/fuseki/ledger-projection.ttl) + ledger-projector (projector role).
#
# Proves, failing loudly: every pre-upgrade row byte-identical after migrate and after
# replaying every recorded key (the outbox's delivery columns aside, which only the projector
# moves, and only forward); 0001–0010 checksums untouched; VERIFY OK; the old backlog is
# consumed after enabling streams: the target holds exactly the accepted `main` state of each
# projectable graph with a marker naming the ref head, every such outbox row is delivered,
# named-graph streams block visibly (NAMED_GRAPH_UNSUPPORTED) and write nothing, other refs
# cannot be enabled and stay pending; a validated acceptance after the upgrade is projected;
# the old server refuses 0011 (ahead); the new server and the new projector refuse 0010
# (behind); clean 0011 install and upgraded 0011 converge (schema-only dump + ownership).
#
# Networking: ledger containers use the host network namespace and bind 127.0.0.1 only;
# PostgreSQL is published on 127.0.0.1:${PGPORT}, Fuseki on 127.0.0.1:${FPORT}. Ports/names
# are distinct from the other qualification scripts and the compose files.
# Usage: scripts/upgrade-p3.sh [previous-git-rev] [commits-per-main-ref]
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker python3 git sha384sum; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
PREVIOUS=${1:-0a56092}
COMMITS=${2:-6}
RUN=$(date -u +%Y%m%dT%H%M%SZ)
OUT="target/upgrade-p3/${RUN}"; mkdir -p "${OUT}"
exec > >(tee "${OUT}/run.log") 2>&1
PROJECT=ledger-qual-upgrade-p3
PG="${PROJECT}-postgres"; OLD="${PROJECT}-previous"; NEW="${PROJECT}-ledger"
FUSEKI="${PROJECT}-fuseki"; PROJ="${PROJECT}-projector"
PGPORT=55436; PORT=18380; VPORT=18390; FPORT=53330; MPORT=19364
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
TARGET_ID=upgrade-p3
AUTH_ISSUER=https://dev-issuer.example/; AUTH_AUDIENCE=api://sculpin-ledger-dev
AUTH_SECRET=development-only-hs256-secret-not-for-production-use
VALIDATOR_SERVICE_ID="urn:upgrade-p3:fake-validator"
export AUTH_ISSUER AUTH_AUDIENCE AUTH_SECRET
VAL_PID=""
T0=$SECONDS; LAST=$SECONDS
step() { local now=$SECONDS; echo "[t+$((now-T0))s, previous step $((now-LAST))s] $*"; echo "$((now-LAST)) $*" >>"${OUT}/durations.txt"; LAST=$now; }
fail() { echo "FAIL: $*" >&2; exit 1; }
cleanup() {
  local rc=$?
  [ -n "${VAL_PID}" ] && kill "${VAL_PID}" 2>/dev/null || true
  for c in "${OLD}" "${NEW}" "${OLD}-ahead" "${NEW}-behind" "${PROJ}-behind" "${OLD}-restored" "${PROJ}" "${FUSEKI}"; do
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
projector() { # name db [args...]
  local name=$1 db=$2; shift 2
  docker run -d --name "${name}" --network host --entrypoint /usr/local/bin/ledger-projector \
    -e LEDGER_PROJECTOR_DATABASE_URL="${PROJECTOR_URL}/${db}?sslmode=disable" -e LEDGER_PROJECTION_TARGET_ID="${TARGET_ID}" \
    -e LEDGER_PROJECTION_QUERY_URL="http://127.0.0.1:${FPORT}/ledger/query" -e LEDGER_PROJECTION_UPDATE_URL="http://127.0.0.1:${FPORT}/ledger/update" \
    -e LEDGER_PROJECTION_USERNAME=admin -e LEDGER_PROJECTION_PASSWORD_FILE=/run/secrets/fuseki-password \
    -v "${PWD}/deploy/fuseki/development-admin-password:/run/secrets/fuseki-password:ro" \
    -e LEDGER_PROJECTOR_DEVELOPMENT=allow-insecure-development-only -e LEDGER_PROJECTOR_ADDR="127.0.0.1:${MPORT}" \
    -e LEDGER_PROJECTOR_POLL_MS=200 -e RUST_LOG=info "${NEW_IMAGE}" "$@" >/dev/null
}
status_json() { admin "${NEW_IMAGE}" ledger projection status --target "${TARGET_ID}" --json 2>/dev/null | tail -1; }
# Ordered, column-explicit dump of every pre-upgrade table (columns as of 0010). $3 names a
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
[ "${PREV_SCHEMA}" = 10 ] && [ "${NEW_SCHEMA}" = 11 ] || fail "expected previous schema 10 and new schema 11, got ${PREV_SCHEMA} -> ${NEW_SCHEMA}"
{ git rev-parse HEAD; git status --porcelain; echo "diff-sha256 $(git diff HEAD | sha256sum | cut -d' ' -f1)"; } >"${OUT}/working-tree.txt"
docker build -q -t "${OLD_IMAGE}" "${SRC}" >"${OUT}/previous-image.txt"
docker build -q -t "${NEW_IMAGE}" . >"${OUT}/new-image.txt"
step "images: previous $(cat "${OUT}/previous-rev.txt") (schema ${PREV_SCHEMA}) = ${OLD_IMAGE} $(cut -c1-19 "${OUT}/previous-image.txt"); working tree $(head -1 "${OUT}/working-tree.txt" | cut -c1-12)+$(($(wc -l <"${OUT}/working-tree.txt")-2)) changes (schema ${NEW_SCHEMA}) = ${NEW_IMAGE} $(cut -c1-19 "${OUT}/new-image.txt"); target ${FUSEKI_IMAGE}"

# --- 1. PostgreSQL and the Phase-2 deployment -------------------------------------------------
docker rm -fv "${PG}" >/dev/null 2>&1 || true
docker run -d --name "${PG}" -p "127.0.0.1:${PGPORT}:5432" -e POSTGRES_USER=ledger -e POSTGRES_PASSWORD=ledger-development-only \
  -e POSTGRES_DB=ledger "${PG_IMAGE}" >/dev/null
i=0; until docker exec "${PG}" pg_isready -h 127.0.0.1 -U ledger -d ledger >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 120 ] || fail "postgres not ready"; sleep 0.5; done
sleep 1; docker exec "${PG}" pg_isready -h 127.0.0.1 -U ledger -d ledger >/dev/null
psql_q ledger "CREATE ROLE ledger_runtime LOGIN PASSWORD 'ledger-runtime-development-only'" >/dev/null
admin "${OLD_IMAGE}" ledger migrate --runtime-role ledger_runtime >"${OUT}/previous-migrate.log" 2>&1 || { cat "${OUT}/previous-migrate.log"; fail "previous release migrate"; }
[ "$(schema_level ledger)" = "${PREV_SCHEMA}" ] || fail "previous release did not migrate to ${PREV_SCHEMA}"
OLD_CHECKSUMS=$(psql_q ledger "SELECT string_agg(version||':'||encode(checksum,'hex'), ',' ORDER BY version) FROM _sqlx_migrations")
STAMP=$(date +%s)
# p2 workload graphs (named-graph quads on main) and projectable graphs; one KB per graph.
GRAPHS="[{\"graph\":\"up3-a1-${STAMP}\",\"tenant\":\"tenant-a\",\"kb\":\"urn:upgrade:kb:a1-${STAMP}\"},{\"graph\":\"up3-b1-${STAMP}\",\"tenant\":\"tenant-b\",\"kb\":\"urn:upgrade:kb:b1-${STAMP}\"},{\"graph\":\"up3-a2-${STAMP}\",\"tenant\":\"tenant-a\",\"kb\":\"urn:upgrade:kb:a2-${STAMP}\"}]"
PGRAPHS="[{\"graph\":\"up3-pa-${STAMP}\",\"tenant\":\"tenant-a\",\"kb\":\"urn:upgrade:kb:pa-${STAMP}\"},{\"graph\":\"up3-pb-${STAMP}\",\"tenant\":\"tenant-b\",\"kb\":\"urn:upgrade:kb:pb-${STAMP}\"}]"
export GRAPHS PGRAPHS BASE="http://127.0.0.1:${PORT}"
python3 -c 'import json,os; [print(g["graph"], g["tenant"], g["kb"]) for g in json.loads(os.environ["GRAPHS"]) + json.loads(os.environ["PGRAPHS"])]' >"${OUT}/graphs.txt"
while read -r g t kb; do
  admin "${OLD_IMAGE}" ledger graph create --graph "$g" --tenant "$t" --status active --kb "$kb" --purpose 'upgrade-p3 qualification' >>"${OUT}/previous-graphs.log" 2>&1 || { cat "${OUT}/previous-graphs.log"; fail "graph create"; }
done <"${OUT}/graphs.txt"
server "${OLD}" "${OLD_IMAGE}" ledger "${PORT}" -e LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only
wait_ready "${PORT}" 60 || { docker logs "${OLD}" | tail -20; fail "previous server not ready"; }
python3 scripts/upgrade-p2/workload.py populate "${OUT}/old-writes.json" "${COMMITS}"
python3 scripts/upgrade-p3/workload.py populate-default "${OUT}/p3.json" "${OUT}/recorded.json" "${COMMITS}"
docker stop -t 30 "${OLD}" >/dev/null; docker rm -f "${OLD}" >/dev/null
python3 scripts/upgrade-p2/fake-validator.py 127.0.0.1 "${VPORT}" "${OUT}/validator-calls.jsonl" >"${OUT}/fake-validator.log" 2>&1 &
VAL_PID=$!
i=0; until python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:${VPORT}/health', timeout=1)" 2>/dev/null; do i=$((i+1)); [ $i -lt 40 ] || fail "fake validator not up"; sleep 0.25; done
server "${OLD}" "${OLD_IMAGE}" ledger "${PORT}" "${validator_env[@]}"
wait_ready "${PORT}" 60 || { docker logs "${OLD}" | tail -20; fail "previous server (validator configured) not ready"; }
python3 scripts/upgrade-p3/workload.py phase2-old "${OUT}/old-writes.json" "${OUT}/p3.json" "${OUT}/recorded.json" "${OUT}/phase2-old.json"
admin "${OLD_IMAGE}" ledger verify >"${OUT}/previous-verify.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/previous-verify.log" || { cat "${OUT}/previous-verify.log"; fail "previous verify before upgrade"; }
psql_q ledger "SELECT 'validation_records '||count(*) FROM validation_records UNION ALL SELECT 'decision_validations '||count(*) FROM decision_validations UNION ALL SELECT 'outbox undelivered '||count(*) FROM projection_outbox WHERE delivered_at IS NULL UNION ALL SELECT 'outbox total '||count(*) FROM projection_outbox UNION ALL SELECT 'ref_events '||count(*) FROM ref_events ORDER BY 1" >"${OUT}/previous-counts.txt"
grep -qx "validation_records 3" "${OUT}/previous-counts.txt" || { cat "${OUT}/previous-counts.txt"; fail "expected 3 validation records on the old release"; }
[ "$(psql_q ledger "SELECT count(*) FROM projection_outbox WHERE delivered_at IS NOT NULL")" = 0 ] || fail "the old release delivered outbox rows"
BACKLOG=$(psql_q ledger "SELECT count(*) FROM projection_outbox")
step "previous release populated through its API: $(tr '\n' ';' <"${OUT}/previous-counts.txt") VERIFY OK"

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
psql_q ledger "CREATE DATABASE restored_0010" >/dev/null
docker exec -i "${PG}" pg_restore -U ledger -d restored_0010 --exit-on-error <"${OUT}/pre-upgrade.dump" >"${OUT}/restore.log" 2>&1 || { cat "${OUT}/restore.log"; fail "restore"; }
snapshot restored_0010 restored
same_snapshot before restored "backup restore"
# The restore is a usable rollback target: the previous (Phase-2) server serves it.
server "${OLD}-restored" "${OLD_IMAGE}" restored_0010 "$((PORT+3))" "${validator_env[@]}"
wait_ready "$((PORT+3))" 60 || { docker logs "${OLD}-restored" | tail -20; fail "the previous server does not serve the logically restored backup"; }
docker rm -f "${OLD}-restored" >/dev/null
step "backup: $(du -h "${OUT}/pre-upgrade.dump" | cut -f1) dump restored into restored_0010 with identical rows; the previous server serves it (rollback target)"

psql_q ledger "CREATE ROLE ledger_projector LOGIN PASSWORD 'ledger-projector-development-only'" >/dev/null
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
[ "$(psql_q ledger "SELECT encode(checksum,'hex') FROM _sqlx_migrations WHERE version = ${NEW_SCHEMA}")" = "$(sha384sum migrations/0011_projection_state.sql | cut -d' ' -f1)" ] || fail "0011 checksum is not sha384 of the working-tree file"
snapshot ledger after-migrate
same_snapshot before after-migrate "migration 0011 changed pre-existing rows"
[ "$(psql_q ledger "SELECT count(*) FROM projection_state")" = 0 ] || fail "0011 enabled a stream by itself"
step "owner migrate ${PREV_SCHEMA} -> ${NEW_SCHEMA} (runtime role refused as projector; re-run idempotent), 0001..0010 untouched, all $(wc -l <"${OUT}/tables.txt") pre-upgrade tables byte-identical"

server "${NEW}" "${NEW_IMAGE}" ledger "${PORT}" "${validator_env[@]}"
wait_ready "${PORT}" 60 || { docker logs "${NEW}" | tail -30; fail "Phase-3 server not ready on the upgraded database"; }
[ "$(psql_q ledger "SELECT count(*) FROM pg_stat_activity WHERE usename = 'ledger_runtime'")" -ge 1 ] || fail "Phase-3 server is not connected as the runtime role"
admin "${NEW_IMAGE}" ledger verify >"${OUT}/verify-after-migrate.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/verify-after-migrate.log" || { cat "${OUT}/verify-after-migrate.log"; fail "verify after migrate"; }
calls_before=$(wc -l <"${OUT}/validator-calls.jsonl")
python3 scripts/upgrade-p2/workload.py check "${OUT}/old-writes.json" "${OUT}/check.json"
python3 scripts/upgrade-p3/workload.py replay "${OUT}/recorded.json"
[ "$(wc -l <"${OUT}/validator-calls.jsonl")" = "${calls_before}" ] || fail "reads/replays called the validator"
snapshot ledger after-replay
same_snapshot before after-replay "replay of old idempotency keys changed rows"
step "Phase-3 server ready as ledger_runtime; VERIFY OK; every recorded key (populate, Phase-2 validations, validated acceptances) replays identically; pre-upgrade rows byte-identical"

# --- 3. Projection of the old backlog -----------------------------------------------------------
docker rm -fv "${FUSEKI}" >/dev/null 2>&1 || true
docker run -d --name "${FUSEKI}" -p "127.0.0.1:${FPORT}:3030" -e ADMIN_PASSWORD="${FUSEKI_PASSWORD}" \
  -v "${PWD}/deploy/fuseki/ledger-projection.ttl:/staging/ledger-projection.ttl:ro" "${FUSEKI_IMAGE}" \
  /jena-fuseki/fuseki-server --config=/staging/ledger-projection.ttl >/dev/null
i=0; until curl -sf "http://127.0.0.1:${FPORT}/\$/ping" >/dev/null 2>&1; do i=$((i+1)); [ $i -lt 120 ] || fail "fuseki not up"; sleep 0.5; done
ALL_MAIN_BEFORE=$(psql_q ledger "SELECT count(*) FROM projection_outbox WHERE branch = 'main'")
while read -r g t kb; do
  admin "${NEW_IMAGE}" ledger projection enable --graph "$g" --target "${TARGET_ID}" >>"${OUT}/enable.log" 2>&1 || { cat "${OUT}/enable.log"; fail "enable ${g}"; }
done <"${OUT}/graphs.txt"
G_DEV=$(head -1 "${OUT}/graphs.txt" | cut -d' ' -f1)
if admin "${NEW_IMAGE}" ledger projection enable --graph "${G_DEV}" --ref dev --target "${TARGET_ID}" >"${OUT}/enable-dev.log" 2>&1; then fail "a non-main ref was enabled"; fi
grep -q '`main` ref only' "${OUT}/enable-dev.log" || { cat "${OUT}/enable-dev.log"; fail "enable of ref dev refused for another reason"; }
projector "${PROJ}" ledger run
wait_ready "${MPORT}" 60 || { docker logs "${PROJ}" | tail -30; fail "projector not ready"; }
[ "$(psql_q ledger "SELECT count(*) FROM pg_stat_activity WHERE usename = 'ledger_projector'")" -ge 1 ] || fail "the projector is not connected as the projector role"
N_P=$(python3 -c 'import json,os; print(len(json.loads(os.environ["PGRAPHS"])))'); N_B=$(python3 -c 'import json,os; print(len(json.loads(os.environ["GRAPHS"])))')
converged=""
for _ in $(seq 1 240); do
  status_json >"${OUT}/status.json" || true
  if python3 - "${OUT}/status.json" "${N_P}" "${N_B}" <<'PY'
import json, os, sys
rows = {r["graph_id"]: r for r in json.loads(open(sys.argv[1]).read() or "{}").get("streams", [])}
proj = [g["graph"] for g in json.loads(os.environ["PGRAPHS"])]
named = [g["graph"] for g in json.loads(os.environ["GRAPHS"])]
ok = all(g in rows and rows[g]["status"] == "active" and rows[g]["lag_versions"] == 0 and rows[g]["pending_events"] == 0 for g in proj)
ok &= all(g in rows and rows[g]["status"] == "blocked" for g in named)
sys.exit(0 if ok else 1)
PY
  then converged=1; break; fi
  sleep 0.5
done
[ -n "${converged}" ] || { cat "${OUT}/status.json"; docker logs "${PROJ}" | tail -30; fail "the projector did not consume the old backlog (projectable streams at lag 0, named-graph streams blocked)"; }
python3 - "${OUT}/status.json" <<'PY' || fail "stream status after consuming the backlog is wrong"
import json, os, sys
rows = {r["graph_id"]: r for r in json.loads(open(sys.argv[1]).read())["streams"]}
for g in json.loads(os.environ["GRAPHS"]):
    r = rows[g["graph"]]
    assert r["last_error_code"] == "NAMED_GRAPH_UNSUPPORTED" and r["projected_ref_version"] is None and r["lag_versions"] > 0, r
for g in json.loads(os.environ["PGRAPHS"]):
    r = rows[g["graph"]]
    assert r["rebuilds"] == 0 and r["consecutive_failures"] == 0 and r["projected_commit"] == r["head_commit"], r
print("status: projectable streams active at lag 0 without rebuilds; named-graph streams blocked with NAMED_GRAPH_UNSUPPORTED")
PY
python3 scripts/upgrade-p3/workload.py projection "${OUT}/p3.json" "http://127.0.0.1:${FPORT}/ledger/query"
while read -r g t kb; do
  case "$g" in up3-p*) docker run --rm --network host --entrypoint /usr/local/bin/ledger-projector \
      -e LEDGER_PROJECTOR_DATABASE_URL="${PROJECTOR_URL}/ledger?sslmode=disable" -e LEDGER_PROJECTION_TARGET_ID="${TARGET_ID}" \
      -e LEDGER_PROJECTION_QUERY_URL="http://127.0.0.1:${FPORT}/ledger/query" -e LEDGER_PROJECTION_UPDATE_URL="http://127.0.0.1:${FPORT}/ledger/update" \
      -e LEDGER_PROJECTION_USERNAME=admin -e LEDGER_PROJECTION_PASSWORD_FILE=/run/secrets/fuseki-password \
      -v "${PWD}/deploy/fuseki/development-admin-password:/run/secrets/fuseki-password:ro" \
      -e LEDGER_PROJECTOR_DEVELOPMENT=allow-insecure-development-only "${NEW_IMAGE}" verify --graph "$g" >"${OUT}/verify-${g}.log" 2>&1 \
      && grep -q "PROJECTION CONSISTENT" "${OUT}/verify-${g}.log" || { cat "${OUT}/verify-${g}.log"; fail "ledger-projector verify ${g}"; } ;;
  esac
done <"${OUT}/graphs.txt"
# The old backlog: every main row of the projectable graphs delivered; named-graph and dev rows
# pending; nothing else about pre-upgrade rows changed.
PGLIST=$(python3 -c "import json,os; print(','.join(\"'%s'\" % g['graph'] for g in json.loads(os.environ['PGRAPHS'])))")
[ "$(psql_q ledger "SELECT count(*) FROM projection_outbox WHERE graph_id IN (${PGLIST}) AND delivered_at IS NULL")" = 0 ] || fail "projectable outbox rows left undelivered"
[ "$(psql_q ledger "SELECT count(*) FROM projection_outbox WHERE (graph_id NOT IN (${PGLIST}) OR branch <> 'main') AND delivered_at IS NOT NULL")" = 0 ] || fail "a blocked or non-main outbox row was marked delivered"
[ "$(psql_q ledger "SELECT count(*) FROM projection_outbox")" = "${BACKLOG}" ] || fail "projection changed the number of outbox rows"
snapshot ledger after-projection "${OUT}/cols-progress"
same_snapshot before-progress after-projection "projection changed pre-upgrade rows beyond the outbox delivery columns"
admin "${NEW_IMAGE}" ledger verify >"${OUT}/verify-after-projection.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/verify-after-projection.log" || { cat "${OUT}/verify-after-projection.log"; fail "verify after projection"; }
step "backlog of ${BACKLOG} outbox rows (${ALL_MAIN_BEFORE} on main): ${N_P} projectable streams projected exactly (verify CONSISTENT), ${N_B} named-graph streams blocked visibly, ref dev refused and pending; pre-upgrade rows unchanged except delivery columns"

# --- 4. Live acceptance after the upgrade is projected ---------------------------------------------
python3 scripts/upgrade-p3/workload.py live "${OUT}/p3.json" "${OUT}/live.json"
LIVE_GRAPH=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["graph"])' "${OUT}/live.json")
LIVE_VERSION=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "${OUT}/live.json")
ok=""
for _ in $(seq 1 120); do
  v=$(status_json | python3 -c 'import json,sys; r=[s for s in json.load(sys.stdin)["streams"] if s["graph_id"]==sys.argv[1]][0]; print(r["projected_ref_version"])' "${LIVE_GRAPH}" 2>/dev/null || true)
  [ "$v" = "${LIVE_VERSION}" ] && { ok=1; break; }; sleep 0.5
done
[ -n "${ok}" ] || fail "the post-upgrade acceptance on ${LIVE_GRAPH} (v${LIVE_VERSION}) was not projected"
python3 scripts/upgrade-p3/workload.py projection "${OUT}/p3.json" "http://127.0.0.1:${FPORT}/ledger/query"
curl -sf "http://127.0.0.1:${MPORT}/metrics" >"${OUT}/metrics.txt"
grep -q "projection_lag_versions{graph=\"${LIVE_GRAPH}\",ref=\"main\",target=\"${TARGET_ID}\",status=\"active\"} 0" "${OUT}/metrics.txt" || fail "metrics do not report zero lag for ${LIVE_GRAPH}"
step "post-upgrade validated acceptance on ${LIVE_GRAPH} projected as v${LIVE_VERSION}; metrics lag 0"

# --- 5. Version skew in both directions -------------------------------------------------------
docker stop -t 30 "${NEW}" "${PROJ}" >/dev/null
server "${OLD}-ahead" "${OLD_IMAGE}" ledger "$((PORT+1))" "${validator_env[@]}"
rc=$(timeout 90 docker wait "${OLD}-ahead" || echo timeout)
docker logs "${OLD}-ahead" >"${OUT}/skew-ahead.log" 2>&1
[ "${rc}" != timeout ] && [ "${rc}" != 0 ] || fail "previous server did not refuse schema ${NEW_SCHEMA} (exit ${rc})"
grep -q "database schema is at 00${NEW_SCHEMA}, newer than the 00${PREV_SCHEMA} this build supports" "${OUT}/skew-ahead.log" || { cat "${OUT}/skew-ahead.log"; fail "previous server refused for another reason than 'ahead'"; }
server "${NEW}-behind" "${NEW_IMAGE}" restored_0010 "$((PORT+2))" "${validator_env[@]}"
rc2=$(timeout 90 docker wait "${NEW}-behind" || echo timeout)
docker logs "${NEW}-behind" >"${OUT}/skew-behind.log" 2>&1
[ "${rc2}" != timeout ] && [ "${rc2}" != 0 ] || fail "Phase-3 server did not refuse schema ${PREV_SCHEMA} (exit ${rc2})"
grep -q "database schema is at 00${PREV_SCHEMA}; this build requires 00${NEW_SCHEMA}" "${OUT}/skew-behind.log" || { cat "${OUT}/skew-behind.log"; fail "Phase-3 server refused for another reason than 'behind'"; }
# A projector started before the owner migrated: a 0010 database has no projector grants,
# so it cannot even read the schema level — refused with the grant instruction. With only
# the migration metadata readable, the refusal is the schema level itself.
projector "${PROJ}-behind" restored_0010 run
rc3=$(timeout 90 docker wait "${PROJ}-behind" || echo timeout)
docker logs "${PROJ}-behind" >"${OUT}/skew-projector-ungranted.log" 2>&1; docker rm -f "${PROJ}-behind" >/dev/null
[ "${rc3}" != timeout ] && [ "${rc3}" != 0 ] || fail "the projector did not refuse an ungranted 0010 database (exit ${rc3})"
grep -q "cannot read the migration metadata.*--projector-role" "${OUT}/skew-projector-ungranted.log" || { cat "${OUT}/skew-projector-ungranted.log"; fail "the projector refused the ungranted 0010 database without the projector grant instruction"; }
psql_q restored_0010 "GRANT SELECT ON public._sqlx_migrations TO ledger_projector" >/dev/null
projector "${PROJ}-behind" restored_0010 run
rc4=$(timeout 90 docker wait "${PROJ}-behind" || echo timeout)
docker logs "${PROJ}-behind" >"${OUT}/skew-projector-behind.log" 2>&1
psql_q restored_0010 "REVOKE SELECT ON public._sqlx_migrations FROM ledger_projector" >/dev/null
[ "${rc4}" != timeout ] && [ "${rc4}" != 0 ] || fail "the projector did not refuse schema ${PREV_SCHEMA} (exit ${rc4})"
grep -q "database schema is at 00${PREV_SCHEMA}; this build requires 00${NEW_SCHEMA}" "${OUT}/skew-projector-behind.log" || { cat "${OUT}/skew-projector-behind.log"; fail "the projector refused schema ${PREV_SCHEMA} for another reason"; }
[ "$(schema_level restored_0010)" = "${PREV_SCHEMA}" ] || fail "restored_0010 changed level"
step "skew: previous server exit ${rc} (ahead); Phase-3 server exit ${rc2} (behind); projector exit ${rc3} (ungranted 0010: grant instruction) and ${rc4} (behind)"

# --- 6. Schema convergence: clean 0011 install vs upgraded 0011 ---------------------------------
psql_q ledger "CREATE DATABASE clean_install" >/dev/null
admin "${NEW_IMAGE}" clean_install migrate --runtime-role ledger_runtime --projector-role ledger_projector >"${OUT}/clean-migrate.log" 2>&1 || { cat "${OUT}/clean-migrate.log"; fail "clean install migrate"; }
schema_dump ledger ledger; schema_dump clean_install clean_install
diff -u "${OUT}/schema-clean_install.sql" "${OUT}/schema-ledger.sql" >"${OUT}/schema.diff" || { head -60 "${OUT}/schema.diff" >&2; fail "clean install and upgraded DDL/grants differ"; }
diff -u "${OUT}/owners-clean_install.txt" "${OUT}/owners-ledger.txt" >"${OUT}/owners.diff" || { head -60 "${OUT}/owners.diff" >&2; fail "clean install and upgraded ownership differ"; }
grep -q "CREATE TABLE public.projection_state" "${OUT}/schema-ledger.sql" && grep -q "TO ledger_projector" "${OUT}/schema-ledger.sql" || fail "schema dump lacks the Phase-3 objects or projector grants"
step "convergence: clean 0011 and upgraded 0011 identical ($(wc -l <"${OUT}/schema-ledger.sql") DDL/grant lines, $(wc -l <"${OUT}/owners-ledger.txt") owned objects)"
echo "UPGRADE-P3 OK (previous $(cat "${OUT}/previous-rev.txt") schema ${PREV_SCHEMA} -> working tree schema ${NEW_SCHEMA})"
