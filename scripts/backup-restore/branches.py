#!/usr/bin/env python3
"""Branch workload for scripts/backup-restore.sh (Plan 0008 regression).

On one stress graph (tenant-stress), through the live API while the main load runs:
create four branches (one from main's root commit), land one commit on each, delete
`backup/b1`, delete and restore `backup/b2`. The restore checks then compare the branch and
lifecycle rows with live. Usage: branches.py <base-url> <graph>
"""
import base64, hashlib, hmac, json, sys, time, urllib.error, urllib.request

BASE, GRAPH = sys.argv[1], sys.argv[2]
SECRET = b"development-only-hs256-secret-not-for-production-use"


def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def token():
    now = int(time.time())
    claims = {"iss": "https://dev-issuer.example/", "aud": "api://sculpin-ledger-dev", "exp": now + 900, "nbf": now - 30,
              "tid": "tenant-stress", "oid": "backup-branches", "sculpin_principal_type": "agent",
              "roles": ["ledger.read", "ledger.propose", "ledger.review", "ledger.admin"]}
    h = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode()); p = b64(json.dumps(claims).encode())
    return f"{h}.{p}.{b64(hmac.new(SECRET, f'{h}.{p}'.encode(), hashlib.sha256).digest())}"


TOKEN = token()


def call(method, path, key=None, body=None):
    headers = {"authorization": f"Bearer {TOKEN}", "content-type": "application/json"}
    if key:
        headers["idempotency-key"] = key
    req = urllib.request.Request(BASE + path, method=method, headers=headers,
                                 data=json.dumps(body).encode() if body is not None else None)
    for _ in range(50):
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                return r.status, json.load(r)
        except urllib.error.HTTPError as e:
            payload = json.loads(e.read() or b"{}")
            if e.code == 503:  # admission control under the running load: retry the same key
                time.sleep(0.2)
                continue
            return e.code, payload
    sys.exit(f"FAIL: {method} {path} kept answering 503")


def expect(cond, what):
    if not cond:
        sys.exit(f"FAIL: {what}")


run = f"{int(time.time()):x}"
s, log = call("GET", f"/v1/graphs/{GRAPH}/branches/log?name=main&limit=1000")
expect(s == 200 and log["commits"], f"main log: {s} {log}")
root = log["commits"][-1]
heads = {}
for i in range(4):
    name = f"backup/b{i}"
    body = {"name": name, "source": "main"} | ({"from_commit": root} if i == 0 else {})
    s, c = call("POST", f"/v1/graphs/{GRAPH}/branches", f"bk-{run}-c{i}", body)
    expect(s == 201, f"create {name}: {s} {c}")
    head = c["event"]["head"]
    prep = {"ref": name, "expected_head": head, "operations": [{"op": "add", "quad": f"<urn:backup:{run}:{i}> <urn:backup:p> \"{i}\" ."}],
            "activity": "backup-branches", "event_time": "2026-09-28T12:00:00Z", "evidence_refs": ["urn:evidence:backup"],
            "source_system": "backup-restore.sh", "message": name}
    s, p = call("POST", f"/v1/graphs/{GRAPH}/proposals", f"bk-{run}-p{i}", prep)
    expect(s == 201, f"prepare {name}: {s} {p}")
    s, a = call("POST", f"/v1/graphs/{GRAPH}/proposals/{p['candidate']}/accept", f"bk-{run}-a{i}",
                {"ref": name, "expected_head": head, "reason": "backup regression"})
    expect(s == 200 and a["ref_version"] == 2, f"accept {name}: {s} {a}")
    heads[name] = a["head"]
for i in (1, 2):
    s, d = call("POST", f"/v1/graphs/{GRAPH}/branches/delete", f"bk-{run}-d{i}", {"name": f"backup/b{i}", "reason": "backup regression"})
    expect(s == 200 and d["event"]["status"] == "deleted", f"delete b{i}: {s} {d}")
s, r = call("POST", f"/v1/graphs/{GRAPH}/branches/restore", f"bk-{run}-r2", {"name": "backup/b2"})
expect(s == 200 and r["event"]["head"] == heads["backup/b2"], f"restore b2: {s} {r}")
# A merge between two of them (Phase 5): backup/b3 integrated into backup/b0.
mbody = {"source": "backup/b3", "target": "backup/b0"}
s, p = call("POST", f"/v1/graphs/{GRAPH}/merges/preview", None, mbody)
expect(s == 200 and p["classification"] == "divergent", f"merge preview: {s} {p}")
s, pr = call("POST", f"/v1/graphs/{GRAPH}/merges/propose", f"bk-{run}-mp", dict(mbody, preview_token=p["preview_token"]))
expect(s == 201, f"merge propose: {s} {pr}")
s, a = call("POST", f"/v1/graphs/{GRAPH}/merges/apply", f"bk-{run}-ma",
            {"proposal_id": pr["proposal_id"], "preview_token": p["preview_token"], "reason": "backup regression"})
expect(s == 200 and a["head"] == pr["candidate"], f"merge apply: {s} {a}")
print(f"branches on {GRAPH}: backup/b0 (from root) .. backup/b3 with one commit each; b1 deleted; b2 deleted and restored; b3 merged into b0")
