#!/usr/bin/env bash
# Plan 0006 merge gate: upgrade qualification from the merged Phase-1.5 release (schema 0009)
# to the Phase-2 working tree (schema 0010, semantic validation).
#
# PREVIOUS (default f81be37 = main's PR #2 merge) is built from git in a worktree, never from
# the working tree. Its deployment model (ADR-0016) is reproduced exactly: the operator creates
# the runtime role, the OWNER runs `ledger-admin migrate --runtime-role`, the server connects
# as the RUNTIME role and never migrates. Data is loaded through the old release's API:
# graphs in two tenants, accepted commits on two refs, rejected and pending proposals,
# decisions, ref events, outbox rows and idempotency records (keys + bodies recorded).
#
# Transition: stop old server -> pg_dump -Fc backup (restored and compared) -> owner applies
# 0010 with the runtime grant (re-run: idempotent reconcile) -> Phase-2 server as runtime,
# with a deterministic fake validator (scripts/upgrade-p2/fake-validator.py) on loopback.
#
# Proves, failing loudly: old rows (commit/patch ids, parents, objects, refs, ref events,
# decisions, outbox, idempotency, proposals, graphs) byte-identical after migrate and after
# replay; every recorded state reconstructs identically; refs identical; old idempotency keys
# replay identically and move nothing; `ledger-admin verify` VERIFY OK; server connected as the
# runtime role; runtime has no UPDATE/DELETE/TRUNCATE on the Phase-2 tables; validation is
# recorded and validated acceptance succeeds on upgraded graphs (unvalidated acceptance is
# refused); the old binary refuses 0010 as ahead; the new binary refuses 0009 as behind;
# migration 0010's pre-existing-validation-id guard refuses and leaves 0009; clean 0010
# install and upgraded 0010 converge (schema-only dump + ownership diff empty).
#
# Networking: every ledger container and the fake validator use the host network namespace
# and bind 127.0.0.1 only (loopback bind, loopback validator URL); PostgreSQL is published on
# 127.0.0.1:${PGPORT}. Ports/names are distinct from scripts/upgrade.sh and the compose files.
# Usage: scripts/upgrade-p2.sh [previous-git-rev] [commits-per-main-ref]
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker python3 git sha384sum; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
PREVIOUS=${1:-f81be37d14b1de102c00fd339a63784b88d725c6}
COMMITS=${2:-6}
RUN=$(date -u +%Y%m%dT%H%M%SZ)
OUT="target/upgrade-p2/${RUN}"; mkdir -p "${OUT}"
exec > >(tee "${OUT}/run.log") 2>&1
PROJECT=ledger-qual-upgrade-p2
PG="${PROJECT}-postgres"; OLD="${PROJECT}-previous"; NEW="${PROJECT}-ledger"
PGPORT=55433; PORT=18080; VPORT=18090
PG_IMAGE='postgres:17.2-bookworm@sha256:3267c505060a0052e5aa6e5175a7b41ab6b04da2f8c4540fc6e98a37210aa2d3'
OLD_IMAGE="cognitive_ledger-previous:${PREVIOUS:0:12}"
NEW_IMAGE="${PROJECT}:working-tree"
SRC="${OUT}/previous-src"   # per run: never touches a worktree this run did not create
SRC_CREATED=""
OWNER_URL="postgres://ledger:ledger-development-only@127.0.0.1:${PGPORT}"
RUNTIME_URL="postgres://ledger_runtime:ledger-runtime-development-only@127.0.0.1:${PGPORT}"
AUTH_ISSUER=https://dev-issuer.example/; AUTH_AUDIENCE=api://sculpin-ledger-dev
AUTH_SECRET=development-only-hs256-secret-not-for-production-use
VALIDATOR_SERVICE_ID="urn:upgrade-p2:fake-validator"
export AUTH_ISSUER AUTH_AUDIENCE AUTH_SECRET
VAL_PID=""
T0=$SECONDS; LAST=$SECONDS
step() { local now=$SECONDS; echo "[t+$((now-T0))s, previous step $((now-LAST))s] $*"; echo "$((now-LAST)) $*" >>"${OUT}/durations.txt"; LAST=$now; }
fail() { echo "FAIL: $*" >&2; exit 1; }
cleanup() {
  local rc=$?
  [ -n "${VAL_PID}" ] && kill "${VAL_PID}" 2>/dev/null || true
  for c in "${OLD}" "${NEW}" "${OLD}-ahead" "${NEW}-behind" "${OLD}-restored"; do
    docker logs "$c" >"${OUT}/container-${c}.log" 2>&1 || true
    docker rm -fv "$c" >/dev/null 2>&1 || true
  done
  docker logs "${PG}" >"${OUT}/container-${PG}.log" 2>&1 || true
  docker rm -fv "${PG}" >/dev/null 2>&1 || true
  # the build worktree is a detached, unmodified checkout created by this run
  [ -n "${SRC_CREATED}" ] && git worktree remove "${SRC}" >/dev/null 2>&1 || true
  echo "exit ${rc}; total $((SECONDS-T0))s; report: ${OUT}"
}
trap cleanup EXIT

