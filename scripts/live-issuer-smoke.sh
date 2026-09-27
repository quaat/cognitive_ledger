#!/usr/bin/env bash
# Plan 0005 §4: live identity-provider smoke test (Entra ID or any OIDC issuer).
# Runs ledger-server in production authentication mode (LEDGER_AUTH_MODE=oidc) against the
# real issuer/JWKS configuration and exercises an externally issued bearer token end to end:
# issuer, audience, signature/JWKS, tenant mapping, principal mapping, principal-type policy
# and role/capability mapping. No token is ever committed, printed or logged; it is read from
# the environment (LEDGER_LIVE_TOKEN) or a file (LEDGER_LIVE_TOKEN_FILE) and passed to curl
# through a header file. Acceptance is never enabled: prepare is the deepest write exercised.
# Production authentication requires a validator trust anchor (ADR-0019 amendment); the
# smoke test names one without an endpoint (validation is not exercised here).
#
# Required environment:
#   LEDGER_LIVE_ISSUER        e.g. https://login.microsoftonline.com/<tenant-id>/v2.0
#   LEDGER_LIVE_AUDIENCE      the application id URI or client id the token is issued for
#   LEDGER_LIVE_JWKS_URL      e.g. https://login.microsoftonline.com/<tenant-id>/discovery/v2.0/keys
#   LEDGER_LIVE_TOKEN | LEDGER_LIVE_TOKEN_FILE   a fresh bearer token for that audience
#   LEDGER_LIVE_TENANT        the tenant claim value (Entra: tid) the token carries
#   LEDGER_LIVE_PRINCIPAL     the principal claim value (Entra: oid) the token carries
# Optional (claims policy, same names as the server):
#   LEDGER_AUTH_TENANT_CLAIM LEDGER_AUTH_PRINCIPAL_CLAIM LEDGER_AUTH_PRINCIPAL_TYPE_CLAIM
#   LEDGER_AUTH_ROLES_CLAIM LEDGER_AUTH_ROLE_MAP LEDGER_AUTH_AGENT_CLIENT_IDS LEDGER_AUTH_SERVICE_CLIENT_IDS
#   LEDGER_LIVE_EXPECT_PRINCIPAL_TYPE (human|agent|service; default human)
#   LEDGER_LIVE_EXPECT_PROPOSE=1  when the token's roles map to `propose` (a prepare is then required to succeed)
# Result line: LIVE_ISSUER=PASS | LIVE_ISSUER=FAIL | LIVE_ISSUER=PENDING_EXTERNAL (no token/config)
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in docker curl python3; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 69; }; done
OUT="target/live-issuer/$(date -u +%Y%m%dT%H%M%SZ)"; mkdir -p "${OUT}"
TOKEN_FILE="${OUT}/.token"   # header file for curl, mode 600, removed on exit
if [ -n "${LEDGER_LIVE_TOKEN_FILE:-}" ]; then TOKEN=$(tr -d '\n' <"${LEDGER_LIVE_TOKEN_FILE}"); else TOKEN=${LEDGER_LIVE_TOKEN:-}; fi
if [ -z "${TOKEN}" ] || [ -z "${LEDGER_LIVE_ISSUER:-}" ] || [ -z "${LEDGER_LIVE_AUDIENCE:-}" ] || [ -z "${LEDGER_LIVE_JWKS_URL:-}" ] || [ -z "${LEDGER_LIVE_TENANT:-}" ] || [ -z "${LEDGER_LIVE_PRINCIPAL:-}" ]; then
  echo "LIVE_ISSUER=PENDING_EXTERNAL (issuer, audience, JWKS URL, tenant, principal and a token are required; nothing was run)"
  exit 3
