#!/usr/bin/env bash
# Plan 0005 §7: upgrade qualification from the previous release.
# Builds the previous release's image from git (PREVIOUS, default: the merged Phase-1
# baseline f027fbf, schema 0007, owner-URL server that migrates on start-up), loads data
# through ITS API, then upgrades in the documented order (stop old server → `ledger-admin
# migrate --runtime-role` with the owner identity → start the new image with the runtime
# identity) and proves: schema level 0009 with the checksums of the previously applied
# migrations untouched, identical ref heads/versions, identical reconstructed states, verbatim
# replay of the old release's idempotency keys, new writes on top, `ledger-admin verify`
# clean, and clean-install/upgrade schema convergence (pg_dump --schema-only diff empty).
# Usage: scripts/upgrade.sh [previous-git-rev] [commits]
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker curl python3 git; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
PREVIOUS=${1:-f027fbf9f06c6c642dca16a0caf9746571fd76a7}
COMMITS=${2:-25}
RUN=$(date -u +%Y%m%dT%H%M%SZ)
OUT="target/upgrade/${RUN}"; mkdir -p "${OUT}"
PROJECT=ledger-qual-upgrade
NET="${PROJECT}_default"
OLD_IMAGE="cognitive_ledger-previous:${PREVIOUS:0:12}"
OLD_NAME="${PROJECT}-previous"
COMPOSE=(docker compose -p "${PROJECT}" -f compose.yaml)
AUTH_ISSUER=https://dev-issuer.example/; AUTH_AUDIENCE=api://sculpin-ledger-dev
AUTH_SECRET=development-only-hs256-secret-not-for-production-use
export AUTH_ISSUER AUTH_AUDIENCE AUTH_SECRET
cleanup() {
  docker logs "${OLD_NAME}" >"${OUT}/previous-server.log" 2>&1 || true
  docker rm -f "${OLD_NAME}" >/dev/null 2>&1 || true
  docker volume rm "${PROJECT}_prevdata" >/dev/null 2>&1 || true
  "${COMPOSE[@]}" logs --no-color ledger postgres migrate >"${OUT}/containers.log" 2>&1 || true
  "${COMPOSE[@]}" down --remove-orphans --volumes >/dev/null 2>&1 || true
  git worktree remove --force "target/upgrade/previous-src" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# --- 1. Previous release image, built from git (never from the working tree) ----------------
git worktree add --detach "target/upgrade/previous-src" "${PREVIOUS}" >/dev/null 2>&1 || { git worktree remove --force target/upgrade/previous-src; git worktree add --detach "target/upgrade/previous-src" "${PREVIOUS}" >/dev/null; }
git -C target/upgrade/previous-src rev-parse HEAD >"${OUT}/previous-rev.txt"
grep -oE 'REQUIRED_SCHEMA_VERSION: i64 = [0-9]+' target/upgrade/previous-src/crates/ledger-store/src/schema.rs | grep -oE '[0-9]+$' >"${OUT}/previous-schema.txt"
docker build -q -t "${OLD_IMAGE}" target/upgrade/previous-src >"${OUT}/previous-image.txt"
echo "previous release $(cat "${OUT}/previous-rev.txt") (schema $(cat "${OUT}/previous-schema.txt")) built as ${OLD_IMAGE}"

# --- 2. PostgreSQL only, then the OLD server against it (owner URL, migrates itself) -------
"${COMPOSE[@]}" config --quiet
"${COMPOSE[@]}" up -d --wait postgres
OWNER_IN_NET='postgres://ledger:ledger-development-only@postgres:5432/ledger?sslmode=disable'
docker run -d --name "${OLD_NAME}" --network "${NET}" -p 127.0.0.1:8080:8080 -v "${PROJECT}_prevdata:/data" \
  -e LEDGER_ADDR=0.0.0.0:8080 -e LEDGER_DATA_DIR=/data -e LEDGER_DATABASE_URL="${OWNER_IN_NET}" \
  -e LEDGER_AUTH_MODE=dev-hs256 -e LEDGER_AUTH_ISSUER="${AUTH_ISSUER}" -e LEDGER_AUTH_AUDIENCE="${AUTH_AUDIENCE}" \
  -e LEDGER_AUTH_DEV_HS256_SECRET="${AUTH_SECRET}" \
  -e LEDGER_ALLOW_INSECURE_NON_LOOPBACK=allow-insecure-non-loopback-development-only \
  -e LEDGER_UNVALIDATED_ACCEPTANCE=allow-unvalidated-acceptance-development-only \
  "${OLD_IMAGE}" >/dev/null
wait_ready() { local i=0; until curl -fs -o /dev/null "http://127.0.0.1:$1/ready"; do i=$((i+1)); [ "$i" -lt $(( $2 * 5 )) ] || { echo "FAIL: server on $1 not ready" >&2; return 1; }; sleep 0.2; done; }
wait_ready 8080 60
psql_q() { docker exec -i "$("${COMPOSE[@]}" ps -q postgres)" psql -U ledger -d "$1" -tAc "$2"; }
[ "$(psql_q ledger 'SELECT max(version) FROM _sqlx_migrations')" = "$(cat "${OUT}/previous-schema.txt")" ] || { echo "FAIL: previous server did not migrate to its schema" >&2; exit 1; }
OLD_CHECKSUMS=$(psql_q ledger "SELECT string_agg(version||':'||encode(checksum,'hex'), ',' ORDER BY version) FROM _sqlx_migrations")
GRAPH="upgrade-$(date +%s)"; TENANT=tenant-upgrade
docker exec -e LEDGER_DATABASE_URL="${OWNER_IN_NET}" "${OLD_NAME}" ledger-admin graph create --graph "${GRAPH}" --tenant "${TENANT}" --status active --purpose 'upgrade qualification' >/dev/null
mint() { python3 - "$1" "$2" <<'PY'
import base64, hashlib, hmac, json, sys, time, os
def b64(b): return base64.urlsafe_b64encode(b).rstrip(b"=").decode()
now = int(time.time())
claims = {"iss": os.environ["AUTH_ISSUER"], "aud": os.environ["AUTH_AUDIENCE"], "exp": now + 3600, "nbf": now - 30,
          "tid": sys.argv[1], "oid": "upgrade-agent", "sculpin_principal_type": "agent", "roles": sys.argv[2].split(",")}
h = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode()); p = b64(json.dumps(claims).encode())
sig = b64(hmac.new(os.environ["AUTH_SECRET"].encode(), f"{h}.{p}".encode(), hashlib.sha256).digest())
print(f"{h}.{p}.{sig}")
PY
}
TOKEN=$(mint "${TENANT}" "ledger.read,ledger.propose,ledger.review")
export TOKEN GRAPH
# Write COMMITS commits through the OLD API, recording every request (key + body) and answer.
python3 - "${COMMITS}" "${OUT}/old-writes.json" <<'PY'
import json, os, sys, urllib.request
base = "http://127.0.0.1:8080"; token = os.environ["TOKEN"]; g = os.environ["GRAPH"]
def call(method, path, key=None, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method, headers={"authorization": f"Bearer {token}", "content-type": "application/json", **({"idempotency-key": key} if key else {})})
    try:
        with urllib.request.urlopen(req, timeout=60) as r: return r.status, json.load(r)
    except urllib.error.HTTPError as e: return e.code, json.load(e)