psql_q() { docker exec -i "${PG}" psql -v ON_ERROR_STOP=1 -U ledger -d "$1" -tAc "$2"; }
# Run SQL as the runtime role (TCP inside the PostgreSQL container, password auth).
psql_rt() { docker exec -i -e PGPASSWORD=ledger-runtime-development-only "${PG}" psql -h 127.0.0.1 -U ledger_runtime -d "$1" -v ON_ERROR_STOP=1 -tAc "$2"; }
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
# Ordered, column-explicit dump of every pre-upgrade table (columns as of 0009), so rows added
# by 0010 columns cannot hide a change and a new column cannot fake one.
snapshot() { # db label
  local dir="${OUT}/snap-$2"; mkdir -p "${dir}"
  while read -r t <&3; do
    local cols n where=""
    cols=$(cat "${OUT}/cols/${t}"); n=$(tr ',' '\n' <"${OUT}/cols/${t}" | wc -l)
    [ "$t" = "_sqlx_migrations" ] && where="WHERE version <= ${PREV_SCHEMA}"
    psql_q "$1" "COPY (SELECT ${cols} FROM public.${t} ${where} ORDER BY $(seq -s, 1 "${n}")) TO STDOUT" >"${dir}/${t}.tsv"
  done 3<"${OUT}/tables.txt"
}
same_snapshot() { # labelA labelB what
  local na nb; na=$(ls "${OUT}/snap-$1" | wc -l); nb=$(ls "${OUT}/snap-$2" | wc -l)
  [ "${na}" = "$(wc -l <"${OUT}/tables.txt")" ] && [ "${nb}" = "${na}" ] || fail "snapshot $1/$2 incomplete (${na}/${nb} tables)"
  diff -r "${OUT}/snap-$1" "${OUT}/snap-$2" >"${OUT}/snap-$1-vs-$2.diff" || { head -40 "${OUT}/snap-$1-vs-$2.diff" >&2; fail "$3: rows differ ($1 vs $2)"; }
}

# --- 0. Disk guard, images ------------------------------------------------------------------
avail=$(df --output=avail -B1G / | tail -1 | tr -d ' ')
[ "${avail}" -ge 4 ] || fail "only ${avail} GB free on /; need >= 4 GB for the image builds"
[ ! -e "${SRC}" ] || fail "${SRC} already exists"
git worktree add --detach "${SRC}" "${PREVIOUS}" >/dev/null && SRC_CREATED=1
git -C "${SRC}" rev-parse HEAD >"${OUT}/previous-rev.txt"
PREV_SCHEMA=$(grep -oE 'REQUIRED_SCHEMA_VERSION: i64 = [0-9]+' "${SRC}/crates/ledger-store/src/schema.rs" | grep -oE '[0-9]+$')
NEW_SCHEMA=$(grep -oE 'REQUIRED_SCHEMA_VERSION: i64 = [0-9]+' crates/ledger-store/src/schema.rs | grep -oE '[0-9]+$')
echo "${PREV_SCHEMA}" >"${OUT}/previous-schema.txt"; echo "${NEW_SCHEMA}" >"${OUT}/new-schema.txt"
[ "${PREV_SCHEMA}" = 9 ] && [ "${NEW_SCHEMA}" = 10 ] || fail "expected previous schema 9 and new schema 10, got ${PREV_SCHEMA} -> ${NEW_SCHEMA}"
{ git rev-parse HEAD; git status --porcelain; echo "diff-sha256 $(git diff HEAD | sha256sum | cut -d' ' -f1)"; } >"${OUT}/working-tree.txt"
docker build -q -t "${OLD_IMAGE}" "${SRC}" >"${OUT}/previous-image.txt"
docker build -q -t "${NEW_IMAGE}" . >"${OUT}/new-image.txt"
step "images: previous $(cat "${OUT}/previous-rev.txt") (schema ${PREV_SCHEMA}) = ${OLD_IMAGE} $(cut -c1-19 "${OUT}/previous-image.txt"); working tree $(head -1 "${OUT}/working-tree.txt" | cut -c1-12)+$(($(wc -l <"${OUT}/working-tree.txt")-2)) changes (schema ${NEW_SCHEMA}) = ${NEW_IMAGE} $(cut -c1-19 "${OUT}/new-image.txt")"