fi
umask 077; printf 'header = "authorization: Bearer %s"\n' "${TOKEN}" >"${TOKEN_FILE}"; unset TOKEN
PROJECT=ledger-qual-live
COMPOSE=(docker compose -p "${PROJECT}" -f compose.yaml)
cleanup() { rm -f "${TOKEN_FILE}"; "${COMPOSE[@]}" logs --no-color ledger >"${OUT}/server.log" 2>&1 || true; "${COMPOSE[@]}" down --remove-orphans --volumes >/dev/null 2>&1 || true; }
trap cleanup EXIT
"${COMPOSE[@]}" config --quiet
"${COMPOSE[@]}" up -d --wait postgres
"${COMPOSE[@]}" run --rm migrate migrate --runtime-role ledger_runtime >/dev/null
# Production authentication: oidc against the live issuer; loopback bind (no insecure switch);
# acceptance stays fail-closed (no LEDGER_UNVALIDATED_ACCEPTANCE).
docker run -d --name "${PROJECT}-server" --network "${PROJECT}_default" -p 127.0.0.1:8080:8080 \
  -e LEDGER_ADDR=0.0.0.0:8080 -e LEDGER_DATABASE_URL='postgres://ledger_runtime:ledger-runtime-development-only@postgres:5432/ledger?sslmode=disable' \
  -e LEDGER_VALIDATOR_SERVICE_ID="${LEDGER_LIVE_VALIDATOR_SERVICE_ID:-urn:sculpin:service:live-smoke-no-endpoint}" \
  -e LEDGER_AUTH_MODE=oidc -e LEDGER_AUTH_ISSUER="${LEDGER_LIVE_ISSUER}" -e LEDGER_AUTH_AUDIENCE="${LEDGER_LIVE_AUDIENCE}" -e LEDGER_AUTH_JWKS_URL="${LEDGER_LIVE_JWKS_URL}" \
  ${LEDGER_AUTH_TENANT_CLAIM:+-e LEDGER_AUTH_TENANT_CLAIM="${LEDGER_AUTH_TENANT_CLAIM}"} ${LEDGER_AUTH_PRINCIPAL_CLAIM:+-e LEDGER_AUTH_PRINCIPAL_CLAIM="${LEDGER_AUTH_PRINCIPAL_CLAIM}"} \
  ${LEDGER_AUTH_PRINCIPAL_TYPE_CLAIM:+-e LEDGER_AUTH_PRINCIPAL_TYPE_CLAIM="${LEDGER_AUTH_PRINCIPAL_TYPE_CLAIM}"} ${LEDGER_AUTH_ROLES_CLAIM:+-e LEDGER_AUTH_ROLES_CLAIM="${LEDGER_AUTH_ROLES_CLAIM}"} \
  ${LEDGER_AUTH_ROLE_MAP:+-e LEDGER_AUTH_ROLE_MAP="${LEDGER_AUTH_ROLE_MAP}"} ${LEDGER_AUTH_AGENT_CLIENT_IDS:+-e LEDGER_AUTH_AGENT_CLIENT_IDS="${LEDGER_AUTH_AGENT_CLIENT_IDS}"} \
  ${LEDGER_AUTH_SERVICE_CLIENT_IDS:+-e LEDGER_AUTH_SERVICE_CLIENT_IDS="${LEDGER_AUTH_SERVICE_CLIENT_IDS}"} \
  -e LEDGER_ALLOW_INSECURE_NON_LOOPBACK=allow-insecure-non-loopback-development-only \
  "$(docker inspect --format '{{.Image}}' "$("${COMPOSE[@]}" ps -q postgres)" >/dev/null && docker compose -p "${PROJECT}" -f compose.yaml images -q ledger 2>/dev/null || docker build -q .)" >/dev/null
