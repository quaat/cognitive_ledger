#!/usr/bin/env python3
"""Workload driver for scripts/upgrade-p3.sh (Plan 0007 gate: Phase 2 (0010) -> Phase 3 (0011)).

Reuses scripts/upgrade-p2/workload.py (its graphs in $GRAPHS carry named-graph quads on
`main`, which projection v1 refuses visibly) and adds default-graph-only graphs ($PGRAPHS)
whose whole accepted history must be projected. Every POST with an idempotency key made
through this driver is recorded (tenant, path, key, body, answer) for a verbatim replay after
the upgrade.

Subcommands (all talk to the ledger at $BASE; tokens are minted with the dev-hs256 secret):
  populate-default <p3.json> <rec.json> [commits]  OLD API, unvalidated-acceptance switch: history on the
                                         projectable graphs (typed/lang literals, IRIs,
                                         deletes) and one pending proposal.
  phase2-old <p2.json> <p3.json> <rec.json> <res.json>
                                         OLD API, validator configured: the p2 Phase-2 flow
                                         (validations, validated acceptance) plus validation
                                         and validated acceptance of the pending proposal
                                         of a projectable graph.
  replay <rec.json>                      NEW API: every recorded key answers identically,
                                         `replayed: true`, refs unmoved.
  projection <p3.json> <query-url>       the target holds exactly the ledger's current `main`
                                         state of every projectable graph, marker = ref head;
                                         the named-graph graphs' cognitive graphs are empty.
  live <p3.json> <res.json>              NEW API: prepare -> validate -> accept on a
                                         projectable graph after the upgrade.
Every mismatch raises (non-zero exit); nothing is weakened to pass.
"""
import base64
import importlib.util
import json
import os
import sys
import urllib.parse
import urllib.request

_spec = importlib.util.spec_from_file_location(
    "p2", os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "upgrade-p2", "workload.py"))
p2 = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(p2)

PGRAPHS = json.loads(os.environ["PGRAPHS"])  # [{"graph", "tenant", "kb"}, ...]
REQUESTED = {"base_kb": {"kb_id": "urn:upgrade:kb", "revision": "kbrev-upgrade-1"},
             "shapes": {"id": "urn:upgrade:shapes", "version": "1+shapes_hash:upgrade"}}
MARKERS = "urn:sculpin:ledger-projection:v1:markers"
LP = "urn:sculpin:ledger-projection:v1#"
RECORDED = []
_call = p2.call


def _tenant_of(token):
    payload = token.split(".")[1]
    return json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))["tid"]


def recording_call(token, method, path, key=None, body=None):
    s, a = _call(token, method, path, key, body)
    if method == "POST" and key and 200 <= s < 300:
        RECORDED.append({"tenant": _tenant_of(token), "path": path, "key": key, "body": body, "status": s, "answer": a})
    return s, a


p2.call = recording_call  # p2.phase2 records through this too
call, mint, expect, strip_volatile = recording_call, p2.mint, p2.expect, p2.strip_volatile