# --- 1. PostgreSQL and the P1.5 deployment (operator role, owner migrate, runtime server) ----
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
GRAPHS="[{\"graph\":\"up2-a1-${STAMP}\",\"tenant\":\"tenant-a\"},{\"graph\":\"up2-b1-${STAMP}\",\"tenant\":\"tenant-b\"},{\"graph\":\"up2-a2-${STAMP}\",\"tenant\":\"tenant-a\"}]"
export GRAPHS BASE="http://127.0.0.1:${PORT}"
for row in "up2-a1-${STAMP} tenant-a" "up2-b1-${STAMP} tenant-b" "up2-a2-${STAMP} tenant-a"; do
  set -- $row
  admin "${OLD_IMAGE}" ledger graph create --graph "$1" --tenant "$2" --status active --kb "urn:upgrade:kb:$2" --purpose 'upgrade-p2 qualification' >>"${OUT}/previous-graphs.log" 2>&1 || { cat "${OUT}/previous-graphs.log"; fail "graph create"; }
done
server "${OLD}" "${OLD_IMAGE}" ledger "${PORT}" -e LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only
wait_ready "${PORT}" 60 || { docker logs "${OLD}" | tail -20; fail "previous server not ready"; }
[ "$(psql_q ledger "SELECT count(*) FROM pg_stat_activity WHERE usename = 'ledger_runtime'")" -ge 1 ] || fail "previous server is not connected as the runtime role"
step "previous release deployed: owner migrate to ${PREV_SCHEMA}, 3 graphs created by ledger-admin, server as ledger_runtime"

python3 scripts/upgrade-p2/workload.py populate "${OUT}/old-writes.json" "${COMMITS}"
admin "${OLD_IMAGE}" ledger verify >"${OUT}/previous-verify.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/previous-verify.log" || { cat "${OUT}/previous-verify.log"; fail "previous release verify before upgrade"; }
psql_q ledger "SELECT 'decisions '||decision||' '||count(*) FROM decisions GROUP BY decision UNION ALL SELECT 'ref_events '||operation||' '||count(*) FROM ref_events GROUP BY operation UNION ALL SELECT 'projection_outbox '||count(*) FROM projection_outbox UNION ALL SELECT 'idempotency '||operation||' '||count(*) FROM idempotency GROUP BY operation UNION ALL SELECT 'commit_index '||count(*) FROM commit_index UNION ALL SELECT 'proposals '||count(*) FROM proposals UNION ALL SELECT 'refs '||count(*) FROM refs ORDER BY 1" >"${OUT}/previous-counts.txt"
tr '\n' ';' <"${OUT}/previous-counts.txt"; echo
step "populated through the previous API; previous verify OK"

# --- 2. Production-style transition -----------------------------------------------------------
docker stop -t 30 "${OLD}" >/dev/null
[ "$(psql_q ledger "SELECT count(*) FROM pg_stat_activity WHERE usename = 'ledger_runtime'")" = 0 ] || fail "runtime connections remain after stopping the previous server"
mkdir -p "${OUT}/cols"
psql_q ledger "SELECT tablename FROM pg_tables WHERE schemaname='public' ORDER BY 1" >"${OUT}/tables.txt"
while read -r t <&3; do
  psql_q ledger "SELECT string_agg(quote_ident(column_name), ',' ORDER BY ordinal_position) FROM information_schema.columns WHERE table_schema='public' AND table_name='${t}'" >"${OUT}/cols/${t}"
done 3<"${OUT}/tables.txt"
snapshot ledger before
echo "snapshot before: $(for f in "${OUT}"/snap-before/*.tsv; do printf '%s=%s ' "$(basename "$f" .tsv)" "$(wc -l <"$f")"; done)"
docker exec "${PG}" pg_dump -U ledger -Fc -d ledger >"${OUT}/pre-upgrade.dump"
[ -s "${OUT}/pre-upgrade.dump" ] || fail "backup is empty"
docker exec -i "${PG}" pg_restore --list <"${OUT}/pre-upgrade.dump" >"${OUT}/pre-upgrade.dump.list"
ndata=$(grep -c "TABLE DATA public" "${OUT}/pre-upgrade.dump.list" || true)
[ "${ndata}" -ge "$(wc -l <"${OUT}/tables.txt")" ] || fail "backup lists ${ndata} TABLE DATA entries for $(wc -l <"${OUT}/tables.txt") tables"
for db in restored_0009 guard_0009; do
  psql_q ledger "CREATE DATABASE ${db}" >/dev/null
  docker exec -i "${PG}" pg_restore -U ledger -d "${db}" --exit-on-error <"${OUT}/pre-upgrade.dump" >"${OUT}/restore-${db}.log" 2>&1 || { cat "${OUT}/restore-${db}.log"; fail "restore into ${db}"; }