trap 'docker rm -f "${PROJECT}-server" >/dev/null 2>&1 || true; cleanup' EXIT
i=0; until curl -fs -o /dev/null http://127.0.0.1:8080/ready; do i=$((i+1)); [ "$i" -lt 300 ] || { echo "FAIL: server not ready" >&2; docker logs "${PROJECT}-server" >&2; exit 1; }; sleep 0.2; done
GRAPH="live-$(date +%s)"
"${COMPOSE[@]}" run --rm migrate graph create --graph "${GRAPH}" --tenant "${LEDGER_LIVE_TENANT}" --status active --purpose 'live issuer smoke' >/dev/null
"${COMPOSE[@]}" run --rm migrate graph create --graph "${GRAPH}-other" --tenant "tenant-not-${LEDGER_LIVE_TENANT}" --status active --purpose 'foreign tenant' >/dev/null
api() { curl -s -o "${OUT}/body.json" -w '%{http_code}' -K "${TOKEN_FILE}" -H 'content-type: application/json' "$@"; }
code() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("code",""))' "${OUT}/body.json" 2>/dev/null || true; }
STATUS=PASS; note() { echo "  $1"; }; fail() { STATUS=FAIL; echo "  FAIL: $1"; }
echo "live issuer smoke against ${LEDGER_LIVE_ISSUER} (audience ${LEDGER_LIVE_AUDIENCE}), graph ${GRAPH}"
# 1. Unauthenticated and tampered tokens are refused (signature/JWKS path).
c=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:8080/v1/graphs/${GRAPH}/refs?name=main"); [ "$c" = 401 ] && note "no token -> 401" || fail "no token -> $c"
python3 - "${TOKEN_FILE}" "${OUT}/.tampered" <<'PY'
import sys, re
h = open(sys.argv[1]).read(); tok = re.search(r'Bearer (\S+)"', h).group(1)
parts = tok.split('.'); sig = parts[2]
flipped = sig[:-2] + ('A' if sig[-2] != 'A' else 'B') + sig[-1]
open(sys.argv[2], 'w').write(f'header = "authorization: Bearer {parts[0]}.{parts[1]}.{flipped}"\n')
PY
c=$(curl -s -o /dev/null -w '%{http_code}' -K "${OUT}/.tampered" "http://127.0.0.1:8080/v1/graphs/${GRAPH}/refs?name=main"); rm -f "${OUT}/.tampered"; [ "$c" = 401 ] && note "tampered signature -> 401" || fail "tampered signature -> $c"
# 2. Authenticated graph-scoped read (issuer, audience, signature, tenant mapping): an empty
#    ref is 404 NOT_FOUND *after* authentication; a foreign tenant's graph is indistinguishable.
c=$(api "http://127.0.0.1:8080/v1/graphs/${GRAPH}/refs?name=main"); [ "$c" = 404 ] && [ "$(code)" = NOT_FOUND ] && note "authenticated read of own tenant graph -> 404 NOT_FOUND (empty ref)" || fail "own graph read -> $c $(code) (401 means issuer/audience/JWKS/tenant mapping failed; 403 means the roles did not map to read)"
c=$(api "http://127.0.0.1:8080/v1/graphs/${GRAPH}-other/refs?name=main"); [ "$c" = 404 ] && note "foreign tenant graph -> 404" || fail "foreign tenant graph -> $c"
# 3. Prepare (propose capability + principal mapping recorded in the proposal row).
c=$(api -X POST -H "idempotency-key: live-$(date +%s)" --data '{"ref":"main","expected_head":null,"operations":[{"op":"add","quad":"<urn:live:s> <urn:live:p> \"1\" ."}],"activity":"live-smoke","message":"live issuer smoke","event_time":"2026-09-27T00:00:00Z","evidence_refs":["urn:evidence:live"],"source_system":"live-issuer-smoke.sh"}' "http://127.0.0.1:8080/v1/graphs/${GRAPH}/proposals")
if [ "${LEDGER_LIVE_EXPECT_PROPOSE:-0}" = 1 ]; then
  [ "$c" = 201 ] && note "prepare -> 201" || fail "prepare -> $c $(code)"
  ROW=$(docker exec "$("${COMPOSE[@]}" ps -q postgres)" psql -U ledger -d ledger -tAc "SELECT tenant_id||'|'||principal_id||'|'||principal_type FROM proposals WHERE graph_id='${GRAPH}'")
  [ "${ROW}" = "${LEDGER_LIVE_TENANT}|${LEDGER_LIVE_PRINCIPAL}|${LEDGER_LIVE_EXPECT_PRINCIPAL_TYPE:-human}" ] && note "proposal recorded tenant/principal/type = ${ROW}" || fail "proposal row ${ROW} != ${LEDGER_LIVE_TENANT}|${LEDGER_LIVE_PRINCIPAL}|${LEDGER_LIVE_EXPECT_PRINCIPAL_TYPE:-human}"
else
  [ "$c" = 403 ] && [ "$(code)" = FORBIDDEN ] && note "prepare without propose capability -> 403 FORBIDDEN" || fail "prepare -> $c $(code) (set LEDGER_LIVE_EXPECT_PROPOSE=1 if the token carries a propose role)"
fi
# 4. Acceptance is fail-closed under production auth (no unvalidated switch): 501/409, never 200.
c=$(api -X POST -H "idempotency-key: live-a-$(date +%s)" --data '{"ref":"main","expected_head":null}' "http://127.0.0.1:8080/v1/graphs/${GRAPH}/proposals/sha256:0000000000000000000000000000000000000000000000000000000000000000/accept")
[ "$c" != 200 ] && note "accept under production auth -> $c $(code) (not 200)" || fail "accept succeeded without validation"
rm -f "${OUT}/body.json"
echo "LIVE_ISSUER=${STATUS}"
[ "${STATUS}" = PASS ]
