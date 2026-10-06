#!/usr/bin/env python3
"""Workload driver for scripts/upgrade-p2.sh (Plan 0006 merge gate: P1.5 -> Phase 2 upgrade).

Subcommands (all talk to the ledger at $BASE; tokens are minted with the dev-hs256 secret):
  populate <out.json>          through the OLD (P1.5) API: several graphs in two tenants,
                               accepted commits on two refs, rejected and pending proposals;
                               records every request key + body + answer and the state at
                               every accepted version.
  check <out.json> <res.json>  through the NEW API: refs, states, tenant isolation and a
                               verbatim replay of every recorded idempotency key (identical
                               answers, `replayed: true`, refs unmoved).
  phase2 <out.json> <res.json> through the NEW API: acceptance without validation is refused
                               (fail closed), a pre-upgrade pending candidate is validated,
                               the validation replays, validated acceptance succeeds, and a
                               fresh prepare -> validate -> accept works on an upgraded graph.
Every mismatch raises (non-zero exit); nothing is weakened to pass.
"""
import base64
import hashlib
import hmac
import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ["BASE"]
GRAPHS = json.loads(os.environ["GRAPHS"])  # [{"graph": ..., "tenant": ...}, ...]
EVENT_TIME = "2026-09-27T00:00:00Z"


def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def mint(tenant, roles, oid=None):
    now = int(time.time())
    claims = {"iss": os.environ["AUTH_ISSUER"], "aud": os.environ["AUTH_AUDIENCE"], "exp": now + 7200, "nbf": now - 30,
              "tid": tenant, "oid": oid or f"upgrade-agent-{tenant}", "sculpin_principal_type": "agent", "roles": roles}
    h = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    p = b64(json.dumps(claims).encode())
    sig = b64(hmac.new(os.environ["AUTH_SECRET"].encode(), f"{h}.{p}".encode(), hashlib.sha256).digest())
    return f"{h}.{p}.{sig}"


OLD_ROLES = ["ledger.read", "ledger.propose", "ledger.review"]
NEW_ROLES = OLD_ROLES + ["ledger.validate"]


def call(token, method, path, key=None, body=None):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"authorization": f"Bearer {token}", "content-type": "application/json"}
    if key:
        headers["idempotency-key"] = key
    req = urllib.request.Request(BASE + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"raw": raw.decode(errors="replace")}


def expect(cond, what):
    if not cond:
        raise SystemExit(f"FAIL: {what}")


def strip_volatile(answer):
    # correlation_id is per request by design (not part of the recorded result).
    return {k: v for k, v in answer.items() if k not in ("correlation_id", "replayed")}


def ops_for(g_index, i):
    """Deterministic but varied operations: IRIs, typed/lang literals, a named graph, deletes."""
    ops = [{"op": "add", "quad": f"<urn:up:{g_index}:s{i}> <urn:up:p> \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer> ."},
           {"op": "add", "quad": f"<urn:up:{g_index}:s{i}> <http://www.w3.org/2000/01/rdf-schema#label> \"sample {i}\"@en ."}]
    if i % 3 == 1:
        ops.append({"op": "add", "quad": f"<urn:up:{g_index}:s{i}> <urn:up:q> <urn:up:{g_index}:s{i-1}> <urn:up:named:{g_index}> ."})
    if i >= 2 and i % 2 == 0:
        ops.append({"op": "delete", "quad": f"<urn:up:{g_index}:s{i-2}> <http://www.w3.org/2000/01/rdf-schema#label> \"sample {i-2}\"@en ."})
    return ops


def prepare_body(ref, head, ops, msg):
    return {"ref": ref, "expected_head": head, "operations": ops, "activity": "upgrade-p2", "event_time": EVENT_TIME,
            "evidence_refs": [f"urn:evidence:{msg}"], "source_system": "upgrade-p2.sh", "message": msg}