done
snapshot restored_0009 restored
same_snapshot before restored "backup restore"
# The restore is a usable rollback target for the previous release: same DDL, grants and
# ownership as the source, the previous verifier is clean on it, the previous server serves it.
schema_and_owners() { # db label
  docker exec "${PG}" pg_dump -U ledger --schema-only --no-owner -d "$1" | grep -vE '^(--|SET |SELECT pg_catalog|\\connect|\\restrict|\\unrestrict|$)' | sed -E 's/[[:space:]]+$//' >"${OUT}/schema-$2.sql"
  psql_q "$1" "SELECT 'rel|'||relname||'|'||pg_get_userbyid(relowner) FROM pg_class WHERE relnamespace='public'::regnamespace UNION ALL SELECT 'fn|'||proname||'|'||pg_get_userbyid(proowner) FROM pg_proc WHERE pronamespace='public'::regnamespace UNION ALL SELECT 'schema|public|'||pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='public' ORDER BY 1" >"${OUT}/owners-$2.txt"
}
schema_and_owners ledger source-0009
schema_and_owners restored_0009 restored-0009
diff -u "${OUT}/schema-source-0009.sql" "${OUT}/schema-restored-0009.sql" >"${OUT}/restore-schema.diff" || true
# A logical restore re-parses CHECK text: PostgreSQL flattens the nested AND of the three
# BETWEEN-style *_branch_bounds CHECKs (identical meaning; the Phase-2 verifier accepts exactly
# that form). Anything else differing is a failure.
# Exactly these replacements, and nothing else: each nested form removed, its flattened form added.
python3 - "${OUT}/restore-schema.diff" <<'PY' || { head -40 "${OUT}/restore-schema.diff" >&2; fail "restored backup DDL/grants differ from the source beyond the re-parsed branch bounds"; }
import sys
lines = {l.rstrip("\n") for l in open(sys.argv[1]) if l[:1] in "+-" and not l.startswith(("+++", "---"))}
bounds = "(octet_length(branch) >= 1) AND (octet_length(branch) <= 128)"
regex = "(branch ~ '^[A-Za-z0-9._/-]+$'::text)"
tables = ("proposals", "ref_events", "refs")
nested = {f"-    CONSTRAINT {t}_branch_bounds CHECK ((({bounds}) AND {regex}))," for t in tables}
flat = {f"+    CONSTRAINT {t}_branch_bounds CHECK (({bounds} AND {regex}))," for t in tables}
if lines != nested | flat:
    print("unexpected restore DDL difference:", sorted(lines ^ (nested | flat)), file=sys.stderr)
    sys.exit(1)
print("restore DDL: exactly the 3 re-parsed branch-bound CHECKs differ")
PY
flattened=3
diff -u "${OUT}/owners-source-0009.txt" "${OUT}/owners-restored-0009.txt" >"${OUT}/restore-owners.diff" || { head -40 "${OUT}/restore-owners.diff" >&2; fail "restored backup ownership differs from the source"; }
admin "${OLD_IMAGE}" restored_0009 verify >"${OUT}/restore-verify-previous.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/restore-verify-previous.log" || { cat "${OUT}/restore-verify-previous.log"; fail "previous ledger-admin verify on the restored backup"; }
# Observation (known P1.5 defect, fixed in Phase 2): the released P1.5 server's strict CHECK
# deparse refuses a logically restored database; P1.5 rollback must use a physical backup.
server "${OLD}-restored" "${OLD_IMAGE}" restored_0009 "$((PORT+3))" -e LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only
if wait_ready "$((PORT+3))" 20; then echo "NOTE: previous server serves the logically restored backup"; else echo "NOTE: previous server refuses the logically restored backup ($(docker logs "${OLD}-restored" 2>&1 | grep -o 'constraint [a-z_]* on public.[a-z_]*' | head -1)); P1.5 rollback needs a physical backup"; fi
docker rm -f "${OLD}-restored" >/dev/null
step "backup: $(du -h "${OUT}/pre-upgrade.dump" | cut -f1) custom-format dump, ${ndata} table-data entries, restored into restored_0009 and guard_0009 with identical rows, DDL/grants/ownership identical to the source except the ${flattened} re-parsed branch-bound CHECKs, previous verify VERIFY OK on it"