def cognitive_graph(kb):
    keep = set(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")
    return "urn:sculpin:kb:" + "".join(chr(b) if b in keep else "%%%02X" % b for b in kb.encode()) + ":cognitive"


def default_ops(g, i):
    """Default graph only: canonical typed and language literals, IRI objects, deletes."""
    ops = [{"op": "add", "quad": f"<urn:up3:{g}:s{i}> <urn:up3:weight> \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer> ."},
           {"op": "add", "quad": f"<urn:up3:{g}:s{i}> <http://www.w3.org/2000/01/rdf-schema#label> \"sample {i}\"@en ."}]
    if i >= 1:
        ops.append({"op": "add", "quad": f"<urn:up3:{g}:s{i}> <urn:up3:next> <urn:up3:{g}:s{i-1}> ."})
    if i >= 2 and i % 2 == 0:
        ops.append({"op": "delete", "quad": f"<urn:up3:{g}:s{i-2}> <http://www.w3.org/2000/01/rdf-schema#label> \"sample {i-2}\"@en ."})
    return ops


def dump_recorded(rec):
    old = json.load(open(rec)) if os.path.exists(rec) else []
    json.dump(old + RECORDED, open(rec, "w"), indent=1, sort_keys=True)


def populate_default(out, rec, commits="5"):
    commits = int(commits)
    result = {"graphs": {}, "pending": []}
    for gi, g in enumerate(PGRAPHS):
        graph, tenant = g["graph"], g["tenant"]
        tok = mint(tenant, p2.OLD_ROLES)
        head, states = None, {}
        for i in range(commits):
            tag = f"{graph}-main-{i}"
            s, p = call(tok, "POST", f"/v1/graphs/{graph}/proposals", f"p3-p-{tag}", p2.prepare_body("main", head, default_ops(gi, i), tag))
            expect(s == 201, f"old prepare {tag}: {s} {p}")
            s, a = call(tok, "POST", f"/v1/graphs/{graph}/proposals/{p['candidate']}/accept", f"p3-a-{tag}",
                        {"ref": "main", "expected_head": head, "reason": f"accept {tag}"})
            expect(s == 200 and a["ref_version"] == i + 1, f"old accept {tag}: {s} {a}")
            head = a["head"]
            s, st = call(tok, "GET", f"/v1/graphs/{graph}/commits/{head}/state")
            expect(s == 200, f"old state {tag}: {s}")
            states[head] = {"version": a["ref_version"], "quads": sorted(st["quads"])}
        result["graphs"][graph] = {"tenant": tenant, "kb": g["kb"], "head": head, "version": commits, "states": states}
    g0 = PGRAPHS[0]
    tok = mint(g0["tenant"], p2.OLD_ROLES)
    head = result["graphs"][g0["graph"]]["head"]
    body = p2.prepare_body("main", head, [{"op": "add", "quad": "<urn:up3:pending> <urn:up3:p> \"validated before the upgrade\" ."}], "p3-pending")
    s, p = call(tok, "POST", f"/v1/graphs/{g0['graph']}/proposals", "p3-p-pending", body)
    expect(s == 201, f"old prepare pending: {s} {p}")
    result["pending"].append({"graph": g0["graph"], "tenant": g0["tenant"], "candidate": p["candidate"], "expected_head": head})
    json.dump(result, open(out, "w"), indent=1, sort_keys=True)
    dump_recorded(rec)
    print(f"projectable graphs populated through the previous API: {len(PGRAPHS)} graphs x {commits} accepted "
          f"commits (default graph only), 1 pending proposal; {len(RECORDED)} keys recorded")


def phase2_old(p2_out, out, rec, res):
    p2.phase2(p2_out, res)  # validations + validated acceptance on the p2 graphs (old release)
    # p2's `check` compares refs with the recorded ones: record where the Phase-2 flow moved them.
    old = json.load(open(p2_out))
    for name in old["refs"]:
        graph, ref = name.split("|")
        tenant = [g["tenant"] for g in p2.GRAPHS if g["graph"] == graph][0]
        s, now = _call(mint(tenant, p2.NEW_ROLES), "GET", f"/v1/graphs/{graph}/refs?name={ref}")
        expect(s == 200, f"ref {name}: {s} {now}")
        old["refs"][name] = now
    json.dump(old, open(p2_out, "w"), indent=1, sort_keys=True)
    p3 = json.load(open(out))
    pend = p3["pending"][0]
    tok = mint(pend["tenant"], p2.NEW_ROLES)
    base = f"/v1/graphs/{pend['graph']}/proposals/{pend['candidate']}"
    s, v = call(tok, "POST", f"{base}/validations", "p3-validate-pending", {"requested": REQUESTED})
    expect(s == 201 and v["conforms"] is True, f"old validate: {s} {v}")
    s, a = call(tok, "POST", f"{base}/accept", "p3-accept-pending",
                {"ref": "main", "expected_head": pend["expected_head"], "reason": "validated before the upgrade",
                 "validation_id": v["validation_id"], "semantic_environment_id": v["semantic_environment_id"]})
    g = p3["graphs"][pend["graph"]]
    expect(s == 200 and a["ref_version"] == g["version"] + 1, f"old validated accept: {s} {a}")
    g["head"], g["version"] = a["head"], a["ref_version"]
    json.dump(p3, open(out, "w"), indent=1, sort_keys=True)
    dump_recorded(rec)
    print(f"previous release (validator configured): Phase-2 flow on the p2 graphs; {pend['graph']} validated and "
          f"accepted as v{a['ref_version']}; {len(RECORDED)} keys recorded")


def replay(rec):
    records = json.load(open(rec))
    toks = {}
    refs_before = {}
    for g in PGRAPHS + p2.GRAPHS:
        toks[g["tenant"]] = toks.get(g["tenant"]) or mint(g["tenant"], p2.NEW_ROLES)
        s, r = _call(toks[g["tenant"]], "GET", f"/v1/graphs/{g['graph']}/refs?name=main")
        refs_before[g["graph"]] = r
    for r in records:
        s, a = _call(toks[r["tenant"]], "POST", r["path"], r["key"], r["body"])
        expect(s == 200 and a.get("replayed") is True, f"replay {r['key']}: {s} {a}")
        expect(strip_volatile(a) == strip_volatile(r["answer"]),
               f"replay {r['key']} answered differently:\n new {strip_volatile(a)}\n old {strip_volatile(r['answer'])}")
    for g in PGRAPHS + p2.GRAPHS:
        s, now = _call(toks[g["tenant"]], "GET", f"/v1/graphs/{g['graph']}/refs?name=main")
        expect(now == refs_before[g["graph"]], f"replays moved {g['graph']}/main")
    print(f"after upgrade: {len(records)} recorded keys (populate-default, Phase-2 validations and validated "
          f"acceptances on the old release) replayed identically; refs unmoved")


def sparql(url, query):
    req = urllib.request.Request(url, data=query.encode(), method="POST",
                                 headers={"content-type": "application/sparql-query", "accept": "application/sparql-results+json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)["results"]["bindings"]


def term(b):
    if b["type"] == "uri":
        return f"<{b['value']}>"
    v = b["value"].replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\r", "\\r")
    if "xml:lang" in b:
        return f"\"{v}\"@{b['xml:lang']}"
    dt = b.get("datatype")
    return f"\"{v}\"" if dt in (None, "http://www.w3.org/2001/XMLSchema#string") else f"\"{v}\"^^<{dt}>"


def projection(out, url):
    p3 = json.load(open(out))
    checked = []
    for graph, g in p3["graphs"].items():
        tok = mint(g["tenant"], p2.NEW_ROLES)
        s, ref = _call(tok, "GET", f"/v1/graphs/{graph}/refs?name=main")
        expect(s == 200, f"ref {graph}: {s}")
        s, st = _call(tok, "GET", f"/v1/graphs/{graph}/commits/{ref['head']}/state")
        expect(s == 200, f"state {graph}: {s}")
        cg = cognitive_graph(g["kb"])
        rows = sparql(url, f"SELECT ?s ?p ?o WHERE {{ GRAPH <{cg}> {{ ?s ?p ?o }} }}")
        target = sorted(f"{term(r['s'])} {term(r['p'])} {term(r['o'])} ." for r in rows)
        expect(target == sorted(st["quads"]), f"{graph}: target graph <{cg}> is not exactly the accepted state at "
               f"{ref['head']} (target {len(target)} vs ledger {len(st['quads'])} triples)")
        marker = {}
        for r in sparql(url, f"SELECT ?p ?o WHERE {{ GRAPH <{MARKERS}> {{ <{cg}> ?p ?o }} }}"):
            marker.setdefault(r["p"]["value"].replace(LP, ""), []).append(r["o"]["value"])
        expect(marker.get("graphId") == [graph] and marker.get("branch") == ["main"]
               and marker.get("commitId") == [ref["head"]] and marker.get("refVersion") == [str(ref["version"])]
               and marker.get("tripleCount") == [str(len(target))] and len(marker) == 7,
               f"{graph}: marker {marker} does not name the ref head {ref['head']} v{ref['version']}")
        checked.append(f"{graph}@v{ref['version']}={len(target)} triples")
    for g in p2.GRAPHS:  # named-graph state: refused visibly, nothing written
        cg = cognitive_graph(g["kb"])
        expect(not sparql(url, f"SELECT * WHERE {{ GRAPH <{cg}> {{ ?s ?p ?o }} }} LIMIT 1"), f"{g['graph']}: named-graph state was written")
        expect(not sparql(url, f"SELECT * WHERE {{ GRAPH <{MARKERS}> {{ <{cg}> ?p ?o }} }} LIMIT 1"), f"{g['graph']}: marker written")
    print("target holds exactly the accepted main state (marker = ref head): " + ", ".join(checked)
          + f"; {len(p2.GRAPHS)} named-graph streams wrote nothing")


def live(out, res):
    p3 = json.load(open(out))
    graph, g = sorted(p3["graphs"].items())[-1]
    tok = mint(g["tenant"], p2.NEW_ROLES)
    s, ref = _call(tok, "GET", f"/v1/graphs/{graph}/refs?name=main")
    body = p2.prepare_body("main", ref["head"], [{"op": "add", "quad": "<urn:up3:after> <urn:up3:p> \"after the upgrade\" ."}], "p3-live")
    s, p = _call(tok, "POST", f"/v1/graphs/{graph}/proposals", "p3-live-prepare", body)
    expect(s == 201, f"prepare after upgrade: {s} {p}")
    base = f"/v1/graphs/{graph}/proposals/{p['candidate']}"
    s, v = _call(tok, "POST", f"{base}/validations", "p3-live-validate", {"requested": REQUESTED})
    expect(s == 201 and v["conforms"] is True, f"validate after upgrade: {s} {v}")
    s, a = _call(tok, "POST", f"{base}/accept", "p3-live-accept",
                 {"ref": "main", "expected_head": ref["head"], "reason": "after the upgrade",
                  "validation_id": v["validation_id"], "semantic_environment_id": v["semantic_environment_id"]})
    expect(s == 200 and a["ref_version"] == ref["version"] + 1, f"validated accept after upgrade: {s} {a}")
    json.dump({"graph": graph, "head": a["head"], "version": a["ref_version"]}, open(res, "w"))
    print(f"after upgrade: {graph} prepare -> validate -> accept as v{a['ref_version']}")


if __name__ == "__main__":
    cmd, args = sys.argv[1], sys.argv[2:]
    {"populate-default": populate_default, "phase2-old": phase2_old, "replay": replay,
     "projection": projection, "live": live}.get(cmd, lambda *_: sys.exit(f"unknown subcommand {cmd}"))(*args)