records = []; head = None
for i in range(int(sys.argv[1])):
    body = {"ref": "main", "expected_head": head, "operations": [{"op": "add", "quad": f"<urn:upgrade:{i}> <urn:p> \"{i}\" ."}],
            "activity": "upgrade", "event_time": "2026-09-27T00:00:00Z", "evidence_refs": [f"urn:e:{i}"], "source_system": "upgrade.sh", "message": f"commit {i}"}
    s, p = call("POST", f"/v1/graphs/{g}/proposals", f"up-p{i}", body); assert s == 201, (s, p)
    ab = {"ref": "main", "expected_head": head, "reason": "upgrade"}
    s, a = call("POST", f"/v1/graphs/{g}/proposals/{p['candidate']}/accept", f"up-a{i}", ab); assert s == 200, (s, a)
    head = a["head"]
    records.append({"i": i, "prepare_key": f"up-p{i}", "prepare_body": body, "prepare": p, "accept_key": f"up-a{i}", "accept_body": ab, "accept": a})
s, ref = call("GET", f"/v1/graphs/{g}/refs?name=main"); assert s == 200
states = {}
for r in records:
    s, st = call("GET", f"/v1/graphs/{g}/commits/{r['accept']['head']}/state"); assert s == 200
    states[r["accept"]["head"]] = sorted(st["quads"])
json.dump({"ref": ref, "records": records, "states": states}, open(sys.argv[2], "w"))
print(f"previous release: {len(records)} commits written, head {ref['head']} version {ref['version']}")
PY

# --- 3. Upgrade: stop old server, owner migration, new image with the runtime identity ----
docker stop "${OLD_NAME}" >/dev/null
"${COMPOSE[@]}" up --build -d --wait ledger >"${OUT}/upgrade-up.log" 2>&1   # runs migrate first (depends_on)
REQUIRED=$(grep -oE 'REQUIRED_SCHEMA_VERSION: i64 = [0-9]+' crates/ledger-store/src/schema.rs | grep -oE '[0-9]+$')
[ "$(psql_q ledger 'SELECT max(version) FROM _sqlx_migrations')" = "${REQUIRED}" ] || { echo "FAIL: upgrade did not reach schema ${REQUIRED}" >&2; exit 1; }
NEW_CHECKSUMS=$(psql_q ledger "SELECT string_agg(version||':'||encode(checksum,'hex'), ',' ORDER BY version) FROM _sqlx_migrations WHERE version <= $(cat "${OUT}/previous-schema.txt")")
[ "${OLD_CHECKSUMS}" = "${NEW_CHECKSUMS}" ] || { echo "FAIL: checksums of previously applied migrations changed" >&2; exit 1; }
wait_ready 8080 60
echo "upgraded: schema $(cat "${OUT}/previous-schema.txt") -> ${REQUIRED}, previously applied migrations untouched, new server serving as the runtime identity"
[ "$(psql_q ledger "SELECT count(*) FROM pg_stat_activity WHERE usename = 'ledger_runtime'")" -ge 1 ] || { echo "FAIL: new server is not connected as the runtime identity" >&2; exit 1; }