def populate(out, commits):
    records, states, refs, pending = [], {}, {}, []
    for gi, g in enumerate(GRAPHS):
        graph, tenant = g["graph"], g["tenant"]
        tok = mint(tenant, OLD_ROLES)
        states[graph] = {}
        branches = [("main", commits)] + ([("dev", 3)] if gi == 0 else [])
        for ref, n in branches:
            head = None
            # Phase-4+ releases refuse genesis on a non-main ref (ADR-0022): with
            # UPGRADE_CREATE_BRANCHES=1 the ref is created from main first and builds on it.
            if ref != "main" and os.environ.get("UPGRADE_CREATE_BRANCHES") == "1":
                s, b = call(tok, "POST", f"/v1/graphs/{graph}/branches", f"b-{graph}-{ref}",
                            {"name": ref, "source": "main"})
                expect(s == 201, f"old branch create {graph}/{ref}: {s} {b}")
                head = b["event"]["head"]
            for i in range(n):
                tag = f"{graph}-{ref}-{i}"
                body = prepare_body(ref, head, ops_for(gi if ref == "main" else f"{gi}dev", i), tag)
                s, p = call(tok, "POST", f"/v1/graphs/{graph}/proposals", f"p-{tag}", body)
                expect(s == 201 and p.get("replayed") is False, f"old prepare {tag}: {s} {p}")
                records.append({"graph": graph, "tenant": tenant, "op": "prepare", "path": f"/v1/graphs/{graph}/proposals",
                                "key": f"p-{tag}", "body": body, "status": s, "answer": p})
                if i == 2 and ref == "main":
                    # A rejected proposal next to the accepted history.
                    rb = {"ref": ref, "reason": f"rejected during upgrade population {tag}"}
                    rp = prepare_body(ref, head, [{"op": "add", "quad": f"<urn:up:{gi}:rejected> <urn:up:p> \"no\" ."}], f"rej-{tag}")
                    s2, p2 = call(tok, "POST", f"/v1/graphs/{graph}/proposals", f"p-rej-{tag}", rp)
                    expect(s2 == 201, f"old prepare (to reject) {tag}: {s2} {p2}")
                    records.append({"graph": graph, "tenant": tenant, "op": "prepare", "path": f"/v1/graphs/{graph}/proposals",
                                    "key": f"p-rej-{tag}", "body": rp, "status": s2, "answer": p2})
                    path = f"/v1/graphs/{graph}/proposals/{p2['candidate']}/reject"
                    s3, r3 = call(tok, "POST", path, f"r-{tag}", rb)
                    expect(s3 == 200 and r3.get("replayed") is False, f"old reject {tag}: {s3} {r3}")
                    records.append({"graph": graph, "tenant": tenant, "op": "reject", "path": path, "key": f"r-{tag}",
                                    "body": rb, "status": s3, "answer": r3})
                ab = {"ref": ref, "expected_head": head, "reason": f"accept {tag}"}
                path = f"/v1/graphs/{graph}/proposals/{p['candidate']}/accept"
                s, a = call(tok, "POST", path, f"a-{tag}", ab)
                expect(s == 200 and a.get("replayed") is False, f"old accept {tag}: {s} {a}")
                records.append({"graph": graph, "tenant": tenant, "op": "accept", "path": path, "key": f"a-{tag}",
                                "body": ab, "status": s, "answer": a})
                head = a["head"]
                s, st = call(tok, "GET", f"/v1/graphs/{graph}/commits/{head}/state")
                expect(s == 200, f"old state {tag}: {s} {st}")
                states[graph][head] = {"ref": ref, "version": a["ref_version"], "quads": sorted(st["quads"])}
            s, r = call(tok, "GET", f"/v1/graphs/{graph}/refs?name={ref}")
            expect(s == 200 and r["head"] == head, f"old ref {graph}/{ref}: {s} {r}")
            refs[f"{graph}|{ref}"] = r
        # A pending (prepared, never decided) proposal on top of main: validated and accepted
        # after the upgrade for the first graph, left pending for the others.
        main_head = refs[f"{graph}|main"]["head"]
        tag = f"{graph}-pending"
        body = prepare_body("main", main_head, [{"op": "add", "quad": f"<urn:up:{gi}:pending> <urn:up:p> \"pending\" ."}], tag)
        s, p = call(tok, "POST", f"/v1/graphs/{graph}/proposals", f"p-{tag}", body)
        expect(s == 201, f"old prepare pending {tag}: {s} {p}")
        records.append({"graph": graph, "tenant": tenant, "op": "prepare", "path": f"/v1/graphs/{graph}/proposals",
                        "key": f"p-{tag}", "body": body, "status": s, "answer": p})
        pending.append({"graph": graph, "tenant": tenant, "candidate": p["candidate"], "expected_head": main_head,
                        "proposal_id": p["proposal_id"]})
    # Tenant isolation as the old release answered it (compared verbatim after the upgrade).
    other = [g for g in GRAPHS if g["tenant"] != GRAPHS[0]["tenant"]][0]
    s, iso = call(mint(other["tenant"], OLD_ROLES), "GET", f"/v1/graphs/{GRAPHS[0]['graph']}/refs?name=main")
    isolation = {"status": s, "code": iso.get("code") if isinstance(iso, dict) else None}
    expect(s in (403, 404), f"cross-tenant read was not refused by the old release: {s} {iso}")
    json.dump({"records": records, "states": states, "refs": refs, "pending": pending, "isolation": isolation},
              open(out, "w"), indent=1, sort_keys=True)
    n_states = sum(len(v) for v in states.values())
    kinds = {k: sum(1 for r in records if r["op"] == k) for k in ("prepare", "accept", "reject")}
    print(f"previous release populated: {len(GRAPHS)} graphs / {len({g['tenant'] for g in GRAPHS})} tenants, "
          f"{len(refs)} refs, {kinds['accept']} accepted, {kinds['reject']} rejected, {len(pending)} pending, "
          f"{n_states} recorded states, {len(records)} idempotency keys")