admin "${NEW_IMAGE}" ledger migrate --runtime-role ledger_runtime >"${OUT}/upgrade-migrate.log" 2>&1 || { cat "${OUT}/upgrade-migrate.log"; fail "owner migrate to ${NEW_SCHEMA}"; }
admin "${NEW_IMAGE}" ledger migrate --runtime-role ledger_runtime >"${OUT}/upgrade-migrate-rerun.log" 2>&1 || { cat "${OUT}/upgrade-migrate-rerun.log"; fail "re-running migrate (grant reconcile) failed"; }
[ "$(schema_level ledger)" = "${NEW_SCHEMA}" ] || fail "upgrade did not reach schema ${NEW_SCHEMA}"
NEW_CHECKSUMS=$(psql_q ledger "SELECT string_agg(version||':'||encode(checksum,'hex'), ',' ORDER BY version) FROM _sqlx_migrations WHERE version <= ${PREV_SCHEMA}")
[ "${OLD_CHECKSUMS}" = "${NEW_CHECKSUMS}" ] || fail "checksums of previously applied migrations changed"
for f in "${SRC}"/migrations/0*.sql; do
  v=$(basename "$f" | cut -d_ -f1 | sed 's/^0*//'); sum=$(sha384sum "$f" | cut -d' ' -f1)
  grep -q "\b${v}:${sum}\b" <<<"${NEW_CHECKSUMS}" || fail "recorded checksum of migration ${v} is not sha384 of the previous release's file"
  cmp -s "$f" "migrations/$(basename "$f")" || fail "migration $(basename "$f") differs between ${PREVIOUS} and the working tree"
done
[ "$(psql_q ledger "SELECT encode(checksum,'hex') FROM _sqlx_migrations WHERE version = ${NEW_SCHEMA}")" = "$(sha384sum migrations/0010_semantic_validation.sql | cut -d' ' -f1)" ] || fail "0010 checksum is not sha384 of the working-tree file"
snapshot ledger after-migrate
same_snapshot before after-migrate "migration 0010 changed pre-existing rows"
step "owner migrate ${PREV_SCHEMA} -> ${NEW_SCHEMA} (re-run idempotent), 0001..0009 checksums untouched, all $(wc -l <"${OUT}/tables.txt") pre-upgrade tables byte-identical"

python3 scripts/upgrade-p2/fake-validator.py 127.0.0.1 "${VPORT}" "${OUT}/validator-calls.jsonl" >"${OUT}/fake-validator.log" 2>&1 &
VAL_PID=$!
i=0; until python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:${VPORT}/health', timeout=1)" 2>/dev/null; do i=$((i+1)); [ $i -lt 40 ] || fail "fake validator not up"; sleep 0.25; done
server "${NEW}" "${NEW_IMAGE}" ledger "${PORT}" -e LEDGER_VALIDATOR_URL="http://127.0.0.1:${VPORT}/validate" -e LEDGER_VALIDATOR_SERVICE_ID="${VALIDATOR_SERVICE_ID}"
wait_ready "${PORT}" 60 || { docker logs "${NEW}" | tail -30; fail "Phase-2 server not ready on the upgraded database"; }
[ "$(psql_q ledger "SELECT count(*) FROM pg_stat_activity WHERE usename = 'ledger_runtime'")" -ge 1 ] || fail "Phase-2 server is not connected as the runtime role"
[ "$(psql_q ledger "SELECT count(*) FROM pg_stat_activity WHERE usename = 'ledger' AND pid <> pg_backend_pid() AND backend_type = 'client backend'")" = 0 ] || fail "an owner connection is open besides this probe"
docker logs "${NEW}" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' >"${OUT}/new-server-start.log"
grep -qE "semantic validation (service|endpoint) configured" "${OUT}/new-server-start.log" && grep -q "service_id=${VALIDATOR_SERVICE_ID}" "${OUT}/new-server-start.log" || fail "Phase-2 server did not configure the validation service ${VALIDATOR_SERVICE_ID}"
admin "${NEW_IMAGE}" ledger verify >"${OUT}/verify-after-migrate.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/verify-after-migrate.log" || { cat "${OUT}/verify-after-migrate.log"; fail "verify after migrate"; }
step "Phase-2 server ready as ledger_runtime with the validator configured; ledger-admin verify: $(grep -c '^ok' "${OUT}/verify-after-migrate.log") checks ok, VERIFY OK"

# --- 3. History, reads and replay through the NEW server ---------------------------------------
python3 scripts/upgrade-p2/workload.py check "${OUT}/old-writes.json" "${OUT}/check.json"
snapshot ledger after-replay
same_snapshot before after-replay "replay of old idempotency keys changed rows"
[ ! -s "${OUT}/validator-calls.jsonl" ] || fail "reads/replays called the validator"
step "reads + replay: pre-upgrade tables still byte-identical after replaying every old key"