# --- 4. History, reads, replay and new writes through the NEW server -------------------------
python3 - "${OUT}/old-writes.json" <<'PY'
import json, os, sys, urllib.request
base = "http://127.0.0.1:8080"; token = os.environ["TOKEN"]; g = os.environ["GRAPH"]
old = json.load(open(sys.argv[1]))
def call(method, path, key=None, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method, headers={"authorization": f"Bearer {token}", "content-type": "application/json", **({"idempotency-key": key} if key else {})})
    try:
        with urllib.request.urlopen(req, timeout=60) as r: return r.status, json.load(r)
    except urllib.error.HTTPError as e: return e.code, json.load(e)
s, ref = call("GET", f"/v1/graphs/{g}/refs?name=main"); assert s == 200
assert ref["head"] == old["ref"]["head"] and ref["version"] == old["ref"]["version"], (ref, old["ref"])
for h, quads in old["states"].items():
    s, st = call("GET", f"/v1/graphs/{g}/commits/{h}/state"); assert s == 200, (s, st)
    assert sorted(st["quads"]) == quads, f"state of {h} differs after upgrade"
replayed = 0
for r in old["records"]:
    s, p = call("POST", f"/v1/graphs/{g}/proposals", r["prepare_key"], r["prepare_body"]); assert s == 200 and p["replayed"] is True, (s, p)
    assert p["candidate"] == r["prepare"]["candidate"] and p["proposal_id"] == r["prepare"]["proposal_id"]
    s, a = call("POST", f"/v1/graphs/{g}/proposals/{p['candidate']}/accept", r["accept_key"], r["accept_body"]); assert s == 200 and a["replayed"] is True, (s, a)
    assert (a["decision_id"], a["ref_event_id"], a["ref_version"], a["head"]) == (r["accept"]["decision_id"], r["accept"]["ref_event_id"], r["accept"]["ref_version"], r["accept"]["head"])
    replayed += 1
s, ref2 = call("GET", f"/v1/graphs/{g}/refs?name=main"); assert ref2 == ref, "replays moved the ref"
body = {"ref": "main", "expected_head": ref["head"], "operations": [{"op": "add", "quad": "<urn:upgrade:after> <urn:p> \"1\" ."}],
        "activity": "upgrade", "event_time": "2026-09-27T00:00:00Z", "evidence_refs": ["urn:e:after"], "source_system": "upgrade.sh", "message": "after upgrade"}
s, p = call("POST", f"/v1/graphs/{g}/proposals", "up-p-after", body); assert s == 201, (s, p)
s, a = call("POST", f"/v1/graphs/{g}/proposals/{p['candidate']}/accept", "up-a-after", {"ref": "main", "expected_head": ref["head"], "reason": "after"}); assert s == 200, (s, a)
assert a["ref_version"] == ref["version"] + 1
print(f"after upgrade: head and version identical, {len(old['states'])} states identical, {replayed} old prepare/accept keys replayed identically (ref unchanged), new commit accepted as version {a['ref_version']}")
PY
"${COMPOSE[@]}" run --rm migrate verify >"${OUT}/verify.log" 2>&1 && grep -q "VERIFY OK" "${OUT}/verify.log" || { echo "FAIL: verify after upgrade" >&2; cat "${OUT}/verify.log" >&2; exit 1; }

# --- 5. Schema convergence: clean install vs upgraded database --------------------------------
psql_q ledger "CREATE DATABASE clean_install" >/dev/null
"${COMPOSE[@]}" run --rm migrate migrate --runtime-role ledger_runtime --database-url 'postgres://ledger:ledger-development-only@postgres:5432/clean_install?sslmode=disable' >"${OUT}/clean-migrate.log" 2>&1
PG_CID=$("${COMPOSE[@]}" ps -q postgres)
for db in ledger clean_install; do
  docker exec "${PG_CID}" pg_dump -U ledger --schema-only --no-owner -d "${db}" | grep -vE '^(--|SET |SELECT pg_catalog|\\connect|$)' | sed -E 's/[[:space:]]+$//' >"${OUT}/schema-${db}.sql"
done
if diff -u "${OUT}/schema-clean_install.sql" "${OUT}/schema-ledger.sql" >"${OUT}/schema.diff"; then
  echo "schema convergence: clean install and upgraded database have identical schemas and privileges ($(wc -l <"${OUT}/schema-ledger.sql") lines)"
else
  echo "FAIL: clean install and upgraded schemas differ:" >&2; head -60 "${OUT}/schema.diff" >&2; exit 1
fi
echo "report: ${OUT}"
echo "UPGRADE OK"