def check(out, res):
    old = json.load(open(out))
    toks = {g["tenant"]: mint(g["tenant"], NEW_ROLES) for g in GRAPHS}
    for name, r in old["refs"].items():
        graph, ref = name.split("|")
        tenant = [g["tenant"] for g in GRAPHS if g["graph"] == graph][0]
        s, now = call(toks[tenant], "GET", f"/v1/graphs/{graph}/refs?name={ref}")
        expect(s == 200 and now == r, f"ref {name} differs after upgrade: {now} vs {r}")
    n = 0
    for graph, by_head in old["states"].items():
        tenant = [g["tenant"] for g in GRAPHS if g["graph"] == graph][0]
        for head, st in by_head.items():
            s, now = call(toks[tenant], "GET", f"/v1/graphs/{graph}/commits/{head}/state")
            expect(s == 200 and now["commit"] == head, f"state {graph}@{head}: {s} {now}")
            expect(sorted(now["quads"]) == st["quads"], f"state of {graph}@{head} (v{st['version']}) differs after upgrade")
            n += 1
    other = [g for g in GRAPHS if g["tenant"] != GRAPHS[0]["tenant"]][0]
    s, iso = call(toks[other["tenant"]], "GET", f"/v1/graphs/{GRAPHS[0]['graph']}/refs?name=main")
    expect({"status": s, "code": iso.get("code")} == old["isolation"], f"tenant isolation answer changed: {s} {iso} vs {old['isolation']}")
    replayed = {"prepare": 0, "accept": 0, "reject": 0}
    for r in old["records"]:
        s, a = call(toks[r["tenant"]], "POST", r["path"], r["key"], r["body"])
        expect(s == 200, f"replay {r['op']} {r['key']}: HTTP {s} {a}")
        expect(a.get("replayed") is True, f"replay {r['op']} {r['key']} not marked replayed: {a}")
        expect(strip_volatile(a) == strip_volatile(r["answer"]),
               f"replay {r['op']} {r['key']} answered differently:\n new {strip_volatile(a)}\n old {strip_volatile(r['answer'])}")
        replayed[r["op"]] += 1
    for name, r in old["refs"].items():
        graph, ref = name.split("|")
        tenant = [g["tenant"] for g in GRAPHS if g["graph"] == graph][0]
        s, now = call(toks[tenant], "GET", f"/v1/graphs/{graph}/refs?name={ref}")
        expect(s == 200 and now == r, f"replays moved ref {name}: {now} vs {r}")
    json.dump({"states_checked": n, "refs_checked": len(old["refs"]), "replayed": replayed}, open(res, "w"), sort_keys=True)
    print(f"after upgrade: {len(old['refs'])} refs identical, {n} historical states identical, tenant isolation "
          f"answer identical ({old['isolation']}), replayed identically: {replayed} (refs unmoved)")