# --- 4. Phase-2 validation and validated acceptance on upgraded graphs ------------------------
python3 scripts/upgrade-p2/workload.py phase2 "${OUT}/old-writes.json" "${OUT}/phase2.json"
calls=$(wc -l <"${OUT}/validator-calls.jsonl")
[ "${calls}" = 2 ] || fail "expected exactly 2 validator calls (replays must not call it), got ${calls}"
python3 - "${OUT}/validator-calls.jsonl" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    c = json.loads(line)
    assert c["answer"] == 200 and c["protocol"] == "sculpin-validation-request/v1" and c["quads"] > 0, c
    # One logical-invocation identifier in two representations (ADR-0019 amendment).
    assert c["invocation_id"] and c["invocation_id"].startswith("sha256:"), c
    assert c["idempotency_key"] == c["invocation_id"], c
print("validator saw: " + "; ".join(f"{json.loads(l)['commit'][:15]}… keys={json.loads(l)['request_keys']} idem={json.loads(l)['idempotency_key']}" for l in open(sys.argv[1])))
PY
psql_q ledger "SELECT 'validation_records '||count(*) FROM validation_records UNION ALL SELECT 'semantic_execution_contexts '||count(*) FROM semantic_execution_contexts UNION ALL SELECT 'decision_validations '||count(*) FROM decision_validations UNION ALL SELECT 'idempotency validate '||count(*) FROM idempotency WHERE operation='validate'" >"${OUT}/phase2-counts.txt"
grep -qx "validation_records 2" "${OUT}/phase2-counts.txt" && grep -qx "decision_validations 2" "${OUT}/phase2-counts.txt" && grep -qx "idempotency validate 2" "${OUT}/phase2-counts.txt" || { cat "${OUT}/phase2-counts.txt"; fail "Phase-2 rows not as expected"; }
snapshot ledger after-phase2
python3 - "${OUT}" <<'PY'
import os, sys
out = sys.argv[1]; changed = []
for f in sorted(os.listdir(f"{out}/snap-before")):
    before = open(f"{out}/snap-before/{f}").read().splitlines(); after = set(open(f"{out}/snap-after-phase2/{f}").read().splitlines())
    missing = [l for l in before if l not in after]
    if missing and f != "refs.tsv":   # refs is the one mutable table (CAS head/version)
        raise SystemExit(f"FAIL: {len(missing)} pre-upgrade row(s) of {f} changed after Phase-2 writes: {missing[:2]}")
    if missing: changed.append(f"{f}: {len(missing)} row(s) moved")
    if f == "refs.tsv" and len(missing) != 2:
        raise SystemExit(f"FAIL: expected exactly 2 refs (the two graphs advanced by Phase-2 acceptance) to move, got {len(missing)}")
print("append-only tables keep every pre-upgrade row after Phase-2 writes; " + "; ".join(changed))
PY
admin "${NEW_IMAGE}" ledger verify >"${OUT}/verify-after-phase2.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/verify-after-phase2.log" || { cat "${OUT}/verify-after-phase2.log"; fail "verify after Phase-2 writes"; }
step "Phase-2: $(tr '\n' ';' <"${OUT}/phase2-counts.txt") VERIFY OK after the new writes"

# --- 5. Runtime least privilege on the new tables ------------------------------------------------
probe_denied() { # description sql
  if psql_rt ledger "$2" >"${OUT}/probe.out" 2>&1; then fail "runtime role could $1"; fi
  grep -q "permission denied" "${OUT}/probe.out" || { cat "${OUT}/probe.out"; fail "runtime $1 failed for another reason than a privilege denial"; }
  echo "denied: $1 ($(grep -o 'permission denied[^"]*' "${OUT}/probe.out" | head -1))" >>"${OUT}/privilege-probes.txt"
}
: >"${OUT}/privilege-probes.txt"
for t in semantic_execution_contexts semantic_virtual_contexts validation_records validation_violations decision_validations; do
  col=$(psql_q ledger "SELECT column_name FROM information_schema.columns WHERE table_schema='public' AND table_name='${t}' ORDER BY ordinal_position LIMIT 1")
  probe_denied "UPDATE ${t}" "UPDATE public.${t} SET ${col} = ${col}"
  probe_denied "DELETE FROM ${t}" "DELETE FROM public.${t}"
  probe_denied "TRUNCATE ${t}" "TRUNCATE public.${t}"