def phase2(out, res):
    old = json.load(open(out))
    result = {}
    requested = {"base_kb": {"kb_id": "urn:upgrade:kb", "revision": "kbrev-upgrade-1"},
                 "shapes": {"id": "urn:upgrade:shapes", "version": "1+shapes_hash:upgrade"}}
    # 1. The pre-upgrade pending candidate of the first graph.
    pend = old["pending"][0]
    graph, tenant, cand = pend["graph"], pend["tenant"], pend["candidate"]
    tok = mint(tenant, NEW_ROLES)
    base = f"/v1/graphs/{graph}/proposals/{cand}"
    _, ref_before = call(tok, "GET", f"/v1/graphs/{graph}/refs?name=main")
    s, a = call(tok, "POST", f"{base}/accept", "p2-accept-unvalidated", {"ref": "main", "expected_head": pend["expected_head"], "reason": "no validation"})
    expect(s == 409 and a.get("code") == "VALIDATION_REQUIRED",
           f"acceptance WITHOUT validation must be 409 VALIDATION_REQUIRED on the Phase-2 server: {s} {a}")
    _, ref_after = call(tok, "GET", f"/v1/graphs/{graph}/refs?name=main")
    expect(ref_after == ref_before, f"refused unvalidated acceptance moved the ref: {ref_before} -> {ref_after}")
    result["unvalidated_accept_refused"] = {"status": s, "code": a.get("code")}
    s, v = call(tok, "POST", f"{base}/validations", "p2-validate-pending", {"requested": requested})
    expect(s == 201 and v.get("replayed") is False and v["conforms"] is True, f"validate pre-upgrade candidate: {s} {v}")
    s2, v2 = call(tok, "POST", f"{base}/validations", "p2-validate-pending", {"requested": requested})
    expect(s2 == 200 and v2.get("replayed") is True and v2["validation_id"] == v["validation_id"]
           and v2["semantic_environment_id"] == v["semantic_environment_id"], f"validation replay: {s2} {v2}")
    s3, v3 = call(tok, "GET", f"{base}/validations/{v['validation_id']}")
    expect(s3 == 200 and v3["validation_id"] == v["validation_id"] and v3["record"] == v["record"], f"validation read: {s3} {v3}")
    other = [g for g in GRAPHS if g["tenant"] != tenant][0]
    s4, v4 = call(mint(other["tenant"], NEW_ROLES), "GET", f"{base}/validations/{v['validation_id']}")
    expect(s4 in (403, 404), f"cross-tenant validation read not refused: {s4} {v4}")
    ab = {"ref": "main", "expected_head": pend["expected_head"], "reason": "validated after upgrade",
          "validation_id": v["validation_id"], "semantic_environment_id": v["semantic_environment_id"]}
    s, acc = call(tok, "POST", f"{base}/accept", "p2-accept-pending", ab)
    expect(s == 200 and acc.get("replayed") is False and acc["head"] == cand, f"validated acceptance of pre-upgrade candidate: {s} {acc}")
    old_version = old["refs"][f"{graph}|main"]["version"]
    expect(acc["ref_version"] == old_version + 1, f"ref version after validated accept {acc['ref_version']} != {old_version}+1")
    s, acc2 = call(tok, "POST", f"{base}/accept", "p2-accept-pending", ab)
    expect(s == 200 and acc2.get("replayed") is True and strip_volatile(acc2) == strip_volatile(acc), f"validated accept replay: {s} {acc2}")
    result["pending"] = {"graph": graph, "validation_id": v["validation_id"], "environment_id": v["semantic_environment_id"],
                         "decision_id": acc["decision_id"], "ref_version": acc["ref_version"]}
    # 2. A fresh prepare -> validate -> accept on another upgraded graph (other tenant).
    g2 = [g for g in GRAPHS if g["tenant"] != tenant][0]
    tok2 = mint(g2["tenant"], NEW_ROLES)
    ref2 = old["refs"][f"{g2['graph']}|main"]
    # Its pre-upgrade pending proposal is superseded or conflicts only if we move main; we do.
    body = prepare_body("main", ref2["head"], [{"op": "add", "quad": "<urn:up:after> <urn:up:p> \"after upgrade\" ."}], "after-upgrade")
    s, p = call(tok2, "POST", f"/v1/graphs/{g2['graph']}/proposals", "p2-prepare-new", body)
    expect(s == 201, f"prepare after upgrade: {s} {p}")
    b2 = f"/v1/graphs/{g2['graph']}/proposals/{p['candidate']}"
    s, v = call(tok2, "POST", f"{b2}/validations", "p2-validate-new", {"requested": requested})
    expect(s == 201 and v["conforms"] is True, f"validate new candidate: {s} {v}")
    s, acc = call(tok2, "POST", f"{b2}/accept", "p2-accept-new",
                  {"ref": "main", "expected_head": ref2["head"], "reason": "validated new",
                   "validation_id": v["validation_id"], "semantic_environment_id": v["semantic_environment_id"]})
    expect(s == 200 and acc["ref_version"] == ref2["version"] + 1, f"validated acceptance of new candidate: {s} {acc}")
    s, st = call(tok2, "GET", f"/v1/graphs/{g2['graph']}/commits/{acc['head']}/state")
    prev = sorted(old["states"][g2["graph"]][ref2["head"]]["quads"])
    expect(s == 200 and sorted(st["quads"]) == sorted(prev + ['<urn:up:after> <urn:up:p> "after upgrade" .']),
           f"state after new validated commit is not old state + 1 quad: {s}")
    result["new"] = {"graph": g2["graph"], "validation_id": v["validation_id"], "decision_id": acc["decision_id"], "ref_version": acc["ref_version"]}
    json.dump(result, open(res, "w"), indent=1, sort_keys=True)
    print(f"phase 2 on upgraded graphs: unvalidated accept refused ({result['unvalidated_accept_refused']}); pre-upgrade candidate "
          f"validated ({result['pending']['validation_id'][:19]}…, replay identical) and accepted as v{result['pending']['ref_version']}; "
          f"fresh prepare->validate->accept on {g2['graph']} accepted as v{result['new']['ref_version']}")


if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "populate":
        populate(sys.argv[2], int(sys.argv[3]))
    elif cmd == "check":
        check(sys.argv[2], sys.argv[3])
    elif cmd == "phase2":
        phase2(sys.argv[2], sys.argv[3])
    else:
        raise SystemExit(f"unknown subcommand {cmd}")