done
probe_denied "UPDATE idempotency.result_validation_id" "UPDATE public.idempotency SET result_validation_id = NULL"
probe_denied "INSERT validation_records.created_at" "INSERT INTO public.validation_records (validation_id, created_at) VALUES ('x', now())"
probe_denied "CREATE TABLE in public" "CREATE TABLE public.p2_probe (x int)"
[ "$(psql_q ledger "SELECT bool_or(has_any_column_privilege('ledger_runtime', 'public.'||t, 'UPDATE') OR has_table_privilege('ledger_runtime', 'public.'||t, 'DELETE,TRUNCATE,REFERENCES,TRIGGER')) FROM unnest(ARRAY['semantic_execution_contexts','semantic_virtual_contexts','validation_records','validation_violations','decision_validations']) t")" = f ] || fail "runtime holds UPDATE/DELETE/TRUNCATE/REFERENCES/TRIGGER on a Phase-2 table"
[ "$(psql_q ledger "SELECT rolsuper OR rolcreaterole OR rolcreatedb OR rolbypassrls FROM pg_roles WHERE rolname='ledger_runtime'")" = f ] || fail "runtime role has elevated attributes"
[ "$(psql_rt ledger "SELECT count(*) FROM validation_records")" = 2 ] || fail "runtime cannot read validation_records"
step "least privilege: $(wc -l <"${OUT}/privilege-probes.txt") runtime write probes denied by privilege; runtime reads the new tables"

# --- 6. Version skew in both directions -------------------------------------------------------
docker stop -t 30 "${NEW}" >/dev/null
server "${OLD}-ahead" "${OLD_IMAGE}" ledger "$((PORT+1))" -e LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only
rc=$(timeout 90 docker wait "${OLD}-ahead" || echo timeout)
docker logs "${OLD}-ahead" >"${OUT}/skew-ahead.log" 2>&1
[ "${rc}" != timeout ] && [ "${rc}" != 0 ] || fail "previous server did not refuse schema ${NEW_SCHEMA} (exit ${rc})"
grep -q "database schema is at 00${NEW_SCHEMA}, newer than the 000${PREV_SCHEMA} this build supports" "${OUT}/skew-ahead.log" || { cat "${OUT}/skew-ahead.log"; fail "previous server refused for another reason than 'ahead'"; }
# Observation only (not a Plan 0006 property): the released P1.5 `ledger-admin verify` checks
# invariants, not the schema level, so it may report on a newer schema; recorded, not asserted.
if admin "${OLD_IMAGE}" ledger verify >"${OUT}/skew-ahead-verify.log" 2>&1; then echo "NOTE: previous ledger-admin verify ($(tail -1 "${OUT}/skew-ahead-verify.log")) does not refuse schema ${NEW_SCHEMA}"; else echo "NOTE: previous ledger-admin verify refuses schema ${NEW_SCHEMA}"; fi
server "${NEW}-behind" "${NEW_IMAGE}" restored_0009 "$((PORT+2))" -e LEDGER_VALIDATOR_URL="http://127.0.0.1:${VPORT}/validate" -e LEDGER_VALIDATOR_SERVICE_ID="${VALIDATOR_SERVICE_ID}"
rc2=$(timeout 90 docker wait "${NEW}-behind" || echo timeout)
docker logs "${NEW}-behind" >"${OUT}/skew-behind.log" 2>&1
[ "${rc2}" != timeout ] && [ "${rc2}" != 0 ] || fail "Phase-2 server did not refuse schema ${PREV_SCHEMA} (exit ${rc2})"
grep -q "database schema is at 000${PREV_SCHEMA}; this build requires 00${NEW_SCHEMA}" "${OUT}/skew-behind.log" || { cat "${OUT}/skew-behind.log"; fail "Phase-2 server refused for another reason than 'behind'"; }
if admin "${NEW_IMAGE}" restored_0009 verify >"${OUT}/skew-behind-verify.log" 2>&1; then fail "Phase-2 ledger-admin verify accepted schema ${PREV_SCHEMA}"; fi
[ "$(schema_level restored_0009)" = "${PREV_SCHEMA}" ] || fail "restored_0009 changed level"
step "skew: previous server exit ${rc} ('$(grep -o 'database schema is at [^:]*' "${OUT}/skew-ahead.log" | head -1)'); Phase-2 server exit ${rc2} ('$(grep -o 'database schema is at [^:]*' "${OUT}/skew-behind.log" | head -1)')"

# --- 7. Migration 0010's pre-existing-validation-id guard --------------------------------------
psql_q guard_0009 "INSERT INTO decisions (proposal_id, graph_id, branch, candidate_commit, decision, tenant_id, principal_id, principal_type, reason, validation_ids)
  SELECT p.proposal_id, p.graph_id, p.branch, p.candidate_commit, 'rejected', p.tenant_id, p.principal_id, p.principal_type, 'guard precondition', ARRAY['sha256:$(printf '0%.0s' $(seq 64))']
  FROM proposals p WHERE NOT EXISTS (SELECT 1 FROM decisions d WHERE d.proposal_id = p.proposal_id OR d.candidate_commit = p.candidate_commit) ORDER BY p.proposal_id LIMIT 1" >/dev/null
[ "$(psql_q guard_0009 "SELECT count(*) FROM decisions WHERE cardinality(validation_ids) > 0")" = 1 ] || fail "guard precondition not constructed"
if admin "${NEW_IMAGE}" guard_0009 migrate --runtime-role ledger_runtime >"${OUT}/guard-migrate.log" 2>&1; then fail "migration 0010 applied although a decision cites validation ids"; fi
grep -q "migration 0010: 1 decision(s) cite validation ids before any validation record existed; refusing to upgrade" "${OUT}/guard-migrate.log" || { cat "${OUT}/guard-migrate.log"; fail "0010 refused for another reason than its guard"; }
[ "$(schema_level guard_0009)" = "${PREV_SCHEMA}" ] || fail "guard database left at $(schema_level guard_0009)"
[ "$(psql_q guard_0009 "SELECT to_regclass('public.validation_records') IS NULL AND to_regclass('public.semantic_execution_contexts') IS NULL AND to_regclass('public.semantic_virtual_contexts') IS NULL AND to_regclass('public.validation_violations') IS NULL AND to_regclass('public.decision_validations') IS NULL AND NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name='idempotency' AND column_name='result_validation_id') AND NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname IN ('decisions_identity','idempotency_validation_fk','idempotency_validation_shape'))")" = t ] || fail "guard refusal left partial 0010 objects"
schema_and_owners guard_0009 guard-after-refusal
# the only difference to the restored copy is the one precondition row, not the schema
diff -u "${OUT}/schema-restored-0009.sql" "${OUT}/schema-guard-after-refusal.sql" >"${OUT}/guard-schema.diff" || { head -40 "${OUT}/guard-schema.diff" >&2; fail "refused 0010 changed the 0009 schema"; }
step "0010 guard: refused ('$(grep -o 'migration 0010: [^;]*' "${OUT}/guard-migrate.log" | head -1)'), database left at ${PREV_SCHEMA} with no 0010 objects"

# --- 8. Schema convergence: clean 0010 install vs upgraded 0010 ---------------------------------
psql_q ledger "CREATE DATABASE clean_install" >/dev/null
admin "${NEW_IMAGE}" clean_install migrate --runtime-role ledger_runtime >"${OUT}/clean-migrate.log" 2>&1 || { cat "${OUT}/clean-migrate.log"; fail "clean install migrate"; }
for db in ledger clean_install; do
  docker exec "${PG}" pg_dump -U ledger --schema-only --no-owner -d "${db}" | grep -vE '^(--|SET |SELECT pg_catalog|\\connect|\\restrict|\\unrestrict|$)' | sed -E 's/[[:space:]]+$//' >"${OUT}/schema-${db}.sql"
  psql_q "${db}" "SELECT 'rel|'||relname||'|'||pg_get_userbyid(relowner) FROM pg_class WHERE relnamespace='public'::regnamespace UNION ALL SELECT 'fn|'||proname||'|'||pg_get_userbyid(proowner) FROM pg_proc WHERE pronamespace='public'::regnamespace UNION ALL SELECT 'schema|public|'||pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='public' ORDER BY 1" >"${OUT}/owners-${db}.txt"
done
diff -u "${OUT}/schema-clean_install.sql" "${OUT}/schema-ledger.sql" >"${OUT}/schema.diff" || { head -60 "${OUT}/schema.diff" >&2; fail "clean install and upgraded DDL/grants differ"; }
diff -u "${OUT}/owners-clean_install.txt" "${OUT}/owners-ledger.txt" >"${OUT}/owners.diff" || { head -60 "${OUT}/owners.diff" >&2; fail "clean install and upgraded object ownership differ"; }
grep -q "CREATE TABLE public.validation_records" "${OUT}/schema-ledger.sql" || fail "schema dump lacks the Phase-2 tables"
step "convergence: clean 0010 and upgraded 0010 identical ($(wc -l <"${OUT}/schema-ledger.sql") DDL/grant lines, $(wc -l <"${OUT}/owners-ledger.txt") owned objects)"
echo "UPGRADE-P2 OK (previous $(cat "${OUT}/previous-rev.txt") schema ${PREV_SCHEMA} -> working tree schema ${NEW_SCHEMA})"
